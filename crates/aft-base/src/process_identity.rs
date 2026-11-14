//! OS process start timestamps distinguish reused PIDs from their prior owners.

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", any(test, feature = "test-support")))]
use std::fs;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", any(test, feature = "test-support")))]
use std::sync::OnceLock;

#[cfg(any(test, feature = "test-support"))]
static PROCESS_START_TIME_OVERRIDES: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_process_start_time_for_test(pid: u32, started_at_ms: Option<u64>) {
    PROCESS_START_TIME_OVERRIDES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("process start override mutex")
        .insert(pid, started_at_ms);
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn clear_process_start_time_for_test(pid: u32) {
    if let Some(overrides) = PROCESS_START_TIME_OVERRIDES.get() {
        overrides
            .lock()
            .expect("process start override mutex")
            .remove(&pid);
    }
}

#[cfg(any(test, feature = "test-support"))]
fn process_start_time_override(pid: u32) -> Option<Option<u64>> {
    PROCESS_START_TIME_OVERRIDES
        .get()
        .and_then(|overrides| overrides.lock().ok()?.get(&pid).copied())
}

#[cfg(target_os = "linux")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let boot_time_secs = boot_time_secs()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let ticks_per_second = ticks_per_second as u64;
    Some(
        boot_time_secs
            .saturating_mul(1_000)
            .saturating_add(start_ticks.saturating_mul(1_000) / ticks_per_second),
    )
}

#[cfg(target_os = "linux")]
fn read_boot_time_secs() -> Option<u64> {
    fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<u64> {
    static BOOT_TIME_SECS: OnceLock<Option<u64>> = OnceLock::new();
    cached_boot_time_secs(&BOOT_TIME_SECS, read_boot_time_secs)
}

/// Read once per process: boot time cannot change while the process runs.
/// Shared with the platform-independent test that proves the reader runs once.
#[cfg(any(target_os = "linux", any(test, feature = "test-support")))]
fn cached_boot_time_secs(
    cache: &OnceLock<Option<u64>>,
    reader: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    *cache.get_or_init(reader)
}

#[cfg(target_os = "macos")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    crate::fs_lock::macos_kinfo(pid)?
        .started_at_us
        .map(|start| start / 1_000)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }
    #[cfg(windows)]
    {
        crate::fs_lock::windows_process_start_time_ms(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audit_growth_boot_time_cache_reads_its_source_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proc-stat-fixture");
        fs::write(&path, "btime 1234\n").unwrap();
        let cache = OnceLock::new();
        let reads = std::cell::Cell::new(0);
        let first = cached_boot_time_secs(&cache, || {
            reads.set(reads.get() + 1);
            fs::read_to_string(&path)
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
        });
        let second = cached_boot_time_secs(&cache, || {
            reads.set(reads.get() + 1);
            fs::read_to_string(&path)
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
        });
        assert_eq!(first, Some(1234));
        assert_eq!(second, first);
        assert_eq!(
            reads.get(),
            1,
            "/proc/stat's equivalent source is read once"
        );
    }
}

/// A failed identity probe is not evidence that a task has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLiveness {
    Gone,
    Alive,
    Unknown,
}

pub fn is_process_alive(pid: u32) -> bool {
    probe_process(pid).1
}

// The second value is recovery's boolean: OS probe errors remain false.
// The recorded-process probe keeps a live PID with an unreadable start time alive.
#[cfg(unix)]
pub fn probe_process(pid: u32) -> (ProcessLiveness, bool) {
    let Ok(pid) = i32::try_from(pid) else {
        return (ProcessLiveness::Unknown, false);
    };
    if pid <= 0 {
        return (ProcessLiveness::Unknown, false);
    }
    if unsafe { libc::kill(pid, 0) == 0 } {
        return (ProcessLiveness::Alive, true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => (ProcessLiveness::Alive, true),
        Some(libc::ESRCH) => (ProcessLiveness::Gone, false),
        _ => (ProcessLiveness::Unknown, false),
    }
}

#[cfg(windows)]
pub fn probe_process(pid: u32) -> (ProcessLiveness, bool) {
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
        return (ProcessLiveness::Unknown, false);
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            // Windows reports ERROR_INVALID_PARAMETER for a nonexistent PID.
            let state = if std::io::Error::last_os_error().raw_os_error() == Some(87) {
                ProcessLiveness::Gone
            } else {
                ProcessLiveness::Unknown
            };
            return (state, false);
        }
        let mut exit_code = 0;
        let state = if GetExitCodeProcess(handle, &mut exit_code) == 0 {
            ProcessLiveness::Unknown
        } else if exit_code == STILL_ACTIVE {
            ProcessLiveness::Alive
        } else {
            ProcessLiveness::Gone
        };
        let _ = CloseHandle(handle);
        (state, state == ProcessLiveness::Alive)
    }
}

#[cfg(test)]
mod liveness_tests {
    use super::is_process_alive;

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
