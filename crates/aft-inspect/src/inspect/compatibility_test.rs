use std::path::Path;

use super::oxc_engine::OxcEngineResult;
use super::scanners::{cycles, dead_code, source_text, unused_exports};
use super::{InspectJob, InspectResult};

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
