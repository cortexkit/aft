impl slot::BuildInputs {
    pub fn new_for_test(
        search: SearchConfig,
        semantic: crate::config::SemanticBackendConfig,
        under_subc: bool,
        cache_unavailable: bool,
        cache_lease: Option<std::sync::Arc<crate::root_cache::CacheLease>>,
        cache_epoch: u64,
    ) -> Self {
        Self {
            search,
            semantic,
            under_subc,
            cache_unavailable,
            cache_lease,
            cache_epoch,
        }
    }
}
