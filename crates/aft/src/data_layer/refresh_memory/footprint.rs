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

#[cfg(target_os = "macos")]
pub(super) fn bytes() -> Option<u64> {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::task::task_info;
    use mach2::task_info::{task_vm_info, TASK_VM_INFO};
    use mach2::traps::mach_task_self;
    use std::mem::{offset_of, size_of, MaybeUninit};

    let mut info = MaybeUninit::<task_vm_info>::zeroed();
    let mut words = (size_of::<task_vm_info>() / size_of::<i32>()) as u32;
    // SAFETY: the current task is valid and the output buffer holds the number
    // of integer words passed to task_info. Older kernels can return fewer.
    let result = unsafe {
        task_info(
            mach_task_self(),
            TASK_VM_INFO,
            info.as_mut_ptr().cast(),
            &mut words,
        )
    };
    let needed = offset_of!(task_vm_info, phys_footprint) + size_of::<u64>();
    if result != KERN_SUCCESS || (words as usize) * size_of::<i32>() < needed {
        return None;
    }
    // SAFETY: task_info supplied the footprint, and the rest was zeroed.
    Some(unsafe { info.assume_init() }.phys_footprint)
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
