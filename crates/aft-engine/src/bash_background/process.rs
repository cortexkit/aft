#[cfg(unix)]
use std::path::Path;


/// The Unix payload wrapper runs the user's command in the same shell that
/// owns the pipeline. Bash exposes per-segment statuses as `PIPESTATUS`, while
/// zsh exposes them as `pipestatus`; plain POSIX sh has neither and therefore
/// receives the original command without a capture suffix. The status stream
/// is published in the task's already-writable io directory only after the
/// pipeline finishes. Temporarily closing an inherited capture descriptor is
/// insufficient: some shells pass their saved redirection copy to pipeline
/// children. Exit/failure markers belong only to the outer wrapper. Windows
/// shells have no portable per-segment status equivalent.
#[cfg(unix)]
// POSIX command exits cannot produce this signed marker. Windows uses the full
// 32-bit exit range, so neither this gate nor its marker interpretation is used there.
pub(crate) const BOOTSTRAP_ABORTED_MARKER: i32 = i32::MIN;

// Use shell builtins before user code, keeping the posix_spawn fast path. A
// subshell here would adjust only itself, not the shell that launches the command.
#[cfg(target_os = "linux")]
macro_rules! shell_oom_priority {
    ($init:literal) => {
        concat!(
            $init,
            r#"if [ -n "$__aft_oom_failure" ]; then
  { printf '%s' 1000 > /proc/self/oom_score_adj; } 2>/dev/null || :
  __aft_oom_value=unreadable
  { IFS= read -r __aft_oom_value < /proc/self/oom_score_adj; } 2>/dev/null || :
  if [ "$__aft_oom_value" != 1000 ]; then
    { printf '%s\n' "$__aft_oom_value" > "$__aft_oom_failure"; } 2>/dev/null || :
  fi
fi
unset __aft_oom_failure __aft_oom_value
"#
        )
    };
}
#[cfg(all(unix, not(target_os = "linux")))]
macro_rules! shell_oom_priority {
    ($init:literal) => {
        ""
    };
}
#[cfg(target_os = "linux")]
pub(crate) use shell_oom_priority;

#[cfg(unix)]
pub(crate) const PAYLOAD_WRAPPER: &[u8] = concat!(
    r#"#!/bin/sh
shell=$1
command=$2
exit_fd=$3
pipeline_status_path=$4
pipeline_shell=$5
"#,
    shell_oom_priority!("__aft_oom_failure=${7-}\n"),
    r#"if [ "${6-}" = "wait-for-identity" ]; then
  if ! IFS= read -r __aft_start || [ "$__aft_start" != "run" ]; then
    printf '%s' '-2147483648' >&"$exit_fd"
    exit 125
  fi
  exec </dev/null
fi
case "$pipeline_status_path:$pipeline_shell" in
  ?*:bash)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${PIPESTATUS[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  ?*:zsh)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${pipestatus[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  *)
    # POSIX sh has no per-segment pipeline status array; preserve the original
    # invocation instead of changing its semantics with a best-effort guess.
    "$shell" -c "$command" 3>&- 4>&-
    ;;
esac
code=$?
printf "%s" "$code" >&"$exit_fd"
exit "$code"
"#
)
.as_bytes();

#[cfg(unix)]
pub(crate) fn pipeline_shell_kind(shell: &Path) -> Option<&'static str> {
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("bash") => Some("bash"),
        Some("zsh") => Some("zsh"),
        _ => None,
    }
}

/// Start a non-PTY child in a new session before exec. The bridge can inherit a
/// harness's controlling terminal even though the child uses pipe-backed stdio;
/// an interactive shell could otherwise perform job control against that terminal.
#[cfg(unix)]
pub(crate) fn start_new_session(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn oom_priority_marker(scratch: Option<&Path>, io_dir: &Path) -> std::path::PathBuf {
    scratch.unwrap_or(io_dir).join("oom-priority-failure")
}

#[cfg(target_os = "linux")]
static OOM_PRIORITY_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn oom_priority_failures() -> u64 {
    OOM_PRIORITY_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_oom_priority_failure(marker: &Path) {
    let Some(bytes) = crate::file_kind::read_head(marker, 16) else {
        return;
    };
    // Removing the marker claims it even when terminal publication is retried.
    if std::fs::remove_file(marker).is_err() {
        return;
    }
    let value = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| (-1000..=1000).contains(value));
    if OOM_PRIORITY_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
        crate::slog_warn!(
            "Linux agent shell OOM priority could not be raised to 1000; observed oom_score_adj={}",
            value
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unreadable".into())
        );
    }
}


#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    fn oom_priority_wrapper_output(
        wrapper: &str,
        shell: &str,
        command: &str,
        marker: &Path,
        enabled: bool,
    ) -> std::process::Output {
        // The exit marker normally uses fd 3; send it to a separate file here.
        let script = format!("exec 3>\"$8\"\n{wrapper}");
        std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &script,
                "aft-wrapper-test",
                shell,
                command,
                "3",
                "",
                "",
                "",
            ])
            .arg(if enabled { marker } else { Path::new("") })
            .arg(marker.with_extension("exit"))
            .output()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_oom_priority_wrapper_preserves_output_exit_and_pipeline_status() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("failure");
        let wrapper = std::str::from_utf8(PAYLOAD_WRAPPER).unwrap();
        // Omitting the marker disables only the OOM step, not the payload wrapper.
        for shell in ["/bin/sh", "/bin/bash"] {
            let command = "printf 'stdout\\000bytes\\n'; printf 'stderr\\n' >&2; exit 37";
            let before = oom_priority_wrapper_output(wrapper, shell, command, &marker, false);
            let after = oom_priority_wrapper_output(wrapper, shell, command, &marker, true);
            assert_eq!(after.stdout, b"stdout\0bytes\n");
            assert_eq!(before.stdout, after.stdout);
            assert_eq!(before.stderr, after.stderr);
            assert_eq!(before.status.code(), Some(37));
            assert_eq!(before.status, after.status);
        }
        let status = root.path().join("pipeline");
        let script = format!("exec 3>\"$8\"\n{wrapper}");
        let result = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &script,
                "aft-wrapper-test",
                "/bin/bash",
                "false | true",
                "3",
            ])
            .arg(&status)
            .args(["bash", ""])
            .arg(&marker)
            .arg(root.path().join("exit"))
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result.stderr);
        assert_eq!(std::fs::read_to_string(status).unwrap(), "1\n0\n");
        assert!(
            !marker.exists(),
            "successful adjustment must not write a failure marker"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_oom_priority_write_failure_runs_command_and_warns_once() {
        // The warning budget is process-wide; unrelated parallel tests must not
        // consume it before this forced-failure scenario runs.
        const CHILD: &str = "AFT_TEST_OOM_PRIORITY_FAILURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bash_background::process::tests::linux_oom_priority_write_failure_runs_command_and_warns_once",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let readback = root.path().join("readback");
        std::fs::write(&readback, "17\n").unwrap();
        let wrapper = std::str::from_utf8(PAYLOAD_WRAPPER)
            .unwrap()
            .replacen("/proc/self/oom_score_adj", "/dev/full", 1)
            .replace("/proc/self/oom_score_adj", readback.to_str().unwrap());
        assert_eq!(oom_priority_failures(), 0);
        let (_, logs) = crate::logging::capture_log_lines(|| {
            for i in 0..2 {
                let marker = root.path().join(format!("failure-{i}"));
                let result = oom_priority_wrapper_output(
                    &wrapper,
                    "/bin/sh",
                    "printf ran; exit 23",
                    &marker,
                    true,
                );
                assert_eq!(result.stdout, b"ran");
                assert_eq!(result.stderr, b"");
                assert_eq!(result.status.code(), Some(23));
                assert_eq!(std::fs::read_to_string(&marker).unwrap(), "17\n");
                observe_oom_priority_failure(&marker);
                observe_oom_priority_failure(&marker);
            }
        });
        assert_eq!(oom_priority_failures(), 2);
        let warnings: Vec<_> = logs
            .iter()
            .filter(|line| line.contains("Linux agent shell OOM priority"))
            .collect();
        assert_eq!(warnings.len(), 1, "{logs:?}");
        assert!(warnings[0].contains("WARN"));
        assert!(warnings[0].contains("oom_score_adj=17"));
    }

    #[cfg(unix)]
    use super::*;


    #[cfg(unix)]
    #[test]
    fn payload_wrapper_captures_bash_and_zsh_pipeline_statuses() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        use std::process::Command;

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }

        for (shell, kind) in shells {
            let temp = tempfile::tempdir().expect("create wrapper test directory");
            let exit_path = temp.path().join("exit");
            let status_path = temp.path().join("pipeline ' status");
            let exit_file = std::fs::File::create(&exit_path).expect("create exit marker");
            let status_file = std::fs::File::create(&status_path).expect("create status file");
            let original_status_inode = status_file.metadata().unwrap().ino();
            let failure_file = std::fs::File::create(temp.path().join("failure")).unwrap();
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                std::str::from_utf8(PAYLOAD_WRAPPER).expect("wrapper is UTF-8"),
                "aft-payload-wrapper",
                shell,
                "false | true",
                "3",
                status_path.to_str().unwrap(),
                kind,
            ]);
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit_file.as_raw_fd(),
                failure_file.as_raw_fd(),
            )
            .unwrap();
            let result = command.status().expect("run payload wrapper");
            drop(exit_file);
            drop(status_file);
            assert!(result.success(), "wrapper failed for {kind}");
            assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "1\n0\n");
            assert_ne!(
                std::fs::metadata(&status_path).unwrap().ino(),
                original_status_inode,
                "status must be published through an atomic rename"
            );
            assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_bootstrap_eof_and_wrong_release_never_execute_command() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};
        for release in [None, Some(b"not-run\n".as_slice())] {
            let temp = tempfile::tempdir().unwrap();
            let exit_path = temp.path().join("exit");
            let exit = std::fs::File::create(&exit_path).unwrap();
            let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
            let marker = temp.path().join("must-not-run");
            let payload = format!("printf ran > '{}'", marker.display());
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    "/bin/sh",
                    &payload,
                    "3",
                    "",
                    "",
                    "wait-for-identity",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit.as_raw_fd(),
                failure.as_raw_fd(),
            )
            .unwrap();
            let mut child = command.spawn().unwrap();
            let mut input = child.stdin.take().unwrap();
            if let Some(release) = release {
                input.write_all(release).unwrap();
            }
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(125));
            assert!(
                !marker.exists(),
                "bootstrap must not run user code without release"
            );
            assert_eq!(std::fs::read_to_string(exit_path).unwrap(), "-2147483648");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_keeps_control_fds_out_of_user_commands() {
        use std::os::fd::AsRawFd;
        use std::process::Command;

        let mut shells = vec![("/bin/sh", ""), ("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for pipeline in [false, true] {
                if pipeline && kind.is_empty() {
                    continue;
                }
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let _status = std::fs::File::create(&status_path).unwrap();
                let probe = temp.path().join("probe");
                // A redirection probe would create Bash's own saved stderr fd
                // and falsely report it as an inherited control descriptor.
                crate::executable_cache::write_executable(&probe, "#!/bin/bash\nif [ ! -c /dev/fd/0 ] || IFS= read -r input; then\n  echo 'command stdin was not restored to /dev/null' >&2\n  exit 44\nfi\nif [ ! -e /dev/fd/1 ] || [ ! -e /dev/fd/2 ]; then\n  echo 'descriptor lookup cannot see open stdio' >&2\n  exit 43\nfi\nfor fd in 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do\n  if [ -e \"/dev/fd/$fd\" ]; then\n    echo \"leaked control fd $fd\" >&2\n    exit 42\n  fi\ndone\nprintf clean\n".as_bytes()).unwrap();
                let payload = if pipeline {
                    format!("'{}' | cat", probe.display())
                } else {
                    format!("'{}'", probe.display())
                };
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    &payload,
                    "3",
                    if pipeline {
                        status_path.to_str().unwrap()
                    } else {
                        ""
                    },
                    kind,
                    "wait-for-identity",
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                use std::io::Write;
                use std::process::Stdio;
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = command.spawn().unwrap();
                child.stdin.take().unwrap().write_all(b"run\n").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    output.stdout,
                    b"clean",
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
                if pipeline {
                    assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "0\n0\n");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_pipeline_capture_preserves_shell_pipeline_semantics() {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for payload in [
                "! false | true",
                "time true | cat",
                "printf '%s' \"$0:${1-unset}:$#\" | cat",
                "printf 'sample\\n' | { read value; trap 'printf \"%s\\n\" \"$value\"' EXIT; }",
            ] {
                // The uninstrumented shell is the reference for shell-specific
                // last-stage scope, pipeline negation, timing and positional args.
                let reference = Command::new(shell)
                    .args(["-c", payload])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("pipeline-status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    payload,
                    "3",
                    status_path.to_str().unwrap(),
                    kind,
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                let captured = command.stdin(Stdio::null()).output().unwrap();
                assert_eq!(
                    captured.status.code(),
                    reference.status.code(),
                    "{shell}: {payload}"
                );
                assert_eq!(captured.stdout, reference.stdout, "{shell}: {payload}");
                assert!(status_path.is_file(), "{shell}: capture was not published");
                assert_eq!(
                    std::fs::read_to_string(exit_path).unwrap(),
                    reference.status.code().unwrap().to_string()
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_pty_child_starts_as_its_own_session_and_process_group_leader() {
        use std::process::Command;

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        start_new_session(&mut command);
        let mut child = command.spawn().expect("spawn isolated child");
        let pid = child.id() as libc::pid_t;

        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);

        terminate_process(&mut child);
        let _ = child.wait();
    }

    /// Regression: when the process-group LEADER exits during the SIGTERM grace
    /// window, `terminate_pgid` must still SIGKILL the rest of the group. A
    /// TERM-ignoring descendant (the wrapper-shell / CLI-spawns-child orphan
    /// class) used to survive because the old code returned the instant the
    /// leader was reaped, skipping the group SIGKILL.
    #[cfg(unix)]
    #[test]
    fn terminate_pgid_kills_term_ignoring_descendant_after_leader_exits() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("desc.pid");
        let ready = dir.path().join("ready");

        // Leader becomes its own process-group leader (setsid → pgid == pid).
        // It backgrounds a descendant shell that ignores SIGTERM, signals
        // readiness (so the trap is definitely installed before we terminate),
        // then sleeps. The leader waits for readiness and exits — so by the time
        // we call terminate_pgid, the leader is gone and only SIGKILL can reap
        // the descendant.
        let script = format!(
            "sh -c \"trap '' TERM; echo \\$$ > '{pid}'; touch '{ready}'; sleep 30\" & \
             {}; exit 0",
            crate::test_gate::gate(&format!("'{}'", ready.display()), "sleep 0.02"),
            pid = pidfile.display(),
            ready = ready.display(),
        );
        let mut leader = unsafe {
            std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn leader")
        };
        let pgid = leader.id() as i32;

        // Wait for the descendant to be ready (trap installed + pid written).
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        let desc_pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("descendant pid file")
            .trim()
            .parse()
            .expect("parse descendant pid");
        assert!(is_process_alive(desc_pid), "descendant should be alive");

        let stopping = Instant::now();
        terminate_pgid(pgid, Some(&mut leader));
        assert!(
            stopping.elapsed() >= TERMINATE_GRACE,
            "an ignoring descendant must receive the whole grace period"
        );

        // The TERM-ignoring descendant must be gone (SIGKILL'd via the group).
        let start = Instant::now();
        while is_process_alive(desc_pid) && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !is_process_alive(desc_pid),
            "TERM-ignoring descendant must be SIGKILLed when the group is terminated"
        );
    }

    #[cfg(unix)]
    fn termination_wrapper(command: &str, root: &Path) -> Child {
        let wrapper = std::str::from_utf8(PAYLOAD_WRAPPER).unwrap();
        let script = format!("exec 3>\"$8\"\n{wrapper}");
        let mut command_builder = std::process::Command::new("/bin/sh");
        command_builder
            .args([
                "-c",
                &script,
                "aft-termination-test",
                "/bin/sh",
                command,
                "3",
                "",
                "",
                "",
                "",
            ])
            .arg(root.join("exit"))
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        start_new_session(&mut command_builder);
        command_builder.spawn().unwrap()
    }

    #[cfg(unix)]
    fn wait_for_termination_file(path: &Path) {
        let started = Instant::now();
        while !path.exists() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "command did not create {}", path.display());
    }

    #[cfg(unix)]
    #[test]
    fn terminate_pgid_allows_term_cleanup_after_wrapper_exits() {
        let root = tempfile::tempdir().unwrap();
        let mut child = termination_wrapper(
            "trap 'sleep 0.3; echo cleaned > marker; exit 0' TERM; touch ready; while :; do sleep 0.05; done",
            root.path(),
        );
        wait_for_termination_file(&root.path().join("ready"));
        let stopping = Instant::now();
        terminate_process(&mut child);
        assert!(
            root.path().join("marker").exists(),
            "TERM cleanup was killed early"
        );
        assert!(stopping.elapsed() >= Duration::from_millis(300));
        assert!(
            stopping.elapsed() < TERMINATE_GRACE,
            "a group containing only exited processes must not use the entire grace period"
        );
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminate_pgid_gives_term_ignoring_command_full_grace() {
        use std::os::unix::process::ExitStatusExt;

        let root = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; touch ready; while :; do sleep 1; done"])
            .current_dir(root.path());
        start_new_session(&mut command);
        let mut child = command.spawn().unwrap();
        wait_for_termination_file(&root.path().join("ready"));
        let stopping = Instant::now();
        terminate_process(&mut child);
        assert!(stopping.elapsed() >= TERMINATE_GRACE);
        assert!(stopping.elapsed() < Duration::from_secs(15));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_pgid_lsp_leader_grace_ends_when_leader_exits() {
        let root = tempfile::tempdir().unwrap();
        let mut child = termination_wrapper(
            "trap '' TERM; echo $$ > pid; touch ready; exec sleep 30",
            root.path(),
        );
        wait_for_termination_file(&root.path().join("ready"));
        let descendant: u32 = std::fs::read_to_string(root.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let stopping = Instant::now();
        crate::lsp::client::kill_lsp_child_group(&mut child);
        assert!(
            stopping.elapsed() < TERMINATE_GRACE,
            "LSP cleanup must not wait for a descendant after its leader exits"
        );
        let killed = Instant::now();
        while is_process_alive(descendant) && killed.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_process_alive(descendant),
            "LSP cleanup must still kill descendants"
        );
    }
}
