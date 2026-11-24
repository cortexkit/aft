impl WatcherExclusionPlanningSnapshot {
    pub fn new(
        matcher_generation: u64,
        attempts: u32,
        attempt_limit: u32,
        status: &'static str,
        last_failure: Option<&'static str>,
    ) -> Self {
        Self {
            matcher_generation,
            attempts,
            attempt_limit,
            status,
            last_failure,
        }
    }
}

impl WatcherOverflowPrefix {
    pub fn new(prefix: String, count: u64) -> Self {
        Self { prefix, count }
    }
}
use super::counters::{WatcherExclusionPlanningSnapshot, WatcherOverflowPrefix};
