//! Opt-in measurements for the real-checkout inspect regression harness.

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

thread_local! {
    static ENTRIES: Cell<usize> = const { Cell::new(0) };
}

static RECORDS: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

pub(crate) fn entry() {
    if enabled() {
        ENTRIES.with(|entries| entries.set(entries.get() + 1));
    }
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("AFT_INSPECT_WALK_PROFILE_ROOT").is_some())
}

pub(crate) fn cpu_ms() -> f64 {
    #[cfg(unix)]
    {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // The process CPU clock includes the inspect worker threads.
        if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr()) } == 0 {
            let time = unsafe { time.assume_init() };
            return time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0;
        }
    }
    0.0
}

pub(crate) struct Walk {
    label: &'static str,
    started: Instant,
    entries: usize,
}

impl Walk {
    pub(crate) fn start(label: &'static str) -> Self {
        Self {
            label,
            started: Instant::now(),
            entries: ENTRIES.with(Cell::get),
        }
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        if enabled() {
            RECORDS.lock().unwrap().push(serde_json::json!({
                "walk": self.label,
                "entries": ENTRIES.with(Cell::get) - self.entries,
                "ms": self.started.elapsed().as_secs_f64() * 1000.0,
            }));
        }
    }
}

pub(crate) fn take() -> Vec<serde_json::Value> {
    std::mem::take(&mut *RECORDS.lock().unwrap())
}
