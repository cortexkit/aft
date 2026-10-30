//! Prompt-free real macOS launch coverage. Only process attribution and ordinary
//! temporary files/terminal IO are observed; no TCC-protected API is exercised.

use super::helpers::{user_config, AftProcess};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "privacy-spawn";

fn macos_native_fixture_project() -> tempfile::TempDir {
    // First-exec assessment of a new executable under macOS's /var/folders
    // TMPDIR can consume the command's entire one-second timeout. Keep the
    // compiled probe under /tmp, as the native bridge fixtures do.
    tempfile::Builder::new()
        .prefix("aft-privacy-spawn-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, sandbox: bool) {
    let response = aft.send(&json!({
        "id": "cfg-privacy", "session_id": SESSION, "command": "configure",
        "harness": "opencode", "project_root": project, "storage_dir": storage,
        "config": user_config(json!({ "bash": { "disclaim_privacy": true }, "sandbox": { "enabled": sandbox } })),
    }).to_string());
    assert_eq!(response["success"], true, "{response}");
}

fn request(aft: &mut AftProcess, params: Value) -> Value {
    aft.send(
        &json!({"id":"privacy-command", "session_id":SESSION, "command":"bash", "params":params})
            .to_string(),
    )
}

fn terminal(aft: &mut AftProcess, response: Value) -> Value {
    let task = response["task_id"].as_str().expect("task id").to_owned();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = aft.send(&json!({"id":"privacy-status", "session_id":SESSION, "command":"bash_status", "params":{"task_id":task}}).to_string());
        assert_eq!(status["success"], true, "{status}");
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "timed_out" | "killed")
        ) {
            return status;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn output(status: &Value) -> String {
    if let Some(path) = status["output_path"].as_str() {
        std::fs::read_to_string(path).unwrap()
    } else {
        status["output_preview"]
            .as_str()
            .or_else(|| status["output"].as_str())
            .expect("command output")
            .to_owned()
    }
}

fn attribution_probe(project: &Path) {
    let mut cc = Command::new("/usr/bin/cc")
        .args(["-x", "c", "-o"])
        .arg(project.join("probe"))
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    cc.stdin.take().unwrap().write_all(br#"
#include <dlfcn.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
int main(int argc, char **argv) {
  /* getpgrp needs no process enumeration permission under Seatbelt. */
  if (argc == 2 && strcmp(argv[1], "group") == 0) { printf("%d\n", getpgrp()); return 0; }
  if (argc == 2 && strcmp(argv[1], "identity") == 0) { printf("%d %d %d\n", getpgrp(), getppid(), getuid()); return 0; }
  int (*responsible)(int) = dlsym(RTLD_DEFAULT, "responsibility_get_pid_responsible_for_pid");
  if (!responsible) return 41;
  int group = getpgrp();
  printf("group=%d responsible-group=%d responsible-child=%d session=%d tty=%d\n", group, responsible(group), responsible(getpid()), getsid(0), isatty(0));
  /* Later exec descendants may be attributed to themselves. The launch group's
     leader must be its own responsible process, never the AFT supervisor. */
  if (responsible(group) != group || getsid(0) != group) return 42;
  if (fcntl(3, F_GETFD) >= 0 || fcntl(4, F_GETFD) >= 0) return 43;
  if (isatty(0)) {
    /* Seatbelt need not permit reopening /dev/tty. Query the inherited fd. */
    if (tcgetpgrp(0) != group) return 44;
    struct winsize size;
    if (ioctl(0, TIOCGWINSZ, &size) != 0) return 45;
    printf("size=%dx%d\n", size.ws_row, size.ws_col);
  }
  puts("own-responsible-group");
  return 0;
}
"#).unwrap();
    assert!(cc.wait().unwrap().success());
    super::helpers::warm_executable(&project.join("probe"), &["group"]);
}

#[test]
fn disclaimed_foreground_background_and_pty_preserve_attribution_io_and_exit_with_seatbelt() {
    let project = macos_native_fixture_project();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn();
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            let response = request(
                &mut aft,
                json!({
                    "command": "./probe || exit $?; printf 'env=%s\\n' \"$PRIVACY_TEST\"; pwd; printf stderr >&2; exit 23",
                "background": background, "pty":pty, "wait": !background,
                "pty_rows":29, "pty_cols":97,
                    "compressed":false, "env":{"PRIVACY_TEST":"kept"}, "timeout": 10000,
                }),
            );
            assert_eq!(
                response["success"], true,
                "sandbox={sandbox} pty={pty}: {response}"
            );
            let status = terminal(&mut aft, response);
            let text = output(&status);
            assert_eq!(
                status["exit_code"], 23,
                "sandbox={sandbox} pty={pty}: {status} output={text}"
            );
            assert!(text.contains(if pty { "tty=1" } else { "tty=0" }), "{text}");
            assert!(text.contains("own-responsible-group"), "{text}");
            assert!(text.contains("env=kept"), "{text}");
            assert!(
                text.contains(project.path().canonicalize().unwrap().to_str().unwrap()),
                "{text}"
            );
            if pty {
                assert!(text.contains("stderr"), "{text}");
                assert!(text.contains("size=29x97"), "{text}");
            }
        }
    }
}

fn timeout_group_snapshot(group: i32) -> String {
    match Command::new("/bin/ps")
        .args(["-o", "pid,ppid,pgid,uid,stat,lstart,comm", "-g"])
        .arg(group.to_string())
        .output()
    {
        Ok(output) => format!(
            "ps -g {group}: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
        Err(error) => format!("ps -g {group} failed: {error}"),
    }
}

/// One process the kernel still files under a process group.
#[derive(Debug)]
struct GroupMember {
    pid: i32,
    /// `proc_pidinfo(PROC_PIDTBSDINFO)` result: the `pbi_status` run state, or
    /// the errno when the kernel would not describe the process.
    state: Result<u32, i32>,
}

impl GroupMember {
    /// Whether this member can still run. macOS refuses BSD info with ESRCH for
    /// a process that has started exiting (it can no longer run or be
    /// signalled, but stays listed in its group until its parent reaps it),
    /// and reports `SZOMB` for an exited, unreaped one. Any other refusal
    /// leaves the member's state unknown, so it counts as live.
    fn is_live(&self) -> bool {
        match self.state {
            Ok(status) => status != libc::SZOMB,
            Err(errno) => errno != libc::ESRCH,
        }
    }
}

/// Lists the processes the kernel files under `group`, including ones that
/// are exiting or exited but not yet reaped.
fn group_members(group: i32) -> Vec<GroupMember> {
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 256];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            group as u32,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    pids.truncate(usize::try_from(bytes).unwrap_or(0) / std::mem::size_of::<i32>());
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            let state = if read == size {
                Ok(info.pbi_status)
            } else {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            };
            GroupMember { pid, state }
        })
        .collect()
}

/// Waits until `group` no longer exists, or returns why it still does at
/// `deadline`.
///
/// `kill(-group, 0)` failing with ESRCH means the group is gone. On macOS it
/// fails with EPERM instead while the group still exists but holds no process
/// that could take a signal: for example, right after the leader is reaped, an
/// orphaned member killed alongside it is still exiting or waiting for launchd
/// to reap it. Such a group has nothing left to kill, so keep polling for it to
/// disappear. An EPERM group that still has a live member is a survivor this
/// process is not allowed to signal, and fails at once.
fn await_group_exit(group: i32, deadline: Instant) -> Result<(), String> {
    loop {
        let result = unsafe { libc::kill(-group, 0) };
        let error = std::io::Error::last_os_error().raw_os_error();
        let members = if result == 0 {
            None
        } else if error == Some(libc::ESRCH) {
            return Ok(());
        } else if error == Some(libc::EPERM) {
            let members = group_members(group);
            if members.iter().any(GroupMember::is_live) {
                return Err(format!(
                    "kill(-{group}, 0) failed with EPERM while the group has a live member: {members:?}"
                ));
            }
            Some(members)
        } else {
            return Err(format!("kill(-{group}, 0)={result}, errno={error:?}"));
        };
        if Instant::now() >= deadline {
            return Err(match members {
                None => format!("process group {group} still accepts signals"),
                Some(members) => format!(
                    "process group {group} was never reaped; members that can no longer run: {members:?}"
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn disclaimed_timeout_kills_foreground_background_and_pty_groups_with_seatbelt() {
    let project = macos_native_fixture_project();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn_with_env(&[("RUST_LOG", std::ffi::OsStr::new("aft=debug"))]);
    let only_sandboxed_foreground =
        std::env::var("AFT_TEST_PRIVACY_TIMEOUT_ONLY_SANDBOXED_FOREGROUND").as_deref() == Ok("1");
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            if only_sandboxed_foreground && (!sandbox || background || pty) {
                continue;
            }
            let case = format!("sandbox={sandbox} background={background} pty={pty}");
            eprintln!("timeout-group starting {case}");
            let group_file = project.path().join("group");
            let _ = std::fs::remove_file(&group_file);
            let response = request(
                &mut aft,
                json!({
                    "command":"./probe identity > group; printf started; trap '' TERM; sleep 30", "background":background, "pty":pty,
                    "compressed":false, "timeout":1000,
                }),
            );
            assert_eq!(response["success"], true, "{response}");
            let status = terminal(&mut aft, response.clone());
            assert_eq!(status["status"], "timed_out", "{status}");
            assert_eq!(status["exit_code"], 124, "{status}");
            if !output(&status).contains("started") {
                let stderr = status["stderr_path"].as_str().map(std::fs::read_to_string);
                eprintln!("{case}: no started marker; launch response={response:#}\ntask stderr={stderr:?}\ngroup file={:?}", std::fs::read_to_string(&group_file));
                let (exit, stderr) = aft.stderr_output();
                eprintln!("{case}: supervisor exit={exit}; sandbox startup diagnostics:\n{stderr}");
                panic!("{case}: command did not reach the started marker: {status:#}");
            }
            let identity = std::fs::read_to_string(group_file).unwrap();
            let ids: Vec<i32> = identity
                .split_whitespace()
                .map(|id| id.parse().unwrap())
                .collect();
            assert_eq!(ids.len(), 3, "{case}: {identity:?}");
            let group = ids[0];
            eprintln!(
                "timeout-group {case}: pgid={group} shell_pid={} uid={}",
                ids[1], ids[2]
            );
            assert!(group > 0);
            if let Err(reason) = await_group_exit(group, Instant::now() + Duration::from_secs(5)) {
                panic!("{case}: timed-out process group {group} survived: {reason}, identity={identity:?}\n{}\nstatus={status}", timeout_group_snapshot(group));
            }
        }
    }
}

/// Spawns `/usr/bin/true` as the leader of its own process group and returns
/// once it has exited but is still unreaped, the state in which macOS answers
/// `kill(-group, 0)` with EPERM rather than ESRCH.
fn unreaped_exited_group() -> (std::process::Child, i32) {
    use std::os::unix::process::CommandExt;
    let child = Command::new("/usr/bin/true")
        .process_group(0)
        .spawn()
        .unwrap();
    let group = child.id() as i32;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let result = unsafe { libc::kill(-group, 0) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            return (child, group);
        }
        assert!(
            Instant::now() < until,
            "group {group} never became exited-but-unreaped"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn timeout_group_wait_outlasts_eperm_from_an_exited_unreaped_member() {
    let (mut child, group) = unreaped_exited_group();
    let reaper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        child.wait().unwrap()
    });
    let result = await_group_exit(group, Instant::now() + Duration::from_secs(5));
    reaper.join().unwrap();
    assert_eq!(result, Ok(()));
}

#[test]
fn timeout_group_wait_reports_an_exited_member_never_reaped_by_its_deadline() {
    let (mut child, group) = unreaped_exited_group();
    let result = await_group_exit(group, Instant::now() + Duration::from_millis(200));
    child.wait().unwrap();
    let reason = result.unwrap_err();
    assert!(
        reason.contains("was never reaped") && reason.contains(&format!("pid: {group}")),
        "{reason}"
    );
}

#[test]
fn timeout_group_wait_reports_a_live_member_by_its_deadline() {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let group = child.id() as i32;
    let result = await_group_exit(group, Instant::now() + Duration::from_millis(200));
    child.kill().unwrap();
    child.wait().unwrap();
    let reason = result.unwrap_err();
    assert!(reason.contains("still accepts signals"), "{reason}");
}
