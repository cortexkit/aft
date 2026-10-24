use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static PROCESS_COUNTING: AtomicBool = AtomicBool::new(false);
static PROCESS_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static PROCESS_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static ALLOCATION_BYTES: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

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
