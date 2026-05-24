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
                return;
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
}
