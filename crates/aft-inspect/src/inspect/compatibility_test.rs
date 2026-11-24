use std::path::Path;

use super::oxc_engine::OxcEngineResult;
use super::scanners::{cycles, dead_code, source_text, unused_exports};
use crate::inspect;

use super::cache::{InspectCache, InspectCacheError, Tier2ContributionUpdates};
use super::manager::{InspectBuilderState, InspectManager, MetricsWaitForTest};
use super::{FileContribution, InspectCategory, InspectJob, InspectResult};

pub const TIER2_CONTRIBUTION_CACHE_VERSION: u32 = super::cache::TIER2_CONTRIBUTION_CACHE_VERSION;
pub const TODOS_NO_INDEX_STORE: &str = super::data_sources::TODOS_NO_INDEX_STORE;

pub fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    dead_code::callgraph_unavailable_aggregate(scanned_files)
}

pub fn run_unused_exports_scan_with_oxc(
    job: &InspectJob,
    graph: Option<&OxcEngineResult>,
) -> InspectResult {
    unused_exports::run_unused_exports_scan_with_oxc(job, graph)
}

pub fn run_cycles_scan_with_oxc(
    job: &InspectJob,
    graph: Option<&OxcEngineResult>,
) -> InspectResult {
    cycles::run_cycles_scan_with_oxc(job, graph)
}

pub fn read_source_is_disclosed(path: &Path) -> bool {
    source_text::read_source_text(path).is_ok()
}

pub fn apply_contribution_upserts(
    cache: &InspectCache,
    category: InspectCategory,
    upserts: Vec<FileContribution>,
    config: &crate::config::Config,
) -> Result<String, InspectCacheError> {
    cache
        .apply_contribution_updates_for_config(
            category,
            Tier2ContributionUpdates {
                upserts,
                ..Default::default()
            },
            config,
        )
        .map(|(hash, _)| hash)
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
    fn legacy_estimated_memory_for_test(&self) -> crate::memory::MemoryEstimate;
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
    fn legacy_estimated_memory_for_test(&self) -> crate::memory::MemoryEstimate {
        self.legacy_estimated_memory_for_test()
    }
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
