//! Process-group isolation and cleanup for deadline-bound subprocesses.

#[cfg(windows)]
use std::process::Stdio;
use std::process::{Child, Command};

/// Apply Unix-specific isolation so a kill() on timeout terminates
/// grandchildren too (e.g. `sh -c 'sleep 60'` orphaning `sleep`).
///
/// Without this, killing the immediate child (`sh`) leaves `sleep`
/// holding stdout/stderr pipes open, and the reader threads block
/// until `sleep` terminates — turning a 2s timeout into a 60s hang.
///
/// The child leads a new process group whose id is its pid, which is all
/// `kill_process_tree` needs for `killpg`. Use `process_group(0)`, never a
/// `pre_exec` closure (such as a `setsid` call): std turns `process_group`
/// into a `posix_spawn` attribute, while any `pre_exec` closure forces the
/// fork+exec fallback. On macOS a fork of the large, many-threaded daemon
/// takes every malloc zone lock, so every formatter run on an edit would
/// stall allocation across the whole process.
#[cfg(unix)]
pub(crate) fn isolate_in_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn isolate_in_process_group(_cmd: &mut Command) {
    // Best-effort no-op outside Unix. Windows timeout cleanup uses taskkill /T
    // in kill_process_tree so .cmd wrappers and grandchildren are terminated.
}

/// Kill the child and, where supported, its entire process tree so wrapper
/// scripts cannot leave descendants running after a timeout.
#[cfg(unix)]
pub(crate) fn kill_process_tree(child: &mut Child) {
    let pid = child.id() as i32;
    if pid > 0 {
        // SAFETY: killpg with SIGKILL on a process group leader is safe.
        // Negative pid form (kill -pgid) targets the whole group.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(windows)]
pub(crate) fn kill_process_tree(child: &mut Child) {
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn kill_process_tree(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(unix, windows))]
    #[test]
    fn isolated_child_can_be_killed_and_reaped() {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 60"]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "ping -n 60 127.0.0.1 >NUL"]);
            command
        };
        isolate_in_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        kill_process_tree(&mut child);
        assert!(!child.wait().unwrap().success());
    }
}
