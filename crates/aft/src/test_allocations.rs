use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static PROCESS_COUNTING: AtomicBool = AtomicBool::new(false);
static PROCESS_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static PROCESS_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static ALLOCATION_BYTES: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_live(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        let next = unsafe { System.realloc(ptr, layout, new_size) };
        if !next.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            record_live(new_size);
        }
        next
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn record_live(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

/// Requested Rust heap bytes, excluding allocator retention and native allocations.
/// Run an isolated test process when comparing these process-wide counters.
pub(crate) fn heap_marker(prefix: &str, phase: &str) {
    if std::env::var_os("AFT_VIEW_PROFILE").is_some() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let rss = crate::memory::rss_bytes();
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let rss: Option<u64> = None;
        #[cfg(target_os = "linux")]
        let rss_peak = rss_high_water_bytes();
        #[cfg(not(target_os = "linux"))]
        let rss_peak: Option<u64> = None;
        eprintln!(
            "view_heap {prefix}.{phase} live={} peak={} rss={rss:?} rss_peak={rss_peak:?}",
            LIVE_BYTES.load(Ordering::Relaxed),
            PEAK_BYTES.load(Ordering::Relaxed),
        );
    }
}

pub(crate) fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
pub(crate) fn rss_high_water_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("VmHWM:") {
            return fields.next()?.parse::<u64>().ok()?.checked_mul(1024);
        }
    }
    None
}

fn record_allocation(bytes: usize) {
    if PROCESS_COUNTING.load(Ordering::Relaxed) {
        PROCESS_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        PROCESS_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
        let _ = ALLOCATION_BYTES.try_with(|total| total.set(total.get().saturating_add(bytes)));
    }
}

pub(crate) fn count<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    struct CountingGuard;

    impl Drop for CountingGuard {
        fn drop(&mut self) {
            COUNTING.with(|counting| counting.set(false));
        }
    }

    ALLOCATION_COUNT.with(|count| count.set(0));
    ALLOCATION_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let guard = CountingGuard;
    let result = operation();
    drop(guard);
    let count = ALLOCATION_COUNT.with(Cell::get);
    (result, count)
}

/// Total heap allocation volume on this thread, including reallocations.
/// Unlike allocation counts, this also catches one oversized reservation.
pub(crate) fn count_bytes<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    let (result, _) = count(operation);
    (result, ALLOCATION_BYTES.with(Cell::get))
}

/// Read the current thread's counters without starting a nested counting scope.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn snapshot() -> (usize, usize) {
    (
        ALLOCATION_COUNT.with(Cell::get),
        ALLOCATION_BYTES.with(Cell::get),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod profile {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    thread_local! {
        pub(crate) static STEPS: RefCell<Option<BTreeMap<&'static str, (u64, usize, usize)>>> = const { RefCell::new(None) };
        pub(crate) static TRACE_PHASES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static NEXT_TRACE_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    pub(crate) fn cpu_ns() -> u64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
    }

    pub(crate) struct Timer(Option<(u64, (usize, usize))>, Option<u64>);

    impl Timer {
        pub(crate) fn new() -> Self {
            let sample = STEPS.with(|steps| {
                steps
                    .borrow()
                    .is_some()
                    .then(|| (cpu_ns(), super::snapshot()))
            });
            let trace = (sample.is_some() && TRACE_PHASES.with(std::cell::Cell::get)).then(|| {
                let id = NEXT_TRACE_ID.with(|next| {
                    let id = next.get();
                    next.set(id + 1);
                    id
                });
                eprintln!("AFT_PHASE|start|{id}");
                id
            });
            Self(sample, trace)
        }

        pub(crate) fn mark(&mut self, step: &'static str) {
            if let Some((previous, allocations)) = self.0 {
                let now = cpu_ns();
                let current = super::snapshot();
                STEPS.with(|steps| {
                    let mut steps = steps.borrow_mut();
                    let entry = steps.as_mut().unwrap().entry(step).or_default();
                    entry.0 += now - previous;
                    entry.1 += current.0 - allocations.0;
                    entry.2 += current.1 - allocations.1;
                });
                if let Some(id) = self.1 {
                    eprintln!("AFT_PHASE|mark|{id}|{step}");
                }
                self.0 = Some((cpu_ns(), super::snapshot()));
            }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            if let Some(id) = self.1 {
                eprintln!("AFT_PHASE|end|{id}");
            }
        }
    }
}

/// Includes Rayon worker allocations; only use in isolated, single-test runs.
pub(crate) fn count_process<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            PROCESS_COUNTING.store(false, Ordering::SeqCst);
        }
    }
    PROCESS_ALLOCATIONS.store(0, Ordering::SeqCst);
    PROCESS_BYTES.store(0, Ordering::SeqCst);
    assert!(!PROCESS_COUNTING.swap(true, Ordering::SeqCst));
    let guard = Guard;
    let result = operation();
    drop(guard);
    (
        result,
        PROCESS_ALLOCATIONS.load(Ordering::SeqCst),
        PROCESS_BYTES.load(Ordering::SeqCst),
    )
}
