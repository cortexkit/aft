//! Cheap readings of the current process, without allocator introspection.

#[cfg(target_os = "linux")]
pub(super) fn bytes() -> Option<u64> {
    use std::io::Read;
    use std::sync::OnceLock;

    static PAGE_SIZE: OnceLock<Option<u64>> = OnceLock::new();
    let size = (*PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf reads the system page size and takes no pointers.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        u64::try_from(size).ok().filter(|size| *size != 0)
    }))?;
    let mut statm = [0; 128];
    let read = std::fs::File::open("/proc/self/statm")
        .ok()?
        .read(&mut statm)
        .ok()?;
    let resident = std::str::from_utf8(&statm[..read])
        .ok()?
        .split_ascii_whitespace()
        .nth(1)?;
    resident.parse::<u64>().ok()?.checked_mul(size)
}

/// The kernel's physical footprint (`phys_footprint`), read with the same
/// `proc_pid_rusage` call the memory census uses for macOS.
#[cfg(target_os = "macos")]
pub(super) fn bytes() -> Option<u64> {
    crate::memory::process_phys_footprint_bytes()
}

#[cfg(windows)]
pub(super) fn bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut info = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of_val(&info) as u32;
    // SAFETY: the pseudo-handle refers to this process, and the output buffer
    // is the extended counters structure selected by its byte size.
    let result = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut info as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size,
        )
    };
    (result != 0).then_some(info.PrivateUsage as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn bytes() -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    #[test]
    fn process_footprint_is_available() {
        assert!(super::bytes().is_some_and(|bytes| bytes > 0));
    }
}
