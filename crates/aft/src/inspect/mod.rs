pub mod cache;
pub(crate) mod diagnostics_category;
#[doc(hidden)]
pub mod dispatch;
mod entry_points;
mod frameworks;
pub mod freshness;
mod generated;
pub(crate) use generated::is_generated_file;
pub mod job;
mod manager;
pub mod oxc_engine;
pub mod phase_log;
pub mod scanners;
pub mod tier2_scheduler;

pub use cache::{ContributionRecord, InspectCache, InspectCacheError};
pub use diagnostics_category::force_scoped_diagnostic_coverage_for_test;
pub use dispatch::{
    inspect_pool_size_for_test, inspect_pool_thread_count_for_test, DispatchHandles, InspectWorker,
};
pub(crate) use entry_points::resolve_entry_points;
pub use freshness::{contribution_is_fresh, verify_contribution_file, ContributionFreshness};
pub use job::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory,
    InspectJob, InspectResult, InspectScanSuccess, InspectSnapshot, InspectTier, JobKey,
    JobOutcome, JobScope, JobStatus, WorkerCtx,
};
#[cfg(test)]
pub(crate) use manager::InspectBuilderState;
pub use manager::{InspectManager, Tier2RunSubmission, Tier2RunSubmissionError};
pub use phase_log::{
    format_wait_text, inspect_phase_log_for_request, InspectPhaseEntry, InspectPhaseId,
    InspectPhaseLog, InspectPhaseLogSnapshot,
};
pub use tier2_scheduler::{Tier2DispatchBlock, Tier2RefreshScheduler, Tier2TriggerReason};
