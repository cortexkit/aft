//! Callgraph store maintenance demand that outlives a single request.
//!
//! Two kinds of demand live here. A forced full rebuild is requested when the
//! store itself no longer matches the configuration (a different storage
//! directory or topology) or when the corpus membership rules changed; it
//! carries the reason that asked for it so the eventual rebuild can say why. A
//! reconcile is requested when watcher events were lost (an overflow, or an
//! interval with no watcher at all): the store is still the right store, it
//! only missed some edits, so it is compared with the disk and the differing
//! files go through the ordinary incremental refresh.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::callgraph_store::{CallGraphStore, DiskReconcileReport};

/// Walked files a single reconcile compares before giving up. A reconcile
/// hashes every size-matching file it visits, so the bound keeps a pathological
/// tree from turning lost events into an unbounded scan; past it, the store is
/// rebuilt instead.
pub(crate) const RECONCILE_MAX_EXAMINED_FILES: usize = 200_000;

/// A reconcile whose changed set is at least this large and more than half of
/// the stored corpus is answered with a rebuild: refreshing most of a corpus
/// file by file writes more than building it once.
const RECONCILE_REBUILD_MIN_CHANGED: usize = 1_000;

/// Refreshes the store must apply before it answers queries as current: a
/// reconcile after lost watcher events, or the importers of a manifest whose
/// resolution fields changed. Each is represented by a guard that the refresh
/// batch keeps alive until the worker has settled it.
#[derive(Debug, Default)]
pub(crate) struct CallgraphCatchUp {
    outstanding: AtomicUsize,
}

impl CallgraphCatchUp {
    pub(crate) fn begin(self: &Arc<Self>) -> Arc<CallgraphCatchUpGuard> {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        Arc::new(CallgraphCatchUpGuard(Arc::clone(self)))
    }

    pub(crate) fn outstanding(&self) -> bool {
        self.outstanding.load(Ordering::SeqCst) > 0
    }
}

/// Marks one catch-up outstanding until dropped.
#[derive(Debug)]
pub(crate) struct CallgraphCatchUpGuard(Arc<CallgraphCatchUp>);

impl Drop for CallgraphCatchUpGuard {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A pending forced rebuild: a monotonically increasing request counter, the
/// highest request a published build has satisfied, and the reason given by
/// the latest request.
#[derive(Debug, Default)]
pub(crate) struct CallgraphForceDemand {
    requested: AtomicU64,
    fulfilled: AtomicU64,
    reason: parking_lot::Mutex<Option<String>>,
}

impl CallgraphForceDemand {
    /// Request a forced rebuild and return its token.
    pub(crate) fn mark(&self, reason: &str) -> u64 {
        // The reason is recorded before the token becomes visible, so a reader
        // that observes the token also observes a reason.
        *self.reason.lock() = Some(reason.to_string());
        self.requested
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub(crate) fn pending(&self) -> Option<u64> {
        let requested = self.requested.load(Ordering::SeqCst);
        let fulfilled = self.fulfilled.load(Ordering::SeqCst);
        (requested > fulfilled).then_some(requested)
    }

    pub(crate) fn fulfill(&self, token: u64) {
        self.fulfilled.fetch_max(token, Ordering::SeqCst);
    }

    /// The reason given by the most recent request.
    pub(crate) fn reason(&self) -> String {
        self.reason
            .lock()
            .clone()
            .unwrap_or_else(|| "unrecorded".to_string())
    }
}

/// What one finished reconcile did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallgraphReconcileAction {
    /// The differing paths were handed to the incremental refresh.
    Refreshed(Vec<PathBuf>),
    /// The store already matched the disk.
    Unchanged,
    /// No store generation is published yet; the first build walks the disk.
    NoPublishedStore,
    /// The comparison could not be completed or the change was too large, so
    /// a full rebuild was requested with this reason.
    ForcedRebuild(String),
    /// The comparison met transient database lock contention. The request
    /// stays pending and is retried by the next store access, instead of
    /// discarding a still-valid store for a cold rebuild.
    Deferred(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallgraphReconcileOutcome {
    /// Why the reconcile was requested (for example a watcher overflow).
    pub reason: String,
    pub report: Option<DiskReconcileReport>,
    pub action: CallgraphReconcileAction,
}

/// Reconcile demand for one context: the reason of a requested reconcile that
/// has not started, whether one is running, and the last finished outcome.
#[derive(Debug, Default)]
pub(crate) struct CallgraphReconcileState {
    pending: parking_lot::Mutex<Vec<String>>,
    running: AtomicBool,
    last: parking_lot::Mutex<Option<CallgraphReconcileOutcome>>,
    completed: AtomicU64,
}

impl CallgraphReconcileState {
    pub(crate) fn request(&self, reason: &str) {
        let mut pending = self.pending.lock();
        if !pending.iter().any(|known| known == reason) {
            pending.push(reason.to_string());
        }
    }

    pub(crate) fn pending_reason(&self) -> Option<String> {
        let pending = self.pending.lock();
        (!pending.is_empty()).then(|| pending.join(", "))
    }

    /// Take the pending request and mark a reconcile as running. Returns None
    /// when nothing is pending or a reconcile is already running; a request
    /// made while one runs stays pending for the next start.
    pub(crate) fn begin(self: &Arc<Self>) -> Option<CallgraphReconcileOwnership> {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            return None;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        Some(CallgraphReconcileOwnership {
            state: Arc::clone(self),
            reasons: std::mem::take(&mut *pending),
            finished: false,
        })
    }

    pub(crate) fn clear_pending(&self) {
        self.pending.lock().clear();
    }

    fn finish(&self, outcome: CallgraphReconcileOutcome) {
        *self.last.lock() = Some(outcome);
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn last(&self) -> Option<CallgraphReconcileOutcome> {
        self.last.lock().clone()
    }

    pub(crate) fn completed(&self) -> u64 {
        self.completed.load(Ordering::SeqCst)
    }
}

/// Owns the reconciliation running flag before thread spawn, so a failed spawn
/// drops the ownership and allows another job to service the pending demand.
pub(crate) struct CallgraphReconcileOwnership {
    state: Arc<CallgraphReconcileState>,
    reasons: Vec<String>,
    finished: bool,
}

impl CallgraphReconcileOwnership {
    fn finish(&mut self, outcome: CallgraphReconcileOutcome) {
        self.state.finish(outcome);
        self.finished = true;
    }
}

impl Drop for CallgraphReconcileOwnership {
    fn drop(&mut self) {
        // Spawn failure and unwinding must preserve demand, not silently lose
        // the watcher gap. Restore it before allowing another job to start.
        if !self.finished {
            for reason in &self.reasons {
                self.state.request(reason);
            }
        }
        self.state.running.store(false, Ordering::SeqCst);
    }
}

/// Everything a reconcile needs, captured on the requesting thread so the
/// comparison can run off it.
pub(crate) struct CallgraphReconcileJob {
    pub(crate) ownership: CallgraphReconcileOwnership,
    pub(crate) callgraph_dir: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) max_examined: usize,
    pub(crate) force: Arc<CallgraphForceDemand>,
    /// Hands the changed paths to the incremental refresh for the generation
    /// that requested the reconcile.
    pub(crate) enqueue_refresh: Box<dyn FnOnce(Vec<PathBuf>) + Send>,
}

impl CallgraphReconcileJob {
    /// Compare the published store with the disk and act on the difference.
    pub(crate) fn run(self) -> CallgraphReconcileOutcome {
        let CallgraphReconcileJob {
            mut ownership,
            callgraph_dir,
            project_root,
            max_examined,
            force,
            enqueue_refresh,
        } = self;
        let reason = ownership.reasons.join(", ");
        let report = match CallGraphStore::open_readonly(callgraph_dir, project_root.clone()) {
            Ok(Some(store)) => store.reconcile_with_disk(max_examined),
            Ok(None) => {
                let outcome = CallgraphReconcileOutcome {
                    reason,
                    report: None,
                    action: CallgraphReconcileAction::NoPublishedStore,
                };
                log_outcome(&project_root, &outcome);
                ownership.finish(outcome.clone());
                return outcome;
            }
            Err(error) => Err(error),
        };
        settle_reconcile(
            ownership,
            reason,
            report,
            &project_root,
            &force,
            enqueue_refresh,
        )
    }
}

/// Act on a finished (or failed) disk comparison and record the outcome.
fn settle_reconcile(
    mut ownership: CallgraphReconcileOwnership,
    reason: String,
    report: crate::callgraph_store::Result<DiskReconcileReport>,
    project_root: &std::path::Path,
    force: &CallgraphForceDemand,
    enqueue_refresh: Box<dyn FnOnce(Vec<PathBuf>) + Send>,
) -> CallgraphReconcileOutcome {
    if let Err(error) = &report {
        if error.is_transient_lock_contention() {
            // Lock contention in either the open probe or comparison says
            // nothing about whether the stored graph
            // is valid, so keep the request pending rather than forcing a
            // cold rebuild. Dropping the unfinished ownership restores the
            // reasons, and the next store access starts another attempt.
            let outcome = CallgraphReconcileOutcome {
                reason: reason.clone(),
                report: None,
                action: CallgraphReconcileAction::Deferred(format!(
                    "{reason}: reconcile deferred ({error})"
                )),
            };
            log_outcome(project_root, &outcome);
            drop(ownership);
            return outcome;
        }
    }
    let (report, action) = match report {
        Ok(report) => {
            let action = if report.truncated {
                let fallback = format!(
                    "{reason}: reconcile stopped at its bound of {} files",
                    report.max_examined
                );
                force.mark(&fallback);
                CallgraphReconcileAction::ForcedRebuild(fallback)
            } else if report.changed_count() >= RECONCILE_REBUILD_MIN_CHANGED
                && report.changed_count().saturating_mul(2) > report.stored_files
            {
                let fallback = format!(
                    "{reason}: {} of {} stored files changed",
                    report.changed_count(),
                    report.stored_files
                );
                force.mark(&fallback);
                CallgraphReconcileAction::ForcedRebuild(fallback)
            } else if report.changed_count() == 0 {
                CallgraphReconcileAction::Unchanged
            } else {
                let paths = report.changed_paths();
                enqueue_refresh(paths.clone());
                CallgraphReconcileAction::Refreshed(paths)
            };
            (Some(report), action)
        }
        Err(error) => {
            let fallback = format!("{reason}: reconcile failed ({error})");
            force.mark(&fallback);
            (None, CallgraphReconcileAction::ForcedRebuild(fallback))
        }
    };
    let outcome = CallgraphReconcileOutcome {
        reason,
        report,
        action,
    };
    log_outcome(project_root, &outcome);
    ownership.finish(outcome.clone());
    outcome
}

fn log_outcome(project_root: &std::path::Path, outcome: &CallgraphReconcileOutcome) {
    let action = match &outcome.action {
        CallgraphReconcileAction::Refreshed(paths) => {
            format!("refresh {} changed file(s)", paths.len())
        }
        CallgraphReconcileAction::Unchanged => "none (store matches disk)".to_string(),
        CallgraphReconcileAction::NoPublishedStore => {
            "none (no published store; the first build walks the disk)".to_string()
        }
        CallgraphReconcileAction::ForcedRebuild(reason) => {
            format!("force rebuild ({reason})")
        }
        CallgraphReconcileAction::Deferred(reason) => format!("retry later ({reason})"),
    };
    match &outcome.report {
        Some(report) => crate::slog_info!(
            "callgraph reconcile after {} for {}: examined={} stored={} hashed={} created={} modified={} deleted={} truncated={} bound={}; action={}",
            outcome.reason,
            project_root.display(),
            report.examined_files,
            report.stored_files,
            report.hashed_files,
            report.created.len(),
            report.modified.len(),
            report.deleted.len(),
            report.truncated,
            report.max_examined,
            action
        ),
        None => crate::slog_info!(
            "callgraph reconcile after {} for {}: action={}",
            outcome.reason,
            project_root.display(),
            action
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_busy_reconcile_does_not_finish_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let dir = temp.path().join("store");
        let store = CallGraphStore::open(dir.clone(), root.clone()).unwrap();
        store.cold_build(&[]).unwrap();
        let path = store.sqlite_path().to_path_buf();
        drop(store);
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker
            .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        let state = Arc::new(CallgraphReconcileState::default());
        state.request("watcher overflow");
        let force = Arc::new(CallgraphForceDemand::default());
        let outcome = CallgraphReconcileJob {
            ownership: state.begin().unwrap(),
            callgraph_dir: dir,
            project_root: root,
            max_examined: 10,
            force: Arc::clone(&force),
            enqueue_refresh: Box::new(|_| panic!("busy must not enqueue a refresh")),
        }
        .run();
        assert!(
            matches!(outcome.action, CallgraphReconcileAction::Deferred(_)),
            "{outcome:?}"
        );
        assert_eq!(state.completed(), 0, "busy must not finish ownership");
        assert!(state.last().is_none());
        assert_eq!(state.pending_reason().as_deref(), Some("watcher overflow"));
        assert!(
            state.begin().is_some(),
            "unfinished ownership must be retryable"
        );
        assert_eq!(force.pending(), None);
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn abandoned_reconcile_restores_demand_and_releases_running() {
        let state = Arc::new(CallgraphReconcileState::default());
        state.request("watcher gap");
        let ownership = state.begin().expect("pending demand");
        state.request("watcher overflow");
        assert!(state.begin().is_none());
        drop(ownership);
        let reason = state.pending_reason().expect("abandoned demand retained");
        assert!(reason.contains("watcher gap"));
        assert!(reason.contains("watcher overflow"));
        assert!(state.begin().is_some(), "another job can start");
        assert_eq!(state.completed(), 0);
    }

    #[test]
    fn busy_reconcile_keeps_demand_pending_instead_of_forcing_a_rebuild() {
        let state = Arc::new(CallgraphReconcileState::default());
        state.request("watcher overflow");
        let ownership = state.begin().expect("pending demand");
        let force = CallgraphForceDemand::default();
        let busy =
            crate::callgraph_store::CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ));
        let outcome = settle_reconcile(
            ownership,
            "watcher overflow".to_string(),
            Err(busy),
            std::path::Path::new("/workspace"),
            &force,
            Box::new(|_| panic!("no refresh for a failed comparison")),
        );
        assert!(matches!(
            outcome.action,
            CallgraphReconcileAction::Deferred(_)
        ));
        assert_eq!(force.pending(), None, "contention must not force a rebuild");
        assert_eq!(
            state.pending_reason().as_deref(),
            Some("watcher overflow"),
            "the request stays pending for the next attempt"
        );
        assert!(state.begin().is_some(), "running was released");
    }

    #[test]
    fn non_transient_reconcile_failure_still_forces_a_rebuild() {
        let state = Arc::new(CallgraphReconcileState::default());
        state.request("watcher gap");
        let ownership = state.begin().expect("pending demand");
        let force = CallgraphForceDemand::default();
        let outcome = settle_reconcile(
            ownership,
            "watcher gap".to_string(),
            Err(crate::callgraph_store::CallGraphStoreError::Unavailable(
                "corrupt".to_string(),
            )),
            std::path::Path::new("/workspace"),
            &force,
            Box::new(|_| panic!("no refresh for a failed comparison")),
        );
        assert!(matches!(
            outcome.action,
            CallgraphReconcileAction::ForcedRebuild(_)
        ));
        assert!(force.pending().is_some());
        assert_eq!(state.pending_reason(), None);
        assert_eq!(state.completed(), 1);
    }

    #[test]
    fn reconcile_reason_with_separator_is_deduplicated_as_one_request() {
        let state = CallgraphReconcileState::default();
        state.request("custom, reason");
        state.request("custom, reason");
        assert_eq!(state.pending_reason().as_deref(), Some("custom, reason"));
    }
}
