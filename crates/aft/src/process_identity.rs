//! OS process start timestamps distinguish reused PIDs from their prior owners.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::fs;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", test))]
use std::sync::OnceLock;

#[cfg(test)]
static PROCESS_START_TIME_OVERRIDES: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn set_process_start_time_for_test(pid: u32, started_at_ms: Option<u64>) {
    PROCESS_START_TIME_OVERRIDES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("process start override mutex")
        .insert(pid, started_at_ms);
}

#[cfg(test)]
pub(crate) fn clear_process_start_time_for_test(pid: u32) {
    if let Some(overrides) = PROCESS_START_TIME_OVERRIDES.get() {
        overrides
            .lock()
            .expect("process start override mutex")
            .remove(&pid);
    }
}

#[cfg(test)]
fn process_start_time_override(pid: u32) -> Option<Option<u64>> {
    PROCESS_START_TIME_OVERRIDES
        .get()
        .and_then(|overrides| overrides.lock().ok()?.get(&pid).copied())
}

#[cfg(target_os = "linux")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
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
#[cfg(any(target_os = "linux", test))]
fn cached_boot_time_secs(
    cache: &OnceLock<Option<u64>>,
    reader: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    *cache.get_or_init(reader)
}

#[cfg(target_os = "macos")]
/// Return the OS start time for `pid`, in Unix milliseconds, when available.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
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
    #[cfg(test)]
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
