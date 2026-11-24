#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static MEMORY_WALK_CLOCK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Override only this thread's memory-walk clock for a synchronous test refresh.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn with_memory_walk_clock_for_test<T>(now: Instant, refresh: impl FnOnce() -> T) -> T {
    struct Reset(Option<Instant>);
    impl Drop for Reset {
        fn drop(&mut self) {
            MEMORY_WALK_CLOCK.with(|clock| clock.set(self.0));
        }
    }
    let _reset = Reset(MEMORY_WALK_CLOCK.with(|clock| clock.replace(Some(now))));
    refresh()
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn memory_walk_now_for_test() -> Instant {
    MEMORY_WALK_CLOCK.with(|clock| clock.get().unwrap_or_else(Instant::now))
}
