//! OS process start timestamps distinguish reused PIDs from their prior owners.

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::fs;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", any(test, feature = "test-support")))]
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Boot-relative Linux process birth, independent of the realtime clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxBirth {
    pub boot_id: String,
    pub start_ticks: u64,
}

#[cfg(target_os = "linux")]
impl LinuxBirth {
    pub fn of(pid: u32) -> Option<Self> {
        Self::from_start_ticks(subc_os::start_time(pid)?)
    }

    pub(crate) fn from_start_ticks(start_ticks: u64) -> Option<Self> {
        Some(Self {
            start_ticks,
            boot_id: subc_os::boot_id::current().ok()?,
        })
    }
}

/// A PID and its recorded epoch start time, with a stronger birth on Linux.
/// Use [`Self::same_process`] to compare identities. An old record without a
/// Linux birth record can match two identities with the same epoch. Those identities
/// do not match each other if their native births differ.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at_ms: u64,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub native_birth: Option<LinuxBirth>,
}

impl ProcessIdentity {
    pub fn of(pid: u32) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(start) = process_start_time_override(pid) {
                return Some(Self::from_epoch(pid, start?));
            }
            if let Some(birth) = LinuxBirth::of(pid) {
                return Some(Self {
                    pid,
                    started_at_ms: linux_start_time_ms(birth.start_ticks, boot_time_secs()?)?,
                    native_birth: Some(birth),
                });
            }
        }
        Some(Self::from_epoch(pid, process_start_time_ms(pid)?))
    }

    pub fn from_epoch(pid: u32, started_at_ms: u64) -> Self {
        Self {
            pid,
            started_at_ms,
            native_birth: None,
        }
    }

    /// Unlike structural equality, matching a legacy record uses its exact epoch.
    pub fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid
            && match (&self.native_birth, &other.native_birth) {
                (Some(saved), Some(current)) => saved == current,
                _ => self.started_at_ms == other.started_at_ms,
            }
    }
}

#[cfg(any(test, feature = "test-support"))]
static PROCESS_START_TIME_OVERRIDES: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
pub fn set_process_start_time_for_test(pid: u32, started_at_ms: Option<u64>) {
    PROCESS_START_TIME_OVERRIDES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("process start override mutex")
        .insert(pid, started_at_ms);
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_process_start_time_for_test(pid: u32) {
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
    linux_start_time_ms(start_ticks, boot_time_secs()?)
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_start_time_ms(start_ticks: u64, boot_time_secs: u64) -> Option<u64> {
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
pub(crate) fn read_boot_time_secs() -> Option<u64> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(seconds) = BOOT_TIME_OVERRIDE.get() {
        return Some(seconds);
    }
    read_proc_boot_time_secs()
}

#[cfg(target_os = "linux")]
fn read_proc_boot_time_secs() -> Option<u64> {
    fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<u64> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(seconds) = BOOT_TIME_OVERRIDE.get() {
        return Some(seconds);
    }
    static BOOT_TIME_SECS: OnceLock<Option<u64>> = OnceLock::new();
    cached_boot_time_secs(&BOOT_TIME_SECS, read_proc_boot_time_secs)
}

#[cfg(all(target_os = "linux", any(test, feature = "test-support")))]
thread_local! {
    static BOOT_TIME_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(target_os = "linux", any(test, feature = "test-support")))]
pub fn with_linux_boot_time_for_test<T>(seconds: u64, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOOT_TIME_OVERRIDE.set(self.0);
        }
    }
    let _restore = Restore(BOOT_TIME_OVERRIDE.replace(Some(seconds)));
    run()
}

#[cfg(target_os = "linux")]
pub fn boot_time() -> Option<std::time::SystemTime> {
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(boot_time_secs()?))
}

#[cfg(target_os = "macos")]
pub fn boot_time() -> Option<std::time::SystemTime> {
    let mut time: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&time);
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut time as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || len != std::mem::size_of_val(&time) || time.tv_sec <= 0 {
        return None;
    }
    // Round down so an artifact from the boot second is never treated as old.
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(time.tv_sec as u64))
}

#[cfg(windows)]
pub fn boot_time() -> Option<std::time::SystemTime> {
    #[repr(C)]
    struct TimeOfDay {
        boot_time: i64,
        current_time: i64,
        time_zone_bias: i64,
        time_zone_id: u32,
        reserved: u32,
        boot_time_bias: u64,
        sleep_time_bias: u64,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(
            class: u32,
            buffer: *mut std::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    let mut info: TimeOfDay = unsafe { std::mem::zeroed() };
    let len = std::mem::size_of_val(&info) as u32;
    let status = unsafe {
        // SystemTimeOfDayInformation (3) reports kernel boot time as FILETIME.
        NtQuerySystemInformation(
            3,
            (&mut info as *mut TimeOfDay).cast(),
            len,
            std::ptr::null_mut(),
        )
    };
    if status < 0 || info.boot_time >= info.current_time {
        return None;
    }
    const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
    let ticks = u64::try_from(info.boot_time.checked_sub(UNIX_EPOCH_TICKS)?).ok()?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(ticks / 10_000_000))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn boot_time() -> Option<std::time::SystemTime> {
    None
}

/// Keep the legacy epoch conversion consistent within one AFT process.
/// Shared with the platform-independent test that proves the reader runs once.
#[cfg(any(target_os = "linux", any(test, feature = "test-support")))]
#[cfg_attr(not(test), allow(dead_code))]
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
    fn same_process_matches_legacy_and_native_identities_at_the_same_epoch() {
        let legacy = ProcessIdentity::from_epoch(42, 1000);
        let native = ProcessIdentity {
            pid: 42,
            started_at_ms: 1000,
            native_birth: Some(LinuxBirth {
                boot_id: "boot".into(),
                start_ticks: 80,
            }),
        };
        assert!(legacy.same_process(&native));
        assert!(native.same_process(&legacy));
        assert!(!ProcessIdentity::from_epoch(42, 1001).same_process(&native));
        assert!(!ProcessIdentity::from_epoch(43, 1000).same_process(&native));

        let mut reused = native.clone();
        reused.native_birth.as_mut().unwrap().start_ticks += 1;
        assert!(legacy.same_process(&reused));
        assert!(!native.same_process(&reused));
    }

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
