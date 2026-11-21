use std::path::Path;

use crate::inspect::oxc_engine::OxcEngineResult;
use crate::inspect::{
    self, InspectCache, InspectCategory, InspectJob, InspectManager, InspectResult,
};

pub use crate::inspect::manager::{
    filter_payload_for_scope_for_test, InspectBuilderState, MetricsWaitForTest,
};
pub use crate::inspect::scanners::todos::{
    outcomes_for_test, todos_job, uncached_aggregate_for_test,
};

pub const TIER2_CONTRIBUTION_CACHE_VERSION: u32 = inspect::cache::TIER2_CONTRIBUTION_CACHE_VERSION;
pub const TODOS_NO_INDEX_STORE: &str = inspect::data_sources::TODOS_NO_INDEX_STORE;

pub fn contribution_set_hash_for_version_for_test(
    cache: &InspectCache,
    category: InspectCategory,
    config: &crate::config::Config,
    version: u32,
) -> Result<String, inspect::InspectCacheError> {
    cache.contribution_set_hash_for_version_for_test(category, config, version)
}

pub fn run_unused_exports_scan_with_oxc(
    job: &InspectJob,
    graph: Option<&OxcEngineResult>,
) -> InspectResult {
    inspect::scanners::unused_exports::run_unused_exports_scan_with_oxc(job, graph)
}

pub fn run_cycles_scan_with_oxc(
    job: &InspectJob,
    graph: Option<&OxcEngineResult>,
) -> InspectResult {
    inspect::scanners::cycles::run_cycles_scan_with_oxc(job, graph)
}

pub fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    inspect::scanners::dead_code::callgraph_unavailable_aggregate(scanned_files)
}

pub fn note_withheld_root(root: &Path) {
    inspect::data_sources::note_root(root, inspect::data_sources::RootKind::Withheld)
}

pub trait InspectManagerTest {
    fn tier2_builder_state(&self, category: InspectCategory) -> InspectBuilderState;
    fn dead_code_build_is_pending(&self) -> bool;
    fn set_cold_build_limiter(
        &self,
        limiter: std::sync::Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    );
    fn non_expiring_metrics_wait_for_test(&self) -> MetricsWaitForTest;
    fn set_dead_code_waiting_for_callgraph_for_test(&self, waiting: bool);
    fn hold_in_flight_lock_for_test(&self, while_held: impl FnOnce());
    fn set_tier2_in_flight_for_test(&self, category: InspectCategory, in_flight: bool);
}

impl InspectManagerTest for InspectManager {
    fn tier2_builder_state(&self, category: InspectCategory) -> InspectBuilderState {
        self.tier2_builder_state(category)
    }
    fn dead_code_build_is_pending(&self) -> bool {
        self.dead_code_build_is_pending()
    }

    fn set_cold_build_limiter(
        &self,
        limiter: std::sync::Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    ) {
        self.set_cold_build_limiter(limiter);
    }
    fn non_expiring_metrics_wait_for_test(&self) -> MetricsWaitForTest {
        self.non_expiring_metrics_wait_for_test()
    }

    fn set_dead_code_waiting_for_callgraph_for_test(&self, waiting: bool) {
        self.set_dead_code_waiting_for_callgraph_for_test(waiting);
    }

    fn hold_in_flight_lock_for_test(&self, while_held: impl FnOnce()) {
        self.hold_in_flight_lock_for_test(while_held);
    }

    fn set_tier2_in_flight_for_test(&self, category: InspectCategory, in_flight: bool) {
        self.set_tier2_in_flight_for_test(category, in_flight);
    }
}

pub trait InspectCacheTest {
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, inspect::InspectCacheError>;
    fn store_tier2_aggregate(
        &self,
        key: inspect::JobKey,
        contribution_set_hash: &str,
        aggregate: serde_json::Value,
    ) -> Result<(), inspect::InspectCacheError>;
}

impl InspectCacheTest for InspectCache {
    fn latest_aggregate_any_hash(
        &self,
        category: InspectCategory,
    ) -> Result<Option<serde_json::Value>, inspect::InspectCacheError> {
        self.latest_aggregate_any_hash(category)
    }

    fn store_tier2_aggregate(
        &self,
        key: inspect::JobKey,
        contribution_set_hash: &str,
        aggregate: serde_json::Value,
    ) -> Result<(), inspect::InspectCacheError> {
        self.store_tier2_aggregate(key, contribution_set_hash, aggregate)
    }
}
