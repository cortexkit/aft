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
use std::sync::{Arc, Weak};

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

#[derive(Debug, Default)]
struct ReconcileQueryResult {
    whole_store: bool,
    paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct ReconcileQueryCheck {
    generation: u64,
    result: parking_lot::Mutex<Option<ReconcileQueryResult>>,
}

#[derive(Debug)]
struct ReconcileQueryObservation {
    generation: u64,
    root: PathBuf,
    checks: parking_lot::Mutex<Vec<Arc<ReconcileQueryCheck>>>,
}

/// Comparisons a query overlaps, including one started when that query opens
/// a reused reader. An unchanged result can clear the uncertainty before the
/// reply is finalized; discovered edits remain attached to that query.
#[derive(Clone, Debug)]
pub(crate) struct CallgraphReconcileQuery(Arc<ReconcileQueryObservation>);

impl PartialEq for CallgraphReconcileQuery {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CallgraphReconcileQuery {}

impl CallgraphReconcileQuery {
    pub(crate) fn pending(&self) -> (bool, std::collections::BTreeSet<String>) {
        let mut whole_store = false;
        let mut paths = std::collections::BTreeSet::new();
        for check in self.0.checks.lock().iter() {
            let result = check.result.lock();
            match result.as_ref() {
                None => whole_store = true,
                Some(result) => {
                    whole_store |= result.whole_store;
                    paths.extend(result.paths.iter().map(|path| {
                        path.strip_prefix(&self.0.root)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/")
                    }));
                }
            }
        }
        (whole_store, paths)
    }
}

/// Reconcile demand for one context: the reason of a requested reconcile that
/// has not started, whether one is running, and the last finished outcome.
#[derive(Debug, Default)]
pub(crate) struct CallgraphReconcileState {
    pending: parking_lot::Mutex<Vec<String>>,
    running: AtomicBool,
    pending_checks: parking_lot::Mutex<Vec<Arc<ReconcileQueryCheck>>>,
    running_checks: parking_lot::Mutex<Vec<Arc<ReconcileQueryCheck>>>,
    /// Keeps query registration atomic with reconcile request/start/restore,
    /// so a query observes a comparison started at the same time instead of
    /// capturing no active checks.
    queries: parking_lot::Mutex<Vec<Weak<ReconcileQueryObservation>>>,
    last: parking_lot::Mutex<Option<CallgraphReconcileOutcome>>,
    completed: AtomicU64,
    /// Test-only count of read-only opens the owning context finished before
    /// starting a comparison. The ordering test reads it through the entry
    /// hold to prove the caller's open completed first.
    #[cfg(test)]
    ensure_open_completed: AtomicU64,
    #[cfg(all(test, target_os = "linux"))]
    finished_at: parking_lot::Mutex<Option<std::time::Instant>>,
    #[cfg(test)]
    hold: parking_lot::Mutex<Option<(std::sync::mpsc::Sender<u64>, std::sync::mpsc::Receiver<()>)>>,
    #[cfg(test)]
    settled: parking_lot::Condvar,
}

impl CallgraphReconcileState {
    #[cfg(test)]
    pub(crate) fn request(&self, reason: &str) {
        self.request_for_generation(reason, 0);
    }

    pub(crate) fn request_for_generation(&self, reason: &str, generation: u64) {
        let mut queries = self.queries.lock();
        let mut pending = self.pending.lock();
        if !pending.iter().any(|known| known == reason) {
            pending.push(reason.to_string());
        }
        let mut checks = self.pending_checks.lock();
        if !checks.iter().any(|check| check.generation == generation) {
            let check = Arc::new(ReconcileQueryCheck {
                generation,
                result: parking_lot::Mutex::new(None),
            });
            checks.push(Arc::clone(&check));
            queries.retain(|query| {
                if let Some(query) = query.upgrade() {
                    if query.generation == generation {
                        query.checks.lock().push(Arc::clone(&check));
                    }
                    true
                } else {
                    false
                }
            });
        }
    }

    pub(crate) fn observe_query(&self, generation: u64, root: PathBuf) -> CallgraphReconcileQuery {
        let mut queries = self.queries.lock();
        let mut checks = self.pending_checks.lock().clone();
        checks.extend(self.running_checks.lock().iter().cloned());
        checks.retain(|check| check.generation == generation);
        let query = Arc::new(ReconcileQueryObservation {
            generation,
            root,
            checks: parking_lot::Mutex::new(checks),
        });
        queries.retain(|query| query.strong_count() > 0);
        queries.push(Arc::downgrade(&query));
        CallgraphReconcileQuery(query)
    }

    pub(crate) fn pending_reason(&self) -> Option<String> {
        let pending = self.pending.lock();
        (!pending.is_empty()).then(|| pending.join(", "))
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> bool {
        !self.pending.lock().is_empty() || self.running.load(Ordering::SeqCst)
    }

    /// Take the pending request and mark a reconcile as running. Returns None
    /// when nothing is pending or a reconcile is already running; a request
    /// made while one runs stays pending for the next start.
    pub(crate) fn begin(self: &Arc<Self>) -> Option<CallgraphReconcileOwnership> {
        let _queries = self.queries.lock();
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
        let checks = std::mem::take(&mut *self.pending_checks.lock());
        *self.running_checks.lock() = checks.clone();
        Some(CallgraphReconcileOwnership {
            state: Arc::clone(self),
            reasons: std::mem::take(&mut *pending),
            checks,
            finished: false,
        })
    }

    pub(crate) fn clear_pending(&self) {
        let _queries = self.queries.lock();
        self.pending.lock().clear();
        for check in self.pending_checks.lock().drain(..) {
            *check.result.lock() = Some(ReconcileQueryResult::default());
        }
    }

    fn finish(&self, outcome: CallgraphReconcileOutcome) {
        *self.last.lock() = Some(outcome);
        #[cfg(all(test, target_os = "linux"))]
        {
            *self.finished_at.lock() = Some(std::time::Instant::now());
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn last(&self) -> Option<CallgraphReconcileOutcome> {
        self.last.lock().clone()
    }

    pub(crate) fn completed(&self) -> u64 {
        self.completed.load(Ordering::SeqCst)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn finished_at_for_test(&self) -> std::time::Instant {
        self.finished_at.lock().expect("comparison finished")
    }

    #[cfg(test)]
    pub(crate) fn wait_for_idle_for_test(&self, hang_cap: std::time::Duration) -> bool {
        let mut queries = self.queries.lock();
        let deadline = std::time::Instant::now() + hang_cap;
        while self.completed() == 0 || self.outstanding() {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            if self.settled.wait_for(&mut queries, remaining).timed_out() {
                return self.completed() > 0 && !self.outstanding();
            }
        }
        true
    }

    /// Record that the owning context finished a read-only open before
    /// starting the next comparison.
    #[cfg(test)]
    pub(crate) fn note_ensure_open_completed_for_test(&self) {
        self.ensure_open_completed.fetch_add(1, Ordering::SeqCst);
    }

    /// How many read-only opens finished before the current comparison.
    #[cfg(test)]
    pub(crate) fn ensure_open_completed_for_test(&self) -> u64 {
        self.ensure_open_completed.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn hold_next_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<u64>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.hold.lock() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
}

/// Owns the reconciliation running flag before thread spawn, so a failed spawn
/// drops the ownership and allows another job to service the pending demand.
pub(crate) struct CallgraphReconcileOwnership {
    state: Arc<CallgraphReconcileState>,
    reasons: Vec<String>,
    checks: Vec<Arc<ReconcileQueryCheck>>,
    finished: bool,
}

impl CallgraphReconcileOwnership {
    pub(crate) fn startup_only(&self) -> bool {
        self.reasons.len() == 1 && self.reasons[0] == "startup"
    }

    fn finish(&mut self, outcome: CallgraphReconcileOutcome) {
        for check in &self.checks {
            let result = match &outcome.action {
                CallgraphReconcileAction::Unchanged
                | CallgraphReconcileAction::NoPublishedStore => ReconcileQueryResult::default(),
                CallgraphReconcileAction::Refreshed(paths) => ReconcileQueryResult {
                    whole_store: false,
                    paths: paths.clone(),
                },
                _ => ReconcileQueryResult {
                    whole_store: true,
                    paths: Vec::new(),
                },
            };
            *check.result.lock() = Some(result);
        }
        self.state.finish(outcome);
        self.finished = true;
    }
}

impl Drop for CallgraphReconcileOwnership {
    fn drop(&mut self) {
        let _queries = self.state.queries.lock();
        // Spawn failure and unwinding must preserve demand, not silently lose
        // the watcher gap. Restore it before allowing another job to start.
        if !self.finished {
            let mut pending = self.state.pending.lock();
            for reason in &self.reasons {
                if !pending.contains(reason) {
                    pending.push(reason.clone());
                }
            }
            self.state
                .pending_checks
                .lock()
                .extend(self.checks.iter().cloned());
        }
        self.state.running_checks.lock().clear();
        self.state.running.store(false, Ordering::SeqCst);
        #[cfg(test)]
        self.state.settled.notify_all();
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
        #[cfg(test)]
        {
            let hold = ownership.state.hold.lock().take();
            if let Some((entered, release)) = hold {
                // The entry hold reports how many of the caller's opens had
                // finished when this thread started: with the spawn ordered
                // after the open, that already includes the current reopen.
                let _ = entered.send(ownership.state.ensure_open_completed_for_test());
                let _ = release.recv_timeout(std::time::Duration::from_secs(30));
            }
        }
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
            // The readonly connection already waited out its busy timeout.
            // Lock contention says nothing about whether the stored graph
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
            let confirmed_changes = report
                .changed_count()
                .saturating_sub(report.unverified_files);
            let action = if report.truncated {
                let fallback = format!(
                    "{reason}: reconcile stopped at its bound of {} files",
                    report.max_examined
                );
                force.mark(&fallback);
                CallgraphReconcileAction::ForcedRebuild(fallback)
            } else if confirmed_changes >= RECONCILE_REBUILD_MIN_CHANGED
                && confirmed_changes.saturating_mul(2) > report.stored_files
            {
                let fallback = format!(
                    "{reason}: {} of {} stored files changed",
                    confirmed_changes, report.stored_files
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
    fn stat_first_unverified_legacy_rows_refresh_without_a_cold_build() {
        let state = Arc::new(CallgraphReconcileState::default());
        state.request("startup");
        let ownership = state.begin().unwrap();
        let force = CallgraphForceDemand::default();
        let mut paths = (0..RECONCILE_REBUILD_MIN_CHANGED)
            .map(|index| PathBuf::from(format!("source{index}.ts")))
            .collect::<Vec<_>>();
        paths.sort();
        let queued = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = Arc::clone(&queued);
        let outcome = settle_reconcile(
            ownership,
            "startup".into(),
            Ok(DiskReconcileReport {
                stored_files: paths.len(),
                modified: paths.clone(),
                unverified_files: paths.len(),
                ..DiskReconcileReport::default()
            }),
            std::path::Path::new("workspace"),
            &force,
            Box::new(move |paths| *observed.lock().unwrap() = paths),
        );
        assert_eq!(
            force.pending(),
            None,
            "missing hints are not proof of corpus drift"
        );
        assert_eq!(
            outcome.action,
            CallgraphReconcileAction::Refreshed(paths.clone())
        );
        assert_eq!(*queued.lock().unwrap(), paths);
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
#[test]
fn reconcile_query_only_tracks_real_checks_and_clears_unchanged() {
    let state = Arc::new(CallgraphReconcileState::default());
    let query = state.observe_query(1, PathBuf::from("workspace"));
    assert_eq!(
        query.pending(),
        (false, Default::default()),
        "a cold query has no comparison"
    );
    state.request_for_generation("startup", 1);
    let mut owned = state.begin().unwrap();
    assert!(query.pending().0);
    owned.finish(CallgraphReconcileOutcome {
        reason: "startup".into(),
        report: None,
        action: CallgraphReconcileAction::Unchanged,
    });
    assert_eq!(
        query.pending(),
        (false, Default::default()),
        "completion clears uncertainty before reply finalization"
    );
    drop(owned);
    assert_eq!(
        state.observe_query(1, PathBuf::from("workspace")).pending(),
        (false, Default::default())
    );
}

#[test]
fn reconcile_query_keeps_discovered_paths_and_ignores_other_generations() {
    let state = Arc::new(CallgraphReconcileState::default());
    let query = state.observe_query(1, PathBuf::from("workspace"));
    let newer_query = state.observe_query(2, PathBuf::from("workspace"));
    state.request_for_generation("startup", 1);
    let mut first = state.begin().unwrap();
    first.finish(CallgraphReconcileOutcome {
        reason: "startup".into(),
        report: None,
        action: CallgraphReconcileAction::Refreshed(vec![PathBuf::from("workspace/changed.rs")]),
    });
    drop(first);
    state.request_for_generation("watcher gap", 1);
    let mut second = state.begin().unwrap();
    second.finish(CallgraphReconcileOutcome {
        reason: "watcher gap".into(),
        report: None,
        action: CallgraphReconcileAction::Unchanged,
    });
    drop(second);
    assert_eq!(
        query.pending(),
        (false, ["changed.rs".to_string()].into_iter().collect())
    );
    assert_eq!(newer_query.pending(), (false, Default::default()));
}
