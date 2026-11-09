//! Reads the caller's return addresses and the thread's name for an event,
//! without allocating: both run inside the allocator.

use super::ring::{Event, MAX_FRAMES};

/// Fills the event's frames and thread name.
#[inline(never)]
pub(super) fn fill(event: &mut Event) {
    // This function and the allocator hook that called it are the event's
    // machinery, not its cause.
    event.frame_count = frames(&mut event.frames, 2) as u8;
    event.thread_len = thread_name(&mut event.thread) as u8;
}

#[cfg(unix)]
mod unwind {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    #[repr(C)]
    pub(super) struct Context {
        _private: [u8; 0],
    }

    pub(super) type Trace = extern "C" fn(*mut Context, *mut c_void) -> c_int;

    pub(super) const NO_REASON: c_int = 0;
    pub(super) const FAILURE: c_int = 9;

    // Every Rust binary on a unix target already links the system unwinder
    // (libgcc_s, LLVM libunwind or libSystem) for panics.
    extern "C" {
        pub(super) fn _Unwind_Backtrace(trace: Trace, arg: *mut c_void) -> c_int;
        pub(super) fn _Unwind_GetIP(context: *mut Context) -> usize;
    }
}

#[cfg(unix)]
struct Collector<'a> {
    frames: &'a mut [usize; MAX_FRAMES],
    count: usize,
    skip: usize,
}

#[cfg(unix)]
extern "C" fn collect(
    context: *mut unwind::Context,
    arg: *mut std::ffi::c_void,
) -> std::os::raw::c_int {
    // SAFETY: `arg` is the collector `frames` passed, alive for the walk.
    let collector = unsafe { &mut *arg.cast::<Collector<'_>>() };
    // SAFETY: the unwinder hands a valid context for the current frame.
    let ip = unsafe { unwind::_Unwind_GetIP(context) };
    if ip == 0 || collector.count >= collector.frames.len() {
        return unwind::FAILURE;
    }
    if collector.skip > 0 {
        collector.skip -= 1;
        return unwind::NO_REASON;
    }
    collector.frames[collector.count] = ip;
    collector.count += 1;
    unwind::NO_REASON
}

/// Writes up to [`MAX_FRAMES`] return addresses of the current stack into
/// `frames`, after skipping the innermost `skip` beyond this function.
/// Returns how many it wrote.
#[cfg(unix)]
#[inline(never)]
fn frames(frames: &mut [usize; MAX_FRAMES], skip: usize) -> usize {
    let mut collector = Collector {
        frames,
        count: 0,
        // The walk starts in this function.
        skip: skip + 1,
    };
    // SAFETY: the callback only touches the collector, which outlives the walk.
    unsafe {
        unwind::_Unwind_Backtrace(collect, (&mut collector as *mut Collector<'_>).cast());
    }
    collector.count
}

#[cfg(not(unix))]
fn frames(_frames: &mut [usize; MAX_FRAMES], _skip: usize) -> usize {
    0
}

#[cfg(target_os = "macos")]
fn thread_name(out: &mut [u8; 32]) -> usize {
    // SAFETY: the buffer is writable for its whole length, and the name is
    // read from the calling thread's own record.
    let status = unsafe {
        libc::pthread_getname_np(libc::pthread_self(), out.as_mut_ptr().cast(), out.len())
    };
    if status != 0 {
        return 0;
    }
    out.iter().position(|&byte| byte == 0).unwrap_or(out.len())
}

#[cfg(target_os = "linux")]
fn thread_name(out: &mut [u8; 32]) -> usize {
    // PR_GET_NAME writes at most 16 bytes, including the terminator.
    // SAFETY: the buffer is at least 16 bytes long.
    let status = unsafe { libc::prctl(libc::PR_GET_NAME, out.as_mut_ptr()) };
    if status != 0 {
        return 0;
    }
    out.iter().position(|&byte| byte == 0).unwrap_or(out.len())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn thread_name(_out: &mut [u8; 32]) -> usize {
    0
}
