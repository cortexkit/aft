//! Edit entry points that retain the engine's composition API.

pub use aft_edit::edit::*;

use crate::backup::{latency, BackupSkippedReason, BackupStore, CapturedRegularFile};
use crate::context::AppContext;
use crate::error::AftError;
use ports::{BackupSnapshots, SnapshotRequest, SnapshotResult};
use std::path::Path;

#[doc(hidden)]
pub fn auto_backup_observed(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
    observation: &EditObservation,
) -> SnapshotResult {
    aft_edit::edit::auto_backup_observed(ctx, session, path, description, op_id, observation)
}

pub fn auto_backup(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
) -> SnapshotResult {
    aft_edit::edit::auto_backup(ctx, session, path, description, op_id)
}

pub fn attach_backup_skipped_reason(
    result: &mut serde_json::Value,
    ctx: &AppContext,
    session: &str,
    op_id: &str,
    path: Option<&Path>,
) {
    aft_edit::edit::attach_backup_skipped_reason(result, ctx, session, op_id, path);
}

#[doc(hidden)]
pub fn auto_backup_from_capture(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
    capture: &CapturedRegularFile,
) -> SnapshotResult {
    aft_edit::edit::auto_backup_from_capture(ctx, session, path, description, op_id, capture)
}

fn snapshot_locked(store: &mut BackupStore, request: SnapshotRequest<'_>) -> SnapshotResult {
    let SnapshotRequest {
        session,
        path,
        description,
        op_id,
        capture,
    } = request;
    if let Some(capture) = capture {
        store.snapshot_with_op_from_capture(session, path, description, op_id, capture)
    } else {
        store.snapshot_with_op(session, path, description, op_id)
    }
}

impl BackupSnapshots for AppContext {
    fn snapshot(&self, request: SnapshotRequest<'_>) -> SnapshotResult {
        let mut store = {
            let _wait = latency::time(latency::Part::StoreLock);
            self.backup().lock()
        };
        snapshot_locked(&mut store, request)
    }

    fn snapshot_observed(
        &self,
        request: SnapshotRequest<'_>,
        recheck: &dyn Fn() -> Result<(), AftError>,
    ) -> Result<SnapshotResult, AftError> {
        #[cfg(test)]
        BEFORE_OBSERVED_STORE_LOCK.with(|slot| {
            if let Some(hook) = slot.take() {
                hook();
            }
        });
        let mut store = {
            let _wait = latency::time(latency::Part::StoreLock);
            self.backup().lock()
        };
        // The live file may have changed while the backup store was busy.
        // Check it against the original observation while the edit lock is held.
        recheck()?;
        Ok(snapshot_locked(&mut store, request))
    }

    fn skipped_reason(
        &self,
        session: &str,
        op_id: &str,
        path: Option<&Path>,
    ) -> Option<BackupSkippedReason> {
        self.backup()
            .lock()
            .skipped_reason_for_operation(session, op_id, path)
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_OBSERVED_STORE_LOCK: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}
