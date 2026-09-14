#[cfg(unix)]
use std::path::Path;

use serde::{Deserialize, Serialize};
/// Shared process-termination helpers for both foreground bash and background
/// bash tasks. Extracted to avoid duplication between `commands/bash.rs` and
/// `bash_background/registry.rs`.
///
/// Termination is graceful-first: SIGTERM + 3-second grace period, then
/// SIGKILL on Unix. On Windows, `taskkill /T /F` kills the entire process tree.
use std::process::Child;
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);
pub const LIVE_DESCENDANT_CAP: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveDescendant {
    pub pid: u32,
    pub comm: String,
    pub argv0: String,
}

/// Enumerate surviving members of a task's process group. `None` means the
/// platform has no process-group enumeration support for these tasks today.
pub fn live_process_group_members(pgid: i32) -> Option<(Vec<LiveDescendant>, usize)> {
    #[cfg(target_os = "linux")]
    let mut members = linux_process_group_members(pgid);
    #[cfg(target_os = "macos")]
    let mut members = macos_process_group_members(pgid);
    #[cfg(windows)]
    {
        let _ = pgid;
        return None;
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = pgid;
        return None;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        members.sort_by_key(|member| member.pid);
        let omitted = members.len().saturating_sub(LIVE_DESCENDANT_CAP);
        members.truncate(LIVE_DESCENDANT_CAP);
        Some((members, omitted))
    }
}

#[cfg(target_os = "linux")]
fn linux_process_group_members(pgid: i32) -> Vec<LiveDescendant> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| linux_process_member(pid, pgid))
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_process_member(pid: u32, expected_pgid: i32) -> Option<LiveDescendant> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(") ")?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    let state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    if pgrp != expected_pgid || state == "Z" {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let argv0 = cmdline
        .split(|byte| *byte == 0)
        .next()
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_else(|| comm.clone());
    Some(LiveDescendant { pid, comm, argv0 })
}

#[cfg(target_os = "macos")]
fn macos_process_group_members(pgid: i32) -> Vec<LiveDescendant> {
    use std::ffi::{c_char, c_int, c_void};

    #[link(name = "proc")]
    extern "C" {
        fn proc_listpgrppids(pgrpid: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    if pgid <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0_u32; 16_384];
    let count = unsafe {
        proc_listpgrppids(
            pgid as u32,
            pids.as_mut_ptr().cast(),
            i32::try_from(pids.len() * std::mem::size_of::<u32>()).unwrap_or(i32::MAX),
        )
    };
    if count <= 0 {
        return Vec::new();
    }
    pids.truncate(count as usize);
    pids.into_iter()
        .filter(|pid| *pid != 0)
        .filter_map(|pid| {
            let mut name = [0 as c_char; 1024];
            let name_len =
                unsafe { proc_name(pid as c_int, name.as_mut_ptr().cast(), name.len() as u32) };
            if name_len <= 0 {
                return None;
            }
            let comm = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let argv0 = macos_argv0(pid).unwrap_or_else(|| comm.clone());
            Some(LiveDescendant { pid, comm, argv0 })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_argv0(pid: u32) -> Option<String> {
    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, i32::try_from(pid).ok()?];
    let mut size = 0_usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size <= std::mem::size_of::<libc::c_int>()
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    let mut cursor = std::mem::size_of::<libc::c_int>();
    cursor += bytes.get(cursor..)?.iter().position(|byte| *byte == 0)?;
    while bytes.get(cursor).is_some_and(|byte| *byte == 0) {
        cursor += 1;
    }
    let end = cursor
        + bytes
            .get(cursor..)?
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len().saturating_sub(cursor));
    (end > cursor).then(|| String::from_utf8_lossy(&bytes[cursor..end]).into_owned())
}

/// The Unix payload wrapper runs the user's command in the same shell that
/// owns the pipeline. Bash exposes per-segment statuses as `PIPESTATUS`, while
/// zsh exposes them as `pipestatus`; plain POSIX sh has neither and therefore
/// receives the original command without a capture suffix. The status stream
/// uses fd 5, which the parent opens only for a clean pipeline and places beside
/// the ordinary exit marker. Windows shells use their existing wrapper and do
/// not have a portable per-segment status equivalent.
#[cfg(unix)]
pub(crate) const PAYLOAD_WRAPPER: &[u8] = br#"#!/bin/sh
shell=$1
command=$2
exit_fd=$3
pipeline_status_fd=$4
pipeline_shell=$5
case "$pipeline_status_fd:$pipeline_shell" in
  5:bash)
    "$shell" -c "$command
__aft_code=\$? __aft_ps=(\"\${PIPESTATUS[@]}\")
printf '%s\\n' \"\${__aft_ps[@]}\" >&5 2>/dev/null || true
exit \"\$__aft_code\""
    ;;
  5:zsh)
    "$shell" -c "$command
__aft_code=\$? __aft_ps=(\"\${pipestatus[@]}\")
printf '%s\\n' \"\${__aft_ps[@]}\" >&5 2>/dev/null || true
exit \"\$__aft_code\""
    ;;
  *)
    # POSIX sh has no per-segment pipeline status array; preserve the original
    # invocation instead of changing its semantics with a best-effort guess.
    "$shell" -c "$command"
    ;;
esac
code=$?
printf "%s" "$code" >&"$exit_fd"
exit "$code"
"#;

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

#[cfg(unix)]
pub fn terminate_process(child: &mut Child) {
    let pgid = child.id() as i32;
    terminate_pgid(pgid, Some(child));
}

#[cfg(unix)]
pub fn terminate_pgid(pgid: i32, mut child: Option<&mut Child>) {
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let grace_started = Instant::now();
    while grace_started.elapsed() < TERMINATE_GRACE {
        if let Some(child) = child.as_deref_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                // The direct child (process-group leader) exited. Stop waiting,
                // but still SIGKILL the whole group below — a descendant that
                // ignored SIGTERM can outlive the leader (the wrapper-shell /
                // CLI-spawns-child orphan class). killpg on an already-empty
                // group is a harmless ESRCH.
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn terminate_process(child: &mut Child) {
    terminate_pid(child.id());
}

#[cfg(windows)]
pub fn terminate_pid(pid: u32) {
    let pid = pid.to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> Handle;
        fn GetExitCodeProcess(hProcess: Handle, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: Handle) -> i32;
    }

    const FALSE: i32 = 0;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 0x103;

    if pid == 0 {
        return false;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_process_alive_returns_true_for_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_captures_bash_and_zsh_pipeline_statuses() {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }

        for (shell, kind) in shells {
            let temp = tempfile::tempdir().expect("create wrapper test directory");
            let exit_path = temp.path().join("exit");
            let status_path = temp.path().join("pipeline-status");
            let exit_file = std::fs::File::create(&exit_path).expect("create exit marker");
            let status_file = std::fs::File::create(&status_path).expect("create status file");
            let exit_fd = exit_file.as_raw_fd();
            let status_fd = status_file.as_raw_fd();
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                std::str::from_utf8(PAYLOAD_WRAPPER).expect("wrapper is UTF-8"),
                "aft-payload-wrapper",
                shell,
                "false | true",
                "3",
                "5",
                kind,
            ]);
            unsafe {
                command.pre_exec(move || {
                    // Mirror apply_marker_fd_allowlist's two-step: a raw
                    // dup2(fd, N) is a no-op when fd == N already, which keeps
                    // the descriptor CLOEXEC and silently closes it at exec.
                    // Parking above the target range first makes the final
                    // dup2 a real copy that clears CLOEXEC.
                    let exit_copy = libc::fcntl(exit_fd, libc::F_DUPFD_CLOEXEC, 6);
                    let status_copy = libc::fcntl(status_fd, libc::F_DUPFD_CLOEXEC, 6);
                    if exit_copy < 0
                        || status_copy < 0
                        || libc::dup2(exit_copy, 3) < 0
                        || libc::dup2(status_copy, 5) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(exit_copy);
                    libc::close(status_copy);
                    Ok(())
                });
            }
            let result = command.status().expect("run payload wrapper");
            drop(exit_file);
            drop(status_file);
            assert!(result.success(), "wrapper failed for {kind}");
            assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "1\n0\n");
            assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
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

    #[test]
    fn is_process_alive_returns_false_for_dead_pid() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn true");

        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("spawn cmd");

        let pid = child.id();
        child.wait().expect("wait for child");

        assert!(!is_process_alive(pid));
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
             while [ ! -f '{ready}' ]; do sleep 0.02; done; exit 0",
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

        terminate_pgid(pgid, Some(&mut leader));

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
}

/// Check that a persisted PID still names the process instance launched for a task.
/// On platforms without process start-time inspection, a live PID is protected;
/// deleting a live task is worse than temporarily retaining a recycled PID.
pub fn is_recorded_process_alive(pid: u32, task_started_at_ms: u64) -> bool {
    const PROCESS_START_GRACE_MS: u64 = 30_000;

    if !is_process_alive(pid) {
        return false;
    }
    crate::root_cache::process_start_time_ms(pid).is_none_or(|started_at_ms| {
        started_at_ms <= task_started_at_ms.saturating_add(PROCESS_START_GRACE_MS)
    })
}
