use std::path::Path;

use crate::inspect::oxc_engine::OxcEngineResult;
use crate::inspect::{
    self, InspectCache, InspectCategory, InspectJob, InspectManager, InspectResult,
};

pub use crate::inspect::scanners::todos::{
    outcomes_for_test, todos_job, uncached_aggregate_for_test,
};
pub use crate::inspect::{
    filter_payload_for_scope_for_test, InspectBuilderState, MetricsWaitForTest,
};

pub use inspect::compatibility_test::{TIER2_CONTRIBUTION_CACHE_VERSION, TODOS_NO_INDEX_STORE};

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
    inspect::compatibility_test::run_unused_exports_scan_with_oxc(job, graph)
}

pub fn run_cycles_scan_with_oxc(
    job: &InspectJob,
    graph: Option<&OxcEngineResult>,
) -> InspectResult {
    inspect::compatibility_test::run_cycles_scan_with_oxc(job, graph)
}

pub fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    inspect::compatibility_test::callgraph_unavailable_aggregate(scanned_files)
}

pub fn note_withheld_root(root: &Path) {
    use aft_index::data_layer::bound_checkouts::{note_root, RootKind};
    note_root(root, RootKind::Withheld)
}

pub mod data_sources {
    pub fn set_todo_store_wait_for_test(project_root: &std::path::Path, wait: std::time::Duration) {
        crate::inspect::data_sources::set_todo_store_wait_for_test(project_root, wait);
    }
}

pub mod cache {
    pub fn aggregate_memory_walks_for_test() -> usize {
        crate::inspect::cache::aggregate_memory_walks_for_test()
    }
}
