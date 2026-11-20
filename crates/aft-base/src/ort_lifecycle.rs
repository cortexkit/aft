//! Keeps native ONNX Runtime work out of process exit.
//!
//! ONNX Runtime is a C++ library with process-global static state (its
//! operator-schema registry, logging manager and environment). When the
//! process exits, the C runtime destroys those statics (`__cxa_finalize`). If
//! a background thread is still inside ORT at that moment — creating the
//! environment, loading a model or running inference — it reads freed memory
//! and the process dies with SIGSEGV at exit.
//!
//! Every native ORT section therefore runs under an [`OrtCallGuard`] from
//! [`enter`]. Before exiting, the process calls [`close_and_wait`]: it refuses
//! any new ORT section and waits for the in-flight ones to finish, so the
//! static destructors never run underneath active ORT code. If a section does
//! not finish within the budget, the process ends without running those
//! destructors (see [`exit_process`] and [`quiesce_before_return`]).

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Default)]
struct GateState {
    active: usize,
    closed: bool,
}

/// Counts in-flight ORT sections and, once closed, refuses new ones.
struct Gate {
    state: Mutex<GateState>,
    idle: Condvar,
}

impl Gate {
    const fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                active: 0,
                closed: false,
            }),
            idle: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn enter(&'static self) -> Option<OrtCallGuard> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(OrtCallGuard { gate: self })
    }

    fn leave(&self) {
        let mut state = self.lock();
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            self.idle.notify_all();
        }
    }

    fn close_and_wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        state.closed = true;
        while state.active > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .idle
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

static GATE: Gate = Gate::new();

/// Proof that the current thread may call into ONNX Runtime. Dropping it
/// marks the native section finished.
#[must_use = "the ORT section is only protected while the guard is alive"]
pub struct OrtCallGuard {
    gate: &'static Gate,
}

impl Drop for OrtCallGuard {
    fn drop(&mut self) {
        self.gate.leave();
    }
}

/// Enter a native ORT section. Returns `None` once the process has started
/// exiting; the caller must then skip the ORT call.
pub fn enter() -> Option<OrtCallGuard> {
    GATE.enter()
}

/// Refuse new ORT sections and wait up to `timeout` for in-flight ones.
/// Returns `true` when no ORT code is running, so a normal exit is safe.
pub fn close_and_wait(timeout: Duration) -> bool {
    GATE.close_and_wait(timeout)
}

/// Number of in-flight ORT work sections that make native teardown unsafe.
pub fn active_sections() -> usize {
    GATE.lock().active
}

/// End a drained daemon module without running native static destructors.
/// The caller must flush its final log line before calling this function.
pub fn exit_without_native_teardown(code: i32) -> ! {
    immediate_exit(code)
}

/// How long exit waits for in-flight ORT work. Environment creation and a
/// MiniLM session load take well under a second; a single inference batch is
/// bounded by the embedder's per-inference budget.
pub const EXIT_WAIT: Duration = Duration::from_secs(10);

/// Prepare a normal return from `main`: refuse new ORT work and wait for the
/// in-flight sections. If they do not finish in time, end the process now
/// with `code`, skipping native teardown; otherwise return so ordinary Rust
/// cleanup runs.
pub fn quiesce_before_return(code: i32) {
    quiesce_before_return_within(code, EXIT_WAIT)
}

/// [`quiesce_before_return`] with a caller-chosen wait, for exits that run
/// against a deadline. Ending without native teardown is always safe here;
/// only the wait for in-flight sections is traded away.
pub fn quiesce_before_return_within(code: i32, timeout: Duration) {
    if close_and_wait(timeout) {
        return;
    }
    skip_native_teardown_after(code, timeout)
}

/// End the process with `code` without letting ORT's static destructors run
/// underneath an in-flight ORT section.
///
/// Callers must flush anything they need (logs, stdout) first. When the gate
/// drains in time the normal `exit` path runs; otherwise the process exits
/// immediately without running C/C++ exit handlers.
pub fn exit_process(code: i32) -> ! {
    if close_and_wait(EXIT_WAIT) {
        std::process::exit(code);
    }
    skip_native_teardown(code)
}

fn skip_native_teardown(code: i32) -> ! {
    skip_native_teardown_after(code, EXIT_WAIT)
}

fn skip_native_teardown_after(code: i32, waited: Duration) -> ! {
    crate::slog_warn!(
        "ONNX Runtime work still in flight after {} ms; exiting without native teardown",
        waited.as_millis()
    );
    crate::logging::flush_durable_log(Duration::from_millis(500));
    immediate_exit(code)
}

#[cfg(unix)]
fn immediate_exit(code: i32) -> ! {
    // SAFETY: `_exit` terminates the process without running atexit handlers
    // or C++ static destructors, which is exactly what is needed while another
    // thread may still be inside ONNX Runtime.
    unsafe { libc::_exit(code) }
}

#[cfg(windows)]
fn immediate_exit(code: i32) -> ! {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn TerminateProcess(process: *mut std::ffi::c_void, code: u32) -> i32;
    }
    // SAFETY: the current-process handle is always valid. TerminateProcess
    // bypasses both CRT atexit and DLL detach callbacks, which could otherwise
    // destroy ORT's native statics while another thread is using them.
    unsafe { TerminateProcess(GetCurrentProcess(), code as u32) };
    // Successful self-termination does not return; abort without cleanup if
    // the operating system unexpectedly refuses it.
    std::process::abort()
}

#[cfg(not(any(unix, windows)))]
fn immediate_exit(code: i32) -> ! {
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_waits_for_active_sections_and_refuses_new_ones() {
        // A private gate, so closing it cannot affect other tests in this
        // process that load the embedder.
        static TEST_GATE: Gate = Gate::new();
        let guard = TEST_GATE.enter().expect("gate starts open");
        let waiter = std::thread::spawn(|| TEST_GATE.close_and_wait(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "close must wait for the active section"
        );
        drop(guard);
        assert!(
            waiter.join().unwrap(),
            "close completes once the section ends"
        );
        assert!(
            TEST_GATE.enter().is_none(),
            "no ORT section may start after close"
        );
    }

    #[test]
    fn close_reports_a_section_that_outlives_the_budget() {
        static TEST_GATE: Gate = Gate::new();
        let _guard = TEST_GATE.enter().expect("gate starts open");
        assert!(!TEST_GATE.close_and_wait(Duration::from_millis(20)));
    }
}
