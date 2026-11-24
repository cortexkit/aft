#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn with_lane_pause_clock<T>(
    start_ms: u64,
    run: impl FnOnce(std::sync::Arc<std::sync::atomic::AtomicU64>) -> T,
) -> T {
    struct Restore(Option<std::sync::Arc<std::sync::atomic::AtomicU64>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LANE_PAUSE_TEST_CLOCK.with(|clock| clock.replace(self.0.take()));
        }
    }
    let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(start_ms));
    let _restore =
        Restore(LANE_PAUSE_TEST_CLOCK.with(|current| current.replace(Some(clock.clone()))));
    run(clock)
}
