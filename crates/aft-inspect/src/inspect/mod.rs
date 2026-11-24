pub mod cache;
#[doc(hidden)]
pub mod diagnostics_category;
#[doc(hidden)]
pub mod diagnostics_host;
#[doc(hidden)]
pub mod dispatch;
mod entry_points;
#[doc(hidden)]
pub use entry_points::InspectEntryPoints;
#[cfg(feature = "test-hooks")]
mod exactness_port;
mod frameworks;
pub mod freshness;
pub(crate) use crate::file_kind::generated;
#[doc(hidden)]
pub mod inventory;
pub(crate) mod scoped_diagnostics_sweep;
#[doc(hidden)]
pub use generated::relative_shape_is_generated;
pub mod job;
pub(crate) mod manager;
pub mod oxc_engine;
pub mod phase_log;
pub mod scanners;
pub mod tier2_scheduler;
#[cfg(test)]
pub(crate) mod walk_profile;

pub use cache::{ContributionRecord, InspectCache, InspectCacheError};
pub use diagnostics_category::force_scoped_diagnostic_coverage_for_test;
pub use dispatch::{
    inspect_pool_size_for_test, inspect_pool_thread_count_for_test, InspectDispatcher,
    InspectWorker,
};
pub(crate) use entry_points::{resolve_entry_points, resolve_project_roles};
pub use freshness::{contribution_is_fresh, verify_contribution_file, ContributionFreshness};
pub use job::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory,
    InspectJob, InspectResult, InspectScanSuccess, InspectSnapshot, InspectTier, JobKey,
    JobOutcome, JobScope, JobStatus, WorkerCtx,
};
pub(crate) use manager::scope_files as tier2_scope_files;
#[cfg(test)]
pub(crate) use manager::InspectBuilderState;
pub use manager::InspectManager;
pub use phase_log::{
    format_wait_text, inspect_phase_log_for_request, InspectPhaseEntry, InspectPhaseId,
    InspectPhaseLog, InspectPhaseLogSnapshot,
};
pub use tier2_scheduler::{
    Tier2DispatchBlock, Tier2ExternalGates, Tier2RefreshScheduler, Tier2TriggerReason,
};

#[doc(hidden)]
pub mod data_sources;
pub mod dead_code_gate;

/// Formats `2940` as `2,940`.
#[doc(hidden)]
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
