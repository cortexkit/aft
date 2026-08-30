//! Per-thread OS priority demotion for background maintenance work.
//!
//! The executor shares its worker pool between interactive requests and
//! maintenance-class jobs; dedicated background threads (callgraph refresh,
//! inspect engines, semantic re-embedders) run maintenance exclusively. This
//! module demotes the *current thread's* CPU and I/O priority while a
//! maintenance job runs, and restores it afterwards, so interactive reader
//! requests always beat indexer work in the OS scheduler regardless of the
//! executor's own queue fairness.
//!
//! Platform mapping (all per-thread, no process-wide demotion):
//! - Linux: `sched_setscheduler(0, SCHED_IDLE, ...)` + raw syscall
//!   `ioprio_set(IOPRIO_WHO_PROCESS, tid, IOPRIO_CLASS_IDLE)` — the kernel
//!   uapi has no `IOPRIO_WHO_TID`, but `WHO_PROCESS` targets the task with
//!   the given pid, i.e. a single thread. I/O priority is per-*thread* on
//!   Linux: the kernel attributes the I/O to the task that issued it.
//!   `SCHED_IDLE` is lower than any other thread's nice value, so maintenance
//!   yields CPU to every interactive request. Restores to `SCHED_OTHER`
//!   (nice 0) and `IOPRIO_CLASS_BE` (nice 0).
//! - macOS: `pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, ...)`. Darwin's
//!   I/O scheduling follows the QoS class of the thread that issued the I/O
//!   (the `IOPressure`/`thread_throughput_qos` mechanisms), so one call covers
//!   CPU and I/O. Restores to `QOS_CLASS_DEFAULT`.
//! - Windows: `SetThreadPriority(THREAD_PRIORITY_LOWEST)`; I/O operations
//!   inherit the issuing thread's priority. Restores to `THREAD_PRIORITY_NORMAL`.
//!
//! All calls are best-effort: a failed demotion logs a one-line warning and
//! never blocks or fails the calling job. Unprivileged users may set
//! `SCHED_IDLE`/idle-prio class without capabilities.

// Per-thread warning guard: log at most once per thread per class.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
thread_local! {
    static WARNED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn warn_once(kind: &str, err: &str) {
    WARNED.with(|w| {
        let bits = w.get();
        let flag = match kind {
            "cpu" => 1u8,
            "io" => 2,
            _ => 0,
        };
        if bits & flag == 0 {
            w.set(bits | flag);
            log::warn!("thread priority demotion failed ({kind}): {err}");
        }
    });
}

#[cfg(target_os = "linux")]
mod imp {
    use super::warn_once;
    use libc::{c_int, c_long, syscall};

    pub fn demote() {
        cpu_idle();
        io_idle();
    }

    pub fn restore() {
        cpu_other();
        io_best_effort();
    }

    /// SCHED_IDLE is not bound by the `libc` crate on gnu/musl; the value is a
    /// stable Linux ABI. The manifest reserves this change to demonstrate the
    /// scheduling test.
    #[allow(dead_code)]
    pub(super) const SCHED_IDLE: c_int = 5;
    #[allow(dead_code)]
    pub(super) const SCHED_OTHER: c_int = 0;

    pub(super) const IOPRIO_CLASS_IDLE: c_int = 3;
    pub(super) const IOPRIO_CLASS_BE: c_int = 2;
    pub(super) const IOPRIO_WHO_PROCESS: c_int = 1;
    const IOPRIO_CLASS_SHIFT: c_int = 13;
    const IOPRIO_NICE_SHIFT: c_int = 0;

    fn cpu_idle() {
        let mut param = unsafe { std::mem::zeroed::<libc::sched_param>() };
        param.sched_priority = 0;
        let rc = unsafe { libc::sched_setscheduler(0, SCHED_IDLE, &param) };
        if rc != 0 {
            warn_once("cpu", &std::io::Error::last_os_error().to_string());
        }
    }

    fn cpu_other() {
        let mut param = unsafe { std::mem::zeroed::<libc::sched_param>() };
        param.sched_priority = 0;
        let rc = unsafe { libc::sched_setscheduler(0, SCHED_OTHER, &param) };
        if rc != 0 {
            warn_once("cpu", &std::io::Error::last_os_error().to_string());
        }
    }

    pub(super) fn tid() -> c_int {
        unsafe { syscall(c_long::from(libc::SYS_gettid)) as c_int }
    }

    pub(super) fn io_prio(class: c_int, nice: c_int) -> c_int {
        (class << IOPRIO_CLASS_SHIFT) | (nice << IOPRIO_NICE_SHIFT)
    }

    /// Raw syscall: `ioprio_set` is not bound by the `libc` crate for
    /// gnu/musl (it exists in glibc 2.14+ and musl as a libc call, but the
    /// syscall number is per-arch and constant; using the syscall keeps a
    /// single code path across linkers).
    fn io_set(who: c_int, id: c_int, prio: c_int) -> bool {
        let rc = unsafe { syscall(libc::SYS_ioprio_set, who, id, prio) };
        rc == 0
    }

    fn io_idle() {
        if !io_set(IOPRIO_WHO_PROCESS, tid(), io_prio(IOPRIO_CLASS_IDLE, 0)) {
            warn_once("io", &std::io::Error::last_os_error().to_string());
        }
    }

    fn io_best_effort() {
        if !io_set(IOPRIO_WHO_PROCESS, tid(), io_prio(IOPRIO_CLASS_BE, 0)) {
            warn_once("io", &std::io::Error::last_os_error().to_string());
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::warn_once;

    pub fn demote() {
        let rc =
            unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        if rc != 0 {
            warn_once("cpu", &std::io::Error::last_os_error().to_string());
        }
    }

    pub fn restore() {
        let rc =
            unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_DEFAULT, 0) };
        if rc != 0 {
            warn_once("cpu", &std::io::Error::last_os_error().to_string());
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::warn_once;

    const THREAD_PRIORITY_NORMAL: i32 = 0;
    const THREAD_PRIORITY_LOWEST: i32 = -2;

    extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn SetThreadPriority(hThread: *mut core::ffi::c_void, nPriority: i32) -> i32;
    }

    fn set(level: i32) -> bool {
        // Safety: GetCurrentThread returns a pseudo-handle for the calling
        // thread, which is the only handle SetThreadPriority accepts here.
        unsafe { SetThreadPriority(GetCurrentThread(), level) != 0 }
    }

    pub fn demote() {
        if !set(THREAD_PRIORITY_LOWEST) {
            warn_once("cpu", &format!("win32 error {}", std::process::id()));
        }
    }

    pub fn restore() {
        if !set(THREAD_PRIORITY_NORMAL) {
            warn_once("cpu", &format!("win32 error {}", std::process::id()));
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    pub fn demote() {}
    pub fn restore() {}
}

/// Denote the current thread (CPU and I/O) for background maintenance.
pub fn demote_background() {
    imp::demote();
}

/// Restore normal priority for the current thread after maintenance work.
pub fn restore_default() {
    imp::restore();
}
/// Restores normal priority when the guard drops, including on panic unwind.
struct BackgroundGuard;

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        restore_default();
    }
}

/// Run `f` with the current thread demoted to background priority, restoring
/// the previous priority afterwards — even if `f` panics (the executor wraps
/// jobs in `catch_unwind`, so the worker thread must not remain demoted).
pub fn with_background<R>(f: impl FnOnce() -> R) -> R {
    demote_background();
    let _guard = BackgroundGuard;
    f()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::imp::{
        io_prio, tid, IOPRIO_CLASS_BE, IOPRIO_CLASS_IDLE, IOPRIO_WHO_PROCESS, SCHED_IDLE,
        SCHED_OTHER,
    };
    use super::{demote_background, restore_default, with_background};
    use libc::{c_int, syscall};

    fn sched_policy() -> c_int {
        unsafe { libc::sched_getscheduler(0) }
    }

    fn io_priority() -> c_int {
        unsafe { syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, tid()) as c_int }
    }

    #[test]
    fn demote_and_restore_changes_scheduler_and_io_priority() {
        assert_eq!(
            sched_policy(),
            SCHED_OTHER,
            "test precondition: thread starts in SCHED_OTHER (policy codes may vary; SCHED_OTHER=0)"
        );

        demote_background();

        assert_eq!(
            sched_policy(),
            SCHED_IDLE,
            "demote moves thread to SCHED_IDLE"
        );
        assert_eq!(
            io_priority() & !0x7f,
            io_prio(IOPRIO_CLASS_IDLE, 0) & !0x7f,
            "demote moves thread to IOPRIO_CLASS_IDLE"
        );

        restore_default();

        assert_eq!(
            sched_policy(),
            SCHED_OTHER,
            "restore moves thread back to SCHED_OTHER"
        );
        assert_eq!(
            io_priority() & !0x7f,
            io_prio(IOPRIO_CLASS_BE, 0) & !0x7f,
            "restore moves thread back to IOPRIO_CLASS_BE"
        );
    }

    #[test]
    fn with_background_restores_after_closure() {
        with_background(|| {
            assert_eq!(
                sched_policy(),
                SCHED_IDLE,
                "inside background, thread is idle"
            );
        });
        assert_eq!(
            sched_policy(),
            SCHED_OTHER,
            "after background closure, thread is back to normal"
        );
    }
}
