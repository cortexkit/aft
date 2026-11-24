//! Capabilities supplied by the composition layer to the edit pipeline.

use crate::backup::{BackupSkippedReason, CapturedRegularFile};
use crate::error::AftError;
use std::path::Path;

pub type SnapshotResult = Result<Option<String>, AftError>;

pub struct SnapshotRequest<'a> {
    pub session: &'a str,
    pub path: &'a Path,
    pub description: &'a str,
    pub op_id: Option<&'a str>,
    pub capture: Option<&'a CapturedRegularFile>,
}

pub trait BackupSnapshots {
    fn snapshot(&self, request: SnapshotRequest<'_>) -> SnapshotResult;

    /// Recheck the observed file under the snapshot lock before persisting it.
    /// The outer error is a stale observation, not a snapshot attempt.
    fn snapshot_observed(
        &self,
        request: SnapshotRequest<'_>,
        recheck: &dyn Fn() -> Result<(), AftError>,
    ) -> Result<SnapshotResult, AftError>;

    fn skipped_reason(
        &self,
        session: &str,
        op_id: &str,
        path: Option<&Path>,
    ) -> Option<BackupSkippedReason>;
}

pub trait PostWriteDiagnostics: std::fmt::Debug {
    /// Append the diagnostics and completeness fields for an edit reply.
    fn append_to(&self, result: &mut serde_json::Value);
}
