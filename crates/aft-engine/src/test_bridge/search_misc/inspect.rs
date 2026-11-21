use std::path::{Path, PathBuf};

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
    fn non_expiring_metrics_wait_for_test(&self) -> MetricsWaitForTest;
    fn set_dead_code_waiting_for_callgraph_for_test(&self, waiting: bool);
    fn hold_in_flight_lock_for_test(&self, while_held: impl FnOnce());
    fn set_tier2_in_flight_for_test(&self, category: InspectCategory, in_flight: bool);
}

impl InspectManagerTest for InspectManager {
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
