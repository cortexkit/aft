impl crate::data_layer::checkout::RefreshCounts {
    pub fn new_for_test(refreshes: u64, guard_fallbacks: u64, precheck_fallbacks: u64) -> Self {
        Self {
            refreshes,
            guard_fallbacks,
            precheck_fallbacks,
        }
    }
}
