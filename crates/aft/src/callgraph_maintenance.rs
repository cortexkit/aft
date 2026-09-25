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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    pending: parking_lot::Mutex<Option<String>>,
    running: AtomicBool,
    last: parking_lot::Mutex<Option<CallgraphReconcileOutcome>>,
    completed: AtomicU64,
}

impl CallgraphReconcileState {
    pub(crate) fn request(&self, reason: &str) {
        let mut pending = self.pending.lock();
        match pending.as_mut() {
            Some(existing) if !existing.split(", ").any(|known| known == reason) => {
                existing.push_str(", ");
                existing.push_str(reason);
            }
            Some(_) => {}
            None => *pending = Some(reason.to_string()),
        }
    }

    pub(crate) fn pending_reason(&self) -> Option<String> {
        self.pending.lock().clone()
    }

    /// Take the pending request and mark a reconcile as running. Returns None
    /// when nothing is pending or a reconcile is already running; a request
    /// made while one runs stays pending for the next start.
    pub(crate) fn begin(&self) -> Option<String> {
        let mut pending = self.pending.lock();
        pending.as_ref()?;
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        pending.take()
    }

    pub(crate) fn clear_pending(&self) {
        self.pending.lock().take();
    }

    fn finish(&self, outcome: CallgraphReconcileOutcome) {
        *self.last.lock() = Some(outcome);
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
    }

    pub(crate) fn last(&self) -> Option<CallgraphReconcileOutcome> {
        self.last.lock().clone()
    }

    pub(crate) fn completed(&self) -> u64 {
        self.completed.load(Ordering::SeqCst)
    }
}

/// Everything a reconcile needs, captured on the requesting thread so the
/// comparison can run off it.
pub(crate) struct CallgraphReconcileJob {
    pub(crate) reason: String,
    pub(crate) callgraph_dir: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) max_examined: usize,
    pub(crate) state: Arc<CallgraphReconcileState>,
    pub(crate) force: Arc<CallgraphForceDemand>,
    /// Hands the changed paths to the incremental refresh for the generation
    /// that requested the reconcile.
    pub(crate) enqueue_refresh: Box<dyn FnOnce(Vec<PathBuf>) + Send>,
}

impl CallgraphReconcileJob {
    /// Compare the published store with the disk and act on the difference.
    pub(crate) fn run(self) -> CallgraphReconcileOutcome {
        let CallgraphReconcileJob {
            reason,
            callgraph_dir,
            project_root,
            max_examined,
            state,
            force,
            enqueue_refresh,
        } = self;
        let report = match CallGraphStore::open_readonly(callgraph_dir, project_root.clone()) {
            Ok(Some(store)) => store.reconcile_with_disk(max_examined),
            Ok(None) => {
                let outcome = CallgraphReconcileOutcome {
                    reason,
                    report: None,
                    action: CallgraphReconcileAction::NoPublishedStore,
                };
                log_outcome(&project_root, &outcome);
                state.finish(outcome.clone());
                return outcome;
            }
            Err(error) => Err(error),
        };
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
        log_outcome(&project_root, &outcome);
        state.finish(outcome.clone());
        outcome
    }
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
