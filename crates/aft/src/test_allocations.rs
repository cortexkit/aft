use mimalloc::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;

struct CountingAllocator(MiMalloc);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { self.0.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator(MiMalloc);

fn record_allocation() {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
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
    COUNTING.with(|counting| counting.set(true));
    let guard = CountingGuard;
    let result = operation();
    drop(guard);
    let count = ALLOCATION_COUNT.with(Cell::get);
    (result, count)
}
