//! Subc-owned standing-root maintenance.
//!
//! This module deliberately has no watcher, scheduler, or timer. `subc::mod`
//! requests `tick` on a worker from its existing maintenance timer arm. Every
//! root pass uses the existing executor's coalescable maintenance lane.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use parking_lot::Mutex;

use crate::config::{Config, IndexKind};
use crate::context::{App, AppContext};
use crate::executor::{Executor, Lane, MaintenanceCoalesceKey};
use crate::path_identity::ProjectRootId;
use crate::root_cache;
use crate::standing_roots::{StandingRootEntry, StandingRoots};

/// The standing cadence is intentionally the same arm cadence that already
/// drives `due_maintenance_jobs`; no standing timer or scheduler is created.
pub(super) const STANDING_MAINTENANCE_INTERVAL: std::time::Duration = super::DRAIN_TICK_PERIOD;

pub(super) struct StandingWorker {
    requests: Option<std::sync::mpsc::SyncSender<()>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl StandingWorker {
    pub(super) fn start(mut work: impl FnMut() + Send + 'static) -> Self {
        let (requests, rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = std::thread::spawn(move || {
            while rx.recv().is_ok() {
                if worker_shutdown.load(Ordering::Acquire) {
                    break;
                }
                work();
            }
        });
        Self {
            requests: Some(requests),
            shutdown,
            worker: Some(worker),
        }
    }

    pub(super) fn request(&self) {
        // At most one follow-up pass is queued while a reconciliation is busy.
        // Resolution remains live on each pass, with no stale identity cache.
        if let Some(requests) = &self.requests {
            let _ = requests.try_send(());
        }
    }
}

impl Drop for StandingWorker {
    fn drop(&mut self) {
        // Reconciliation can own a SQLite connection even after the transport
        // has disconnected. Stop queued passes and join the current pass so its
        // owning connection closes before the module loop releases storage.
        self.shutdown.store(true, Ordering::Release);
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
static LAST_STANDING_VERIFY_STRATEGY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

#[derive(Debug, PartialEq, Eq)]
enum ReconciliationLogAction {
    WarnFirst,
    WarnChanged {
        previous_error: String,
        suppressed_repeats: u64,
    },
    Suppress,
}

#[derive(Default)]
struct ReconciliationFailureLog {
    last_error: Option<String>,
    suppressed_repeats: u64,
}

#[derive(Default)]
struct ReconciliationBackoff {
    snapshot: Option<(
        Option<std::path::PathBuf>,
        Vec<crate::config::IndexRootConfig>,
    )>,
    retry_at: Option<std::time::Instant>,
    failures: u32,
}

impl ReconciliationBackoff {
    fn ready(&mut self, config: &Config, now: std::time::Instant) -> bool {
        let snapshot = (config.storage_dir.clone(), config.index.roots.clone());
        if self.snapshot.as_ref() != Some(&snapshot) {
            *self = Self {
                snapshot: Some(snapshot),
                ..Self::default()
            };
        }
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn failed(&mut self, config: &Config, now: std::time::Instant) {
        self.snapshot = Some((config.storage_dir.clone(), config.index.roots.clone()));
        let seconds = 60_u64
            .saturating_mul(1_u64 << self.failures.min(4))
            .min(900);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + std::time::Duration::from_secs(seconds));
    }
}

impl ReconciliationFailureLog {
    fn record_failure(&mut self, error: &str) -> ReconciliationLogAction {
        match self.last_error.take() {
            None => {
                self.last_error = Some(error.to_string());
                ReconciliationLogAction::WarnFirst
            }
            Some(previous_error) if previous_error == error => {
                self.last_error = Some(previous_error);
                self.suppressed_repeats = self.suppressed_repeats.saturating_add(1);
                ReconciliationLogAction::Suppress
            }
            Some(previous_error) => {
                self.last_error = Some(error.to_string());
                ReconciliationLogAction::WarnChanged {
                    previous_error,
                    suppressed_repeats: std::mem::take(&mut self.suppressed_repeats),
                }
            }
        }
    }

    fn record_success(&mut self) -> Option<(String, u64)> {
        self.last_error
            .take()
            .map(|error| (error, std::mem::take(&mut self.suppressed_repeats)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ProbeLocation {
    Identity(u64, u64),
    Missing(std::path::PathBuf),
}
fn probe_location(path: &std::path::Path) -> ProbeLocation {
    crate::config_live::dir_identity(path).map_or_else(
        || ProbeLocation::Missing(path.to_path_buf()),
        |(dev, ino)| ProbeLocation::Identity(dev, ino),
    )
}

/// Whether a fresh volume check (`probed`) left the cache state of the running
/// config unchanged: availability and its reason, root identity, cache epoch
/// and the semantic model folder must all match. Only a change needs the
/// session's indexes stopped or restarted.
/// The cache settings a probe started from. The probe prepares the cache root
/// off the actor, and its result waits in the actor queue; a reconnect that
/// runs first can select another cache root, lease or epoch. The result is
/// applied only while the context still runs with these exact settings.
#[derive(Debug, Clone, PartialEq)]
struct CacheProbeBasis {
    storage_dir: Option<std::path::PathBuf>,
    cache_dir: Option<std::path::PathBuf>,
    cache_dir_configured: Option<crate::config::ConfiguredCacheDir>,
    cache_dir_opt_out: bool,
    cache_config_unknown: Option<String>,
    cache_epoch: u64,
}

impl CacheProbeBasis {
    fn of(config: &Config) -> Self {
        Self {
            storage_dir: config.storage_dir.clone(),
            cache_dir: config.cache_dir.clone(),
            cache_dir_configured: config.cache_dir_configured.clone(),
            cache_dir_opt_out: config.cache_dir_opt_out,
            cache_config_unknown: config.cache_config_unknown.clone(),
            cache_epoch: config.cache_epoch,
        }
    }
}

/// Copy the probe's cache fields onto `config`, unless `config` no longer runs
/// the settings the probe started from; then the result is stale and dropped
/// (the next probe checks the new settings). Returns whether it applied.
fn apply_cache_probe(config: &mut Config, basis: &CacheProbeBasis, prepared: &Config) -> bool {
    if CacheProbeBasis::of(config) != *basis {
        return false;
    }
    config.cache_unavailable = prepared.cache_unavailable.clone();
    config.cache_epoch = prepared.cache_epoch;
    config.cache_lease = prepared.cache_lease.clone();
    config.semantic.local_cache_dir = prepared.semantic.local_cache_dir.clone();
    true
}

fn cache_state_matches(running: &Config, probed: &Config) -> bool {
    let same_lease = match (&running.cache_lease, &probed.cache_lease) {
        (Some(running), Some(probed)) => running.same_root(probed),
        (None, None) => true,
        _ => false,
    };
    same_lease
        && running.cache_epoch == probed.cache_epoch
        && running.cache_unavailable == probed.cache_unavailable
        && running.semantic.local_cache_dir == probed.semantic.local_cache_dir
        && root_cache::cache_unavailable(running) == root_cache::cache_unavailable(probed)
}
pub(super) struct StandingActor {
    app: Arc<App>,
    executor: Arc<Executor>,
    roots: StandingRoots,
    reconciliation: Mutex<()>,
    /// Once a nonempty snapshot is observed, every bind must serialize with
    /// standing reconciliation, including binds that remove the last entry.
    handoff_required: AtomicBool,
    startup: std::sync::Once,
    /// The configuration startup reconciliation reads: defaults, plus the
    /// daemon's storage directory when one was given.
    startup_config: Config,
    observed_config: Mutex<Config>,
    reconciliation_failures: Mutex<ReconciliationFailureLog>,
    reconciliation_backoff: Mutex<ReconciliationBackoff>,
    /// Root ids registered solely to host unbound standing work. Session actors
    /// are never removed by this owner.
    owned_actors: Mutex<HashMap<String, (ProjectRootId, bool)>>,
    cache_checks: Mutex<HashMap<(ProbeLocation, ProbeLocation), std::time::Instant>>,
}

impl StandingActor {
    #[cfg(test)]
    pub(super) fn hold_locks_for_test(&self, while_held: impl FnOnce()) {
        let _reconciliation = self.reconciliation.lock();
        let _config = self.observed_config.lock();
        let _failures = self.reconciliation_failures.lock();
        let _owned = self.owned_actors.lock();
        while_held();
    }

    /// Whether the one-time startup reconciliation has finished.
    #[cfg(test)]
    pub(super) fn startup_reconciled_for_test(&self) -> bool {
        self.startup.is_completed()
    }

    /// Whether standing maintenance may currently build `literal_path`: false
    /// once a session bind has paused the entry.
    #[cfg(test)]
    pub(super) fn admits_build_for_test(&self, literal_path: &str) -> bool {
        self.roots.admit_build(literal_path).is_some()
    }

    /// The artifact key standing maintenance resolved for `literal_path`.
    #[cfg(test)]
    pub(super) fn entry_artifact_key_for_test(&self, literal_path: &str) -> Option<String> {
        self.roots
            .entries()
            .into_iter()
            .find(|entry| entry.literal_path == literal_path)
            .map(|entry| entry.artifact_key)
    }

    pub(super) fn new(app: Arc<App>, executor: Arc<Executor>) -> Self {
        let config = Config::default();
        #[cfg(test)]
        let config = app.isolate_test_config(config);
        Self {
            app,
            executor,
            roots: StandingRoots::default(),
            reconciliation: Mutex::new(()),
            handoff_required: AtomicBool::new(false),
            startup: std::sync::Once::new(),
            startup_config: config.clone(),
            observed_config: Mutex::new(config),
            reconciliation_failures: Mutex::new(ReconciliationFailureLog::default()),
            reconciliation_backoff: Mutex::new(ReconciliationBackoff::default()),
            owned_actors: Mutex::new(HashMap::new()),
            cache_checks: Mutex::new(HashMap::new()),
        }
    }

    /// Point startup reconciliation at the daemon's storage directory, so it
    /// never opens the default store when the daemon runs with another one.
    pub(super) fn with_startup_storage_dir(mut self, storage_dir: &std::path::Path) -> Self {
        let config = Config {
            storage_dir: Some(storage_dir.to_path_buf()),
            ..Config::default()
        };
        #[cfg(test)]
        let config = self.app.isolate_test_config(config);
        self.startup_config = config;
        self
    }

    /// Reconcile the initial empty snapshot off the frame loop. A first bind can
    /// reach this actor before its worker, so every reconciliation boundary must
    /// ensure startup has run without later replacing a bound snapshot.
    pub(super) fn reconcile_at_startup(&self) {
        let _reconciliation = self.reconciliation.lock();
        self.reconcile_startup_once();
    }

    fn reconcile_startup_once(&self) {
        self.startup.call_once(|| {
            if let Err(error) = self.roots.reconcile(&self.startup_config) {
                log::warn!("standing roots startup reconciliation failed: {error}");
            }
        });
    }

    /// Observe the current configuration only at a subc boundary. Session actor
    /// contexts provide validated user-tier snapshots; unbound contexts created
    /// by this actor have no harness marker and cannot preserve a retired config.
    pub(super) fn observe_config_snapshot(&self) {
        let owned = self.owned_actors.lock().clone();
        let snapshot = self
            .executor
            .actor_entries()
            .into_iter()
            .filter(|(root_id, ctx)| {
                // A root this actor created has no harness marker. If RouteBind
                // later reuses it, configure restores the marker and its new
                // snapshot participates instead of being hidden by old ownership.
                !owned.values().any(|(owned_root, owned_here)| {
                    *owned_here && owned_root == root_id && ctx.config().harness.is_none()
                })
            })
            .min_by(|(left, _), (right, _)| left.as_path().cmp(right.as_path()))
            .map(|(_, ctx)| ctx.config().as_ref().clone());
        if let Some(snapshot) = snapshot {
            *self.observed_config.lock() = snapshot;
        }
    }

    /// Begin the standing half of a session bind before the session owns the
    /// shared artifact family. The bounded wait is part of the bind transition,
    /// not a timer or scheduler, and stale standing publication remains fenced
    /// even if a worker reaches its checkpoint late.
    pub(super) fn begin_session_bind(&self, ctx: &AppContext) {
        ctx.begin_configure_ack_phase("standing_bind");
        let snapshot = ctx.config().as_ref().clone();
        if !snapshot.index.roots.is_empty() {
            self.handoff_required.store(true, Ordering::Release);
        } else if !self.handoff_required.load(Ordering::Acquire) {
            // No standing snapshot has ever been admitted. In particular, the
            // startup worker may still be initializing its empty database; that
            // I/O cannot protect a publisher and must not hold up the bind ACK.
            return;
        }
        ctx.begin_configure_ack_step("reconciliation_lock");
        let _reconciliation = self.reconciliation.lock();
        // Empty-store initialization belongs to the standing worker, not the
        // bind ACK. Keep the lock while checking both snapshots so an existing
        // standing publisher can never be skipped by this fast path.
        if snapshot.index.roots.is_empty() && self.roots.entries().is_empty() {
            return;
        }
        ctx.begin_configure_ack_step("startup_reconcile");
        self.reconcile_startup_once();
        ctx.begin_configure_ack_step("snapshot_reconcile");
        // A new bind is an explicit retry; idle maintenance uses the backoff.
        *self.reconciliation_backoff.lock() = ReconciliationBackoff::default();
        if let Err(error) = self.roots.reconcile(&snapshot) {
            self.record_reconciliation_failure(
                &snapshot,
                &error.to_string(),
                std::time::Instant::now(),
            );
            return;
        }
        let entries = self.roots.entries();
        // With no standing family there is nothing to revoke. Deriving an
        // otherwise unused session key can spawn Git on a cold root, and must
        // not hold its bind ACK hostage.
        if entries.is_empty() {
            return;
        }
        let Some(session_root) = ctx
            .canonical_cache_root_opt()
            .or_else(|| snapshot.project_root.clone())
        else {
            return;
        };
        ctx.begin_configure_ack_step("session_cache_key");
        let session_claim = SessionClaim::of(ctx, &session_root);
        ctx.begin_configure_ack_step("revoke_and_checkpoint");
        for entry in entries
            .into_iter()
            .filter(|entry| session_claim.covers(entry))
        {
            match self.roots.begin_case_a_bind(&entry.literal_path) {
                Ok(_) => {
                    let _ = self.roots.wait_for_case_a_checkpoint(&entry.literal_path);
                }
                Err(error) => log::warn!(
                    "standing root bind transition refused for {}: {}",
                    entry.literal_path,
                    error
                ),
            }
        }
    }

    /// Probe each storage-wide cache location at most once per minute. An
    /// unbound context takes the fresh cache fields directly. A bound session
    /// whose cache state changed (the volume went away, came back or was
    /// replaced) gets its connect request re-run under the actor's exclusive
    /// bind gate, against the configuration it is running with, so its indexes
    /// stop or restart; config file edits live reload deferred stay deferred.
    /// No cache worker is started by the health/standing probe itself.
    fn refresh_cache_roots(&self, now: std::time::Instant) {
        let mut groups = HashMap::<(ProbeLocation, ProbeLocation), Vec<_>>::new();
        for (root, ctx) in self.executor.actor_entries() {
            if let Some(parent) = &ctx.config().cache_dir {
                groups
                    .entry((probe_location(&ctx.storage_dir()), probe_location(parent)))
                    .or_default()
                    .push((root, ctx));
            }
        }
        self.cache_checks
            .lock()
            .retain(|storage, _| groups.contains_key(storage));
        for (storage, entries) in groups {
            {
                let mut checked = self.cache_checks.lock();
                if checked.get(&storage).is_some_and(|last| {
                    now.saturating_duration_since(*last) < std::time::Duration::from_secs(60)
                }) {
                    continue;
                }
                checked.insert(storage, now);
            }
            for (root, ctx) in entries {
                // Verify this context's configuration rather than borrowing
                // another context's cache lease or unavailable status.
                let running = ctx.config();
                let basis = CacheProbeBasis::of(&running);
                let mut prepared = running.as_ref().clone();
                drop(running);
                root_cache::prepare_cache_root(&mut prepared);
                let prepared = Arc::new(prepared);
                let id = format!(
                    "subc-bind-cache-recovery-{}",
                    crate::path_identity::project_scope_key(root.as_path())
                );
                if ctx.config().harness.is_none() || ctx.subc_unbound_quiesced() {
                    let prepared = Arc::clone(&prepared);
                    let response_id = id.clone();
                    self.executor.submit_async(
                        root,
                        Lane::Mutating,
                        id,
                        Box::new(move |ctx| {
                            let mut applied = false;
                            ctx.update_config(|config| {
                                applied = apply_cache_probe(config, &basis, &prepared);
                            });
                            crate::protocol::Response::success(
                                response_id,
                                serde_json::json!({"cache_rechecked": applied}),
                            )
                        }),
                    );
                    continue;
                }
                // A bound session only needs a recovery when the volume check
                // changed its cache state: the volume went away, came back, or
                // was replaced. The recovery re-runs the session's connect
                // request against the configuration it is running with (see
                // `handle_cache_recovery`), so config file edits made since
                // the connect never reach it.
                if cache_state_matches(&ctx.config(), &prepared) {
                    continue;
                }
                let Some((mut request, connect_generation)) =
                    ctx.cache_recovery_request_if_due(now)
                else {
                    continue;
                };
                request.id.clone_from(&id);
                let prepared = Arc::clone(&prepared);
                self.executor.submit_bind_cancellable_async(
                    root,
                    id,
                    Arc::new(move |ctx| {
                        crate::commands::configure::handle_cache_recovery(
                            &request,
                            connect_generation,
                            ctx,
                            &prepared,
                        )
                    }),
                );
            }
        }
    }

    /// Reconcile the observed snapshot and enqueue one coalesced pass per root.
    /// Entry order and `search`, `semantic`, `callgraph` kind order are retained
    /// by `StandingRoots::entries` and `IndexKind::ALL` respectively.
    pub(super) fn tick(&self) {
        self.tick_at(std::time::Instant::now());
    }

    fn tick_at(&self, now: std::time::Instant) {
        self.refresh_cache_roots(now);
        let _reconciliation = self.reconciliation.lock();
        self.reconcile_startup_once();
        self.observe_config_snapshot();
        let snapshot = self.observed_config.lock().clone();
        if !self.reconciliation_backoff.lock().ready(&snapshot, now) {
            return;
        }
        if !snapshot.index.roots.is_empty() {
            self.handoff_required.store(true, Ordering::Release);
        }
        let report = match self.roots.reconcile_background(&snapshot) {
            Ok(report) => report,
            Err(error) => {
                self.record_reconciliation_failure(&snapshot, &error.to_string(), now);
                return;
            }
        };
        *self.reconciliation_backoff.lock() = ReconciliationBackoff::default();
        if let Some((previous_error, suppressed_repeats)) =
            self.reconciliation_failures.lock().record_success()
        {
            log::info!(
                "standing roots reconciliation recovered after suppressing {suppressed_repeats} repeated refusal(s): {previous_error}"
            );
        }

        self.retire_removed_actors(&report.removed);
        let contended = self.resume_entries_without_bound_session(&report.active_entries);
        for entry in report.active_entries {
            if !contended.iter().any(|claim| claim.covers(&entry)) {
                self.submit_entry_pass(entry, &snapshot);
            }
        }
    }

    fn record_reconciliation_failure(
        &self,
        snapshot: &Config,
        error: &str,
        now: std::time::Instant,
    ) {
        self.reconciliation_backoff.lock().failed(snapshot, now);
        match self.reconciliation_failures.lock().record_failure(error) {
            ReconciliationLogAction::WarnFirst => log::warn!("standing roots reconciliation refused: {error}"),
            ReconciliationLogAction::WarnChanged { previous_error, suppressed_repeats } => log::warn!("standing roots reconciliation refused: {error} ({suppressed_repeats} repeats of previous error suppressed: {previous_error})"),
            ReconciliationLogAction::Suppress => {}
        }
    }

    /// Session selection and standing selection are exclusive for one shared
    /// artifact key. A session unbind is detected from the normal subc lifecycle
    /// state and resumes only the paused standing lifecycle.
    fn resume_entries_without_bound_session(
        &self,
        entries: &[StandingRootEntry],
    ) -> Vec<SessionClaim> {
        let mut contended = Vec::new();
        if entries.is_empty() {
            return contended;
        }
        let sessions = self
            .executor
            .actor_entries()
            .into_iter()
            .filter_map(|(root_id, ctx)| {
                let root = ctx
                    .canonical_cache_root_opt()
                    .or_else(|| ctx.config().project_root.clone())?;
                let claim = SessionClaim::of(&ctx, &root);
                match ctx.try_subc_unbound_quiesced() {
                    Some(true) => None,
                    Some(false) => Some(claim),
                    None => {
                        // A busy session may still own this artifact family.
                        // Do not resume it or wait under standing reconciliation:
                        // unrelated roots and new binds must keep progressing.
                        log::debug!(
                            "standing roots skipping {} this pass: root lifecycle admission busy",
                            root_id.as_path().display()
                        );
                        contended.push(claim.clone());
                        Some(claim)
                    }
                }
            })
            .collect::<Vec<_>>();
        for entry in entries {
            if !sessions.iter().any(|claim| claim.covers(entry)) {
                // No session can prove any standing kind fresh after unbind, so
                // resume marks the whole selected set strict before its next pass.
                if let Err(error) = self.roots.resume_after_session(&entry.literal_path, &[]) {
                    log::warn!(
                        "standing root resume transition refused for {}: {}",
                        entry.literal_path,
                        error
                    );
                }
            }
        }
        contended
    }

    fn retire_removed_actors(&self, removed: &[String]) {
        let mut owned = self.owned_actors.lock();
        for literal_path in removed {
            if let Some((root_id, owned_here)) = owned.remove(literal_path) {
                self.executor.cancel_queued_maintenance(&root_id);
                if let Some(ctx) = self.executor.actor_context(&root_id) {
                    ctx.set_standing_artifact_exempt(false);
                }
                if owned_here {
                    self.executor.remove_actor(&root_id);
                }
            }
        }
    }

    fn submit_entry_pass(&self, entry: StandingRootEntry, snapshot: &Config) {
        let Some(root_id) = self.ensure_actor(&entry, snapshot) else {
            return;
        };
        let roots = self.roots.clone();
        let literal_path = entry.literal_path.clone();
        let executor_request_id = format!("subc-standing-pass-{}", entry.literal_path);
        let response_request_id = executor_request_id.clone();
        let job = Box::new(move |ctx: &AppContext| {
            if let Some(reason) = ctx.cache_unavailable_reason() {
                return crate::protocol::Response::error(
                    &response_request_id,
                    root_cache::CACHE_DIR_UNAVAILABLE,
                    reason,
                );
            }
            let Some(admission) = roots.admit_build(&literal_path) else {
                return crate::protocol::Response::success(
                    response_request_id,
                    serde_json::json!({"standing": true, "entry": literal_path, "admitted": false}),
                );
            };
            let Some(permit) =
                crate::cold_build_limiter::acquire_standing_while_cancellable_with_limiter(
                    &ctx.cold_build_limiter(),
                    "standing maintenance pass",
                    format!("standing:{}", literal_path),
                    admission.publication.admission_epoch,
                    || !admission.cancellation_requested(),
                    || {
                        crate::executor::current_job_cancelled()
                            || admission.cancellation_requested()
                    },
                )
            else {
                return crate::protocol::Response::success(
                    response_request_id,
                    serde_json::json!({"standing": true, "entry": literal_path, "admitted": false, "yielded": true}),
                );
            };
            debug_assert_eq!(
                permit.admission_epoch,
                admission.publication.admission_epoch
            );
            for kind in IndexKind::ALL {
                if !entry.indexes.contains(&kind) {
                    continue;
                }
                if crate::executor::current_job_cancelled() {
                    break;
                }
                // A strict plan is selected unconditionally at a standing pass
                // boundary. The artifact-specific loader/build code consumes the
                // plan when a resident or disk artifact is present; a failed or
                // interrupted attempt intentionally leaves the durable flag set.
                let verified = strict_verify_current_state(ctx, &entry, kind)
                    || (kind == IndexKind::Search
                        && build_missing_search_after_strict_check(
                            ctx,
                            &roots,
                            &entry,
                            &admission,
                            permit.admission_epoch,
                        ));
                if verified {
                    if let Err(error) = roots.record_strict_verification(&literal_path, kind) {
                        log::warn!(
                            "standing strict verification outcome could not commit for {} {}: {}",
                            literal_path,
                            kind.as_str(),
                            error
                        );
                    }
                }
            }
            crate::protocol::Response::success(
                response_request_id,
                serde_json::json!({"standing": true, "entry": literal_path}),
            )
        });
        let _ = self.executor.submit_coalescable_maintenance_async(
            root_id,
            Lane::MaintenanceCommit,
            executor_request_id,
            MaintenanceCoalesceKey::StandingPass,
            job,
        );
    }

    fn ensure_actor(&self, entry: &StandingRootEntry, snapshot: &Config) -> Option<ProjectRootId> {
        let root_id = match ProjectRootId::from_path(&entry.resolved_target) {
            Ok(root_id) => root_id,
            Err(error) => {
                log::warn!(
                    "standing root {} cannot enter the executor: {}",
                    entry.literal_path,
                    error
                );
                return None;
            }
        };
        if let Some(ctx) = self.executor.actor_context(&root_id) {
            ctx.set_standing_artifact_exempt(true);
            self.owned_actors
                .lock()
                .insert(entry.literal_path.clone(), (root_id.clone(), false));
            return Some(root_id);
        }

        let mut config = snapshot.clone();
        config.project_root = Some(entry.resolved_target.clone());
        // This context is subc-owned rather than session-bound. A later
        // RouteBind restores `harness` and replaces this observed snapshot.
        config.harness = None;
        config.indexes.trigram = entry.indexes.contains(&IndexKind::Search);
        config.indexes.semantic = entry.indexes.contains(&IndexKind::Semantic);
        config.indexes.callgraph = entry.indexes.contains(&IndexKind::Callgraph);
        let ctx = Arc::new(AppContext::from_app(Arc::clone(&self.app), config));
        ctx.set_canonical_cache_root(entry.resolved_target.clone());
        ctx.set_standing_artifact_exempt(true);
        // A standing actor is not configured through the session path and thus
        // must install the same root-keyed write capability before it can ever
        // acquire a WriterLease. It does not install a filesystem watcher.
        root_cache::configure_artifact_access(&entry.resolved_target, &entry.artifact_key, false);
        self.executor.register_actor(root_id.clone(), ctx);
        self.owned_actors
            .lock()
            .insert(entry.literal_path.clone(), (root_id.clone(), true));
        Some(root_id)
    }
}

/// The standing entries a bound session takes over from standing maintenance:
/// its bind pauses them (`begin_session_bind`), and standing maintenance does
/// not resume them while the session stays bound.
#[derive(Clone, Debug)]
enum SessionClaim {
    /// Every standing entry with the session's artifact key, whatever its
    /// path: a sibling clone of the same repository shares the family.
    Key(String),
    /// A session whose last bind left its identity provisional has no key
    /// yet. That bind published no key and turned its indexes off, so it
    /// reads and writes no shared artifact family; it claims only the
    /// standing entry for its own folder, which it takes over once its
    /// identity settles.
    ///
    /// Deriving a key here instead would ask Git on every maintenance tick
    /// (no failed Git answer is cached), and would claim the wrong entries
    /// anyway: the key Git alone gives can differ from the settled key, and
    /// from the key the standing entry for the same folder resolved, which
    /// may keep an older path or repository pin.
    Root(PathBuf),
}

impl SessionClaim {
    fn of(ctx: &AppContext, root: &Path) -> Self {
        match ctx.settled_artifact_cache_key(root) {
            Some(key) => Self::Key(key),
            // Spelled as the standing resolver spells `resolved_target`.
            None => Self::Root(std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())),
        }
    }

    fn covers(&self, entry: &StandingRootEntry) -> bool {
        match self {
            Self::Key(key) => entry.artifact_key == *key,
            Self::Root(root) => entry.resolved_target == *root,
        }
    }
}

#[cfg(test)]
mod startup_storage_tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn standing_empty_steady_ticks_do_no_database_or_git_work() {
        #[cfg(unix)]
        if std::env::var_os("AFT_TEST_STANDING_GIT_LOG").is_none() {
            let recorder = tempfile::tempdir().unwrap();
            let git = which::which("git").unwrap();
            let wrapper = recorder.path().join("git");
            crate::executable_cache::write_executable(
                &wrapper,
                format!(
                    "#!/bin/sh\nprintf x >> \"$AFT_TEST_STANDING_GIT_LOG\"\nexec '{}' \"$@\"\n",
                    git.to_string_lossy().replace('\'', "'\\''")
                )
                .as_bytes(),
            )
            .unwrap();
            let path = std::env::join_paths(
                std::iter::once(recorder.path().to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            // AFT caches its command search path. A separate test process puts
            // the Git logging wrapper first on PATH without changing the
            // environment of concurrently running tests.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subc::standing::startup_storage_tests::standing_empty_steady_ticks_do_no_database_or_git_work", "--nocapture"])
                .env("PATH", path)
                .env("AFT_TEST_RAW_PATH", "1")
                .env("AFT_TEST_STANDING_GIT_LOG", recorder.path().join("commands"))
                .output().unwrap();
            eprint!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success());
            return;
        }
        let git_commands = || {
            std::env::var_os("AFT_TEST_STANDING_GIT_LOG")
                .and_then(|path| std::fs::metadata(path).ok())
                .map_or(0, |meta| meta.len() as i64)
        };
        let storage = tempfile::tempdir().unwrap();
        if std::env::var_os("AFT_TEST_STANDING_GIT_LOG").is_some() {
            let project = storage.path().join("recorder-control");
            std::fs::create_dir(&project).unwrap();
            assert!(crate::effective_path::new_command("git")
                .build()
                .arg("init")
                .arg("-q")
                .arg(&project)
                .status()
                .unwrap()
                .success());
            let before = git_commands();
            crate::scoped_key::resolve_standing_root(project.to_str().unwrap()).unwrap();
            assert!(
                git_commands() > before,
                "the recorder must observe real resolver Git commands"
            );
        }
        drop(crate::db::open(&storage.path().join("aft.db")).unwrap());
        let ctx = super::super::test_support::test_ctx();
        let actor = StandingActor::new(ctx.app(), Arc::new(Executor::new()))
            .with_startup_storage_dir(storage.path());
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let mut startup = true;
        let worker = StandingWorker::start(move || {
            let before = crate::standing_roots::reconciliation_work_for_test();
            let git_before = git_commands();
            if startup {
                actor.reconcile_at_startup();
                startup = false;
            } else {
                actor.tick();
            }
            let after = crate::standing_roots::reconciliation_work_for_test();
            ack_tx
                .send(std::array::from_fn::<_, 7, _>(|i| {
                    if i == 6 {
                        git_commands() - git_before
                    } else {
                        after[i] - before[i]
                    }
                }))
                .unwrap();
        });
        worker.request();
        let startup = ack_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            startup,
            [1, 2, 2, 1, 0, 0, 0],
            "startup must initialize the namespace once"
        );
        let mut recurring = [0; 7];
        for _ in 0..100 {
            worker.request();
            let tick = ack_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            for i in 0..7 {
                recurring[i] += tick[i];
            }
        }
        eprintln!("standing startup={startup:?} recurring_100_ticks={recurring:?} (logical opens, physical opens/closes, schema peeks, resolutions, IMMEDIATE, git commands)");
        assert_eq!(recurring, [0; 7]);
    }

    #[test]
    fn standing_startup_uses_explicit_storage_not_the_default_root() {
        let _env = crate::test_env::process_env_lock();
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(value) = &self.0 {
                    std::env::set_var("AFT_STORAGE_DIR", value);
                } else {
                    std::env::remove_var("AFT_STORAGE_DIR");
                }
            }
        }
        let _restore = Restore(std::env::var_os("AFT_STORAGE_DIR"));
        std::env::remove_var("AFT_STORAGE_DIR");
        let root = tempfile::tempdir().unwrap();
        let ctx = super::super::test_support::test_ctx();
        let actor = StandingActor::new(ctx.app(), Arc::new(Executor::new()))
            .with_startup_storage_dir(root.path());
        let default =
            crate::bash_background::storage_dir_without_overrides_for_test().join("aft.db");
        // Other tests in the same process may already have created a database
        // at the default root (CI shares one hermetic home), so compare its
        // state before and after instead of requiring it to be absent.
        let snapshot = |path: &std::path::Path| {
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().ok()))
        };
        let before = snapshot(&default);
        actor.reconcile_at_startup();
        assert!(root.path().join("aft.db").is_file());
        assert_eq!(
            snapshot(&default),
            before,
            "standing startup with an explicit storage dir must not touch the default root"
        );
    }
}

fn strict_verify_current_state(
    ctx: &AppContext,
    entry: &StandingRootEntry,
    kind: IndexKind,
) -> bool {
    if crate::executor::current_job_cancelled() {
        return false;
    }
    // The strict plan is not inferred from the warm memo: an observation gap
    // must hash/scan rather than accepting a recent stat-first result.
    let plan = crate::cache_freshness::warm_verify_plan(
        &entry.resolved_target,
        crate::cache_freshness::VerifyArtifact::Search,
        None,
    );
    debug_assert_eq!(plan, crate::cache_freshness::WarmVerifyPlan::Strict);

    match kind {
        IndexKind::Search => {
            let cache_dir = crate::search_index::resolve_cache_dir_with_key(
                &entry.artifact_key,
                ctx.config().cache_storage_dir().as_deref(),
            );
            let Some(mut index) = crate::search_index::SearchIndex::read_from_disk(
                &cache_dir,
                &entry.resolved_target,
            ) else {
                // The pass has strictly established that no published baseline
                // exists. A later standing build admission remains required and
                // the durable flag deliberately stays set meanwhile.
                return false;
            };
            let verify_strategy = crate::cache_freshness::VerifyStrategy::Strict;
            #[cfg(test)]
            LAST_STANDING_VERIFY_STRATEGY.store(
                match verify_strategy {
                    crate::cache_freshness::VerifyStrategy::StatFirst => 1,
                    crate::cache_freshness::VerifyStrategy::Strict => 2,
                },
                std::sync::atomic::Ordering::SeqCst,
            );
            !index.verify_against_disk_with_strategy(
                crate::search_index::current_git_head(&entry.resolved_target),
                verify_strategy,
            )
        }
        // Semantic and callgraph have artifact-specific strict loaders whose
        // publication fences are not shared with the search cache. Do not clear
        // a durable gap until those loaders report a successful strict outcome.
        IndexKind::Semantic | IndexKind::Callgraph => false,
    }
}

/// A missing search baseline is only built after strict loading has established
/// that the old artifact cannot serve. Its final rename stays inside the same
/// lifecycle publication fence used for a stale worker rejection.
fn build_missing_search_after_strict_check(
    ctx: &AppContext,
    roots: &StandingRoots,
    entry: &StandingRootEntry,
    admission: &crate::standing_roots::StandingBuildAdmission,
    permit_epoch: u64,
) -> bool {
    if admission.cancellation_requested() || crate::executor::current_job_cancelled() {
        return false;
    }
    let config = ctx.config();
    let cache_dir = crate::search_index::resolve_cache_dir_with_key(
        &entry.artifact_key,
        config.cache_storage_dir().as_deref(),
    );
    let max_file_size = config.search_index_max_file_size;
    drop(config);
    let before_fingerprint = root_fingerprint(&entry.resolved_target);
    let configure_generation = ctx.configure_generation();
    let mut index = crate::search_index::SearchIndex::build_with_limit_to_cache_dir(
        &entry.resolved_target,
        max_file_size,
        &cache_dir,
    );
    let lease = match crate::root_cache::WriterLease::acquire_shared(
        crate::root_cache::RootCacheDomain::Index,
        &cache_dir,
        &entry.artifact_key,
        &entry.resolved_target,
    ) {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(_) => return false,
    };
    roots
        .publish_if_current(
            &entry.literal_path,
            admission.publication,
            &lease,
            || root_fingerprint(&entry.resolved_target) == before_fingerprint,
            || {
                permit_epoch == admission.publication.admission_epoch
                    && ctx.configure_generation() == configure_generation
                    && !admission.cancellation_requested()
            },
            || {
                index.write_to_disk(
                    &cache_dir,
                    crate::search_index::current_git_head(&entry.resolved_target).as_deref(),
                )
            },
        )
        .ok()
        .flatten()
        .unwrap_or(false)
}

fn root_fingerprint(root: &std::path::Path) -> Option<(u64, Option<u128>)> {
    let metadata = std::fs::metadata(root).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_nanos());
    Some((metadata.len(), modified))
}

#[cfg(test)]
mod tests {
    #[test]
    fn standing_refusal_backoff_counts_attempts_over_time() {
        let storage = tempfile::tempdir().unwrap();
        let ctx = super::super::test_support::test_ctx();
        let actor = StandingActor::new(ctx.app(), Arc::new(Executor::new()))
            .with_startup_storage_dir(storage.path());
        actor.reconcile_at_startup();
        let project = storage.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(
            storage.path().join("cache-keys.json"),
            b"unreadable repository memo",
        )
        .unwrap();
        let mut config = actor.observed_config.lock();
        config.storage_dir = Some(storage.path().to_path_buf());
        config.index.roots = vec![crate::config::IndexRootConfig {
            path: project.display().to_string(),
            indexes: vec![IndexKind::Search],
        }];
        drop(config);
        crate::developer_tools::with_git_usable_for_test(false, || {
            let resolutions = || crate::standing_roots::reconciliation_work_for_test()[4];
            let before = resolutions();
            let now = std::time::Instant::now();
            for tick in 0..240 {
                actor.tick_at(now + std::time::Duration::from_millis(tick * 250));
            }
            assert!(
                actor
                    .reconciliation_failures
                    .lock()
                    .last_error
                    .as_ref()
                    .is_some_and(|error| error.contains("identity")),
                "the fixture must reach an identity refusal, not successful reconciliation"
            );
            assert_eq!(
                resolutions() - before,
                1,
                "a refused root was resolved on every maintenance tick"
            );
            actor.tick_at(now + std::time::Duration::from_secs(60));
            assert_eq!(resolutions() - before, 2);
            actor.tick_at(now + std::time::Duration::from_secs(179));
            assert_eq!(resolutions() - before, 2);
            actor.tick_at(now + std::time::Duration::from_secs(180));
            assert_eq!(resolutions() - before, 3);
        });
    }

    use super::*;

    #[tokio::test]
    async fn empty_standing_bind_ack_does_not_derive_a_session_cache_key() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: false,
                    callgraph: false,
                },
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let executor = Arc::new(Executor::new());
        let root_id = ProjectRootId::from_path(root.path()).unwrap();
        executor.register_actor(root_id.clone(), Arc::clone(&ctx));
        let actor = Arc::new(
            StandingActor::new(ctx.app(), Arc::clone(&executor))
                .with_startup_storage_dir(storage.path()),
        );
        let bind_actor = Arc::clone(&actor);
        let held_actor = Arc::clone(&actor);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            held_actor.hold_locks_for_test(|| {
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
            });
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        // This is the post-configure part of the bind job. A root with all
        // artifact lanes disabled has no memoized key for it to reuse.
        let (reply, _cancellation) = executor.submit_bind_cancellable_async(
            root_id,
            "subc-bind-empty-standing".into(),
            Arc::new(move |ctx| {
                bind_actor.begin_session_bind(ctx);
                crate::protocol::Response::success(
                    "subc-bind-empty-standing",
                    serde_json::json!({}),
                )
            }),
        );
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), reply).await;
        let _ = release_tx.send(());
        holder.join().unwrap();
        assert!(
            reply
                .expect("an empty standing startup must not delay the bind reply")
                .unwrap()
                .success
        );
        assert_eq!(
            ctx.artifact_cache_key_derivation_count_for_test(),
            0,
            "bind completion must not wait for Git when no standing artifact family can contend"
        );
        assert!(
            !storage.path().join("aft.db").exists(),
            "empty standing storage initialization must not delay the bind reply"
        );
        actor.reconcile_at_startup();
        assert!(storage.path().join("aft.db").exists());
        actor.tick();
        assert_eq!(ctx.artifact_cache_key_derivation_count_for_test(), 0);
    }

    #[test]
    fn cache_probe_for_an_old_root_leaves_a_reconnected_root_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let prepared_for = |parent: &std::path::Path| {
            std::fs::create_dir_all(parent).unwrap();
            let mut config = Config {
                storage_dir: Some(storage.clone()),
                cache_dir: Some(parent.to_path_buf()),
                ..Default::default()
            };
            assert!(root_cache::prepare_cache_root_for_test(&mut config).is_none());
            config
        };
        // The probe snapshots the running config on root A and prepares it.
        let on_a = prepared_for(&temp.path().join("volume-a"));
        let basis = CacheProbeBasis::of(&on_a);
        let mut probed = on_a.clone();
        root_cache::prepare_cache_root(&mut probed);
        // A reconnect selects root B before the probe's callback runs.
        let mut on_b = prepared_for(&temp.path().join("volume-b"));
        let lease_b = on_b.cache_lease.clone().unwrap();
        let epoch_b = on_b.cache_epoch;
        let models_b = on_b.semantic.local_cache_dir.clone();
        assert_ne!(models_b, probed.semantic.local_cache_dir);

        assert!(!apply_cache_probe(&mut on_b, &basis, &probed));
        assert!(Arc::ptr_eq(on_b.cache_lease.as_ref().unwrap(), &lease_b));
        assert_eq!(on_b.cache_epoch, epoch_b);
        assert_eq!(on_b.semantic.local_cache_dir, models_b);
        assert!(root_cache::cache_unavailable(&on_b).is_none());

        // On the configuration it was probed for, the result applies.
        let mut still_a = on_a.clone();
        assert!(apply_cache_probe(&mut still_a, &basis, &probed));
        assert!(Arc::ptr_eq(
            still_a.cache_lease.as_ref().unwrap(),
            probed.cache_lease.as_ref().unwrap()
        ));
    }

    #[test]
    fn cache_root_standing_probes_distinct_cache_identities_for_one_storage() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let ready = temp.path().join("ready");
        let missing = temp.path().join("missing");
        std::fs::create_dir(&ready).unwrap();
        let executor = Arc::new(Executor::new());
        let mut contexts = Vec::new();
        for (name, parent) in [
            ("project-a", ready.clone()),
            ("project-b", missing),
            ("project-c", ready),
        ] {
            let project = temp.path().join(name);
            std::fs::create_dir(&project).unwrap();
            let mut config = Config {
                storage_dir: Some(storage.clone()),
                cache_dir: Some(parent),
                project_root: Some(project.clone()),
                ..Default::default()
            };
            if name == "project-c" {
                config.cache_config_unknown = Some("unreadable user document".into());
            }
            root_cache::prepare_cache_root_for_test(&mut config);
            let ctx = Arc::new(AppContext::from_app(App::default_shared(), config));
            let root = ProjectRootId::from_path(&project).unwrap();
            executor.register_actor(root.clone(), Arc::clone(&ctx));
            contexts.push((root, ctx));
        }
        let standing = StandingActor::new(contexts[0].1.app(), Arc::clone(&executor));
        standing.refresh_cache_roots(std::time::Instant::now());
        for (root, _) in &contexts {
            executor
                .submit(
                    root.clone(),
                    Lane::Mutating,
                    "probe-barrier".into(),
                    Box::new(|_| {
                        crate::protocol::Response::success("barrier", serde_json::json!({}))
                    }),
                )
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }
        assert!(contexts[0].1.config().cache_lease.is_some());
        assert!(contexts[0].1.cache_unavailable_reason().is_none());
        assert!(contexts[1].1.config().cache_lease.is_none());
        assert!(contexts[1].1.cache_unavailable_reason().is_some());
        assert!(
            contexts[2].1.config().cache_lease.is_none(),
            "a healthy context cannot lend its lease to a context with unknown config"
        );
        assert!(contexts[2].1.cache_unavailable_reason().is_some());
        assert_eq!(standing.cache_checks.lock().len(), 2);
    }

    #[test]
    fn cache_root_periodic_recovery_is_throttled_and_uses_normal_bind() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        let cache = temp.path().join("offline-volume");
        std::fs::create_dir(&project).unwrap();
        let ctx = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config::default(),
        ));
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id":"initial-cache-bind", "command":"configure", "harness":"opencode", "project_root":project, "storage_dir":storage,
            "config":[{"tier":"user", "source":"test user", "doc":serde_json::json!({"storage":{"cache_dir":cache}, "indexes":{"trigram":false,"semantic":false,"callgraph":false}}).to_string()}]
        })).unwrap();
        assert!(crate::commands::configure::handle_configure(&request, &ctx).success);
        ctx.mark_subc_bound();
        let executor = Arc::new(Executor::new());
        let root = ProjectRootId::from_path(&project).unwrap();
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let standing = StandingActor::new(ctx.app(), Arc::clone(&executor));
        let now = std::time::Instant::now();
        let barrier = || {
            executor
                .submit(
                    root.clone(),
                    Lane::Mutating,
                    "cache-recovery-barrier".into(),
                    Box::new(|_| {
                        crate::protocol::Response::success("barrier", serde_json::json!({}))
                    }),
                )
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        };
        assert!(ctx.cache_unavailable_reason().is_some());
        std::fs::create_dir(&cache).unwrap();
        let _mounted_volume =
            crate::walk_boundary::simulated_volumes::simulate(&cache, u64::MAX - 17);
        standing.refresh_cache_roots(now);
        barrier();
        assert!(
            ctx.cache_unavailable_reason().is_none(),
            "the first standing probe must recover a volume mounted after configure"
        );
        let marker_path = cache.join("aft/.aft-owner.json");
        let marker = std::fs::read(&marker_path).unwrap();
        std::fs::remove_file(&marker_path).unwrap();
        standing.refresh_cache_roots(now + std::time::Duration::from_secs(1));
        barrier();
        assert!(
            ctx.cache_unavailable_reason().is_none(),
            "unchanged location identities are throttled for one minute"
        );
        standing.refresh_cache_roots(now + std::time::Duration::from_secs(60));
        barrier();
        assert!(
            ctx.cache_unavailable_reason().is_some(),
            "the next scheduled probe verifies each context's ownership"
        );
        std::fs::write(marker_path, marker).unwrap();
        standing.refresh_cache_roots(now + std::time::Duration::from_secs(120));
        barrier();
        assert!(ctx.cache_unavailable_reason().is_none());
        assert!(cache.join("aft").is_dir());
        assert_eq!(
            ctx.try_health_snapshot(&project).cache_unavailable_reason,
            None
        );
    }

    /// A bound session whose cache folder is on a (simulated) separate volume,
    /// configured from a real user config file so that live reload sees later
    /// edits to it. The standing probe must not apply those edits: it recovers
    /// against the configuration the session is running with.
    struct BoundCacheSession {
        temp: tempfile::TempDir,
        project: std::path::PathBuf,
        cache: std::path::PathBuf,
        user_file: std::path::PathBuf,
        ctx: Arc<AppContext>,
        executor: Arc<Executor>,
        root: ProjectRootId,
        standing: StandingActor,
        _volume: crate::walk_boundary::simulated_volumes::SimulatedVolume,
    }

    impl BoundCacheSession {
        fn start() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let storage = temp.path().join("storage");
            let cache = temp.path().join("cache-volume");
            let user_dir = temp.path().join("user-config");
            for dir in [&project, &cache, &user_dir] {
                std::fs::create_dir(dir).unwrap();
            }
            let volume = crate::walk_boundary::simulated_volumes::simulate(&cache, u64::MAX - 17);
            let user_file = user_dir.join("aft.jsonc");
            Self::write_user_config(&user_file, &cache);
            let ctx = Arc::new(AppContext::from_app(
                App::default_shared(),
                Config::default(),
            ));
            let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "connect", "command": "configure", "harness": "opencode",
                "project_root": project, "storage_dir": storage,
                "cortexkit_user_config_path": user_file,
            }))
            .unwrap();
            let response = crate::commands::configure::handle_configure(&request, &ctx);
            assert!(response.success, "{response:?}");
            assert!(ctx.cache_unavailable_reason().is_none());
            ctx.mark_subc_bound();
            let executor = Arc::new(Executor::new());
            let root = ProjectRootId::from_path(&project).unwrap();
            executor.register_actor(root.clone(), Arc::clone(&ctx));
            let standing = StandingActor::new(ctx.app(), Arc::clone(&executor));
            Self {
                temp,
                project,
                cache,
                user_file,
                ctx,
                executor,
                root,
                standing,
                _volume: volume,
            }
        }

        fn write_user_config(file: &std::path::Path, cache_dir: &std::path::Path) {
            let doc = serde_json::json!({
                "storage": {"cache_dir": cache_dir},
                "indexes": {"trigram": false, "semantic": false, "callgraph": false},
            });
            std::fs::write(file, doc.to_string()).unwrap();
        }

        /// Point the user file at a volume that is not mounted and let live
        /// reload report the change as deferred until the next connect.
        fn defer_cache_dir_edit(&self) -> std::path::PathBuf {
            let unmounted = self.temp.path().join("unmounted-volume");
            Self::write_user_config(&self.user_file, &unmounted);
            match crate::config_live::reload_config_now(&self.ctx) {
                crate::config_live::ReloadOutcome::Reloaded { deferred, .. } => assert!(
                    deferred.contains(&"storage.cache_dir"),
                    "cache_dir edit was not deferred: {deferred:?}"
                ),
                other => panic!("config edit was not reloaded: {other:?}"),
            }
            unmounted
        }

        /// Run one standing probe and wait for the work it queued.
        fn refresh(&self, at: std::time::Instant) {
            self.standing.refresh_cache_roots(at);
            self.executor
                .submit(
                    self.root.clone(),
                    Lane::Mutating,
                    "cache-recovery-barrier".into(),
                    Box::new(|_| {
                        crate::protocol::Response::success("barrier", serde_json::json!({}))
                    }),
                )
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }

        fn caches_on(&self) -> bool {
            self.ctx.cache_unavailable_reason().is_none()
                && !self
                    .ctx
                    .degraded_reasons()
                    .iter()
                    .any(|reason| reason == root_cache::CACHE_DIR_UNAVAILABLE)
                && self
                    .ctx
                    .try_health_snapshot(&self.project)
                    .cache_unavailable_reason
                    .is_none()
        }
    }

    #[test]
    fn cache_recovery_queued_before_a_reconnect_leaves_the_new_connection() {
        let session = BoundCacheSession::start();
        // The probe takes the retained connect request and prepares the cache
        // root, then queues the recovery.
        let (old_request, generation) = session
            .ctx
            .cache_recovery_request_if_due(std::time::Instant::now())
            .expect("the connect request is retained for cache recovery");
        let mut prepared = session.ctx.config().as_ref().clone();
        root_cache::prepare_cache_root(&mut prepared);
        // A reconnect with another storage root and harness lands before the
        // queued recovery runs.
        let new_storage = session.temp.path().join("storage-after-reconnect");
        let reconnect: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "reconnect", "command": "configure", "harness": "pi",
            "project_root": session.project, "storage_dir": new_storage,
            "cortexkit_user_config_path": session.user_file,
        }))
        .unwrap();
        let response = crate::commands::configure::handle_configure(&reconnect, &session.ctx);
        assert!(response.success, "{response:?}");
        let connected = session.ctx.config();
        assert_eq!(
            connected.storage_dir.as_deref(),
            Some(new_storage.as_path())
        );
        // The cache volume is enrolled to the first storage root, so the new
        // connection may report it unavailable; the recovery must not change
        // the new connection's cache state either way.
        let connected_cache_state = session.ctx.cache_unavailable_reason();

        let recovery = crate::commands::configure::handle_cache_recovery(
            &old_request,
            generation,
            &session.ctx,
            &prepared,
        );
        assert!(recovery.success, "{recovery:?}");
        let after = session.ctx.config();
        assert_eq!(
            after.storage_dir, connected.storage_dir,
            "the old connect request moved the session back to its old storage root"
        );
        assert_eq!(after.harness, connected.harness);
        assert_eq!(after.cache_epoch, connected.cache_epoch);
        assert_eq!(
            session.ctx.cache_unavailable_reason(),
            connected_cache_state
        );
    }

    /// A refused reconnect leaves the earlier configuration active; its cache
    /// recovery must keep working, so caches stop when the volume goes away and
    /// restart when it returns.
    #[cfg(unix)]
    #[test]
    fn cache_recovery_still_runs_after_a_refused_reconnect() {
        let session = BoundCacheSession::start();
        let active = session.ctx.config();
        let refused: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "refused-reconnect", "command": "configure", "harness": "opencode",
            "project_root": session.project, "storage_dir": 7,
            "cortexkit_user_config_path": session.user_file,
        }))
        .unwrap();
        let response = crate::commands::configure::handle_configure(&refused, &session.ctx);
        assert!(!response.success, "{response:?}");
        assert!(
            Arc::ptr_eq(&active, &session.ctx.config()),
            "the refused reconnect changed the active configuration"
        );
        drop(active);

        let away = session.temp.path().join("cache-volume-unmounted");
        let now = std::time::Instant::now();
        std::fs::rename(&session.cache, &away).unwrap();
        session.refresh(now);
        assert!(
            session.ctx.cache_unavailable_reason().is_some(),
            "the probe did not notice the volume went away"
        );
        std::fs::rename(&away, &session.cache).unwrap();
        session.refresh(now + std::time::Duration::from_secs(61));
        assert!(
            session.caches_on(),
            "the returning volume did not restore the caches (a recovery dropped as stale?): {:?}",
            session.ctx.cache_unavailable_reason()
        );
    }

    /// A cache recovery and a reconnect are both route-bind configures on the
    /// actor's Mutating lane. A bind admitted beside a reader runs with only a
    /// shared hold on the epoch gate, but it still marks the actor's writer in
    /// flight for its whole job, and the scheduler then admits only read lanes:
    /// a reconnect queued while the recovery runs does not start until the
    /// recovery finishes, and it publishes last, so its settings win.
    #[test]
    fn reconnect_queued_while_a_cache_recovery_runs_waits_and_wins() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        let session = BoundCacheSession::start();
        let (old_request, generation) = session
            .ctx
            .cache_recovery_request_if_due(std::time::Instant::now())
            .expect("the connect request is retained for cache recovery");
        let mut prepared = session.ctx.config().as_ref().clone();
        root_cache::prepare_cache_root(&mut prepared);
        let prepared = Arc::new(prepared);
        let old_storage = session.ctx.config().storage_dir.clone();
        let wait = std::time::Duration::from_secs(10);

        // A reader holds the epoch gate, so the recovery starts with only a
        // shared hold: the case the writer-in-flight rule alone serializes.
        let (reader_started_tx, reader_started_rx) = std::sync::mpsc::channel();
        let (reader_release_tx, reader_release_rx) = std::sync::mpsc::channel::<()>();
        let reader = session.executor.submit(
            session.root.clone(),
            Lane::PureRead,
            "held-reader".into(),
            Box::new(move |_| {
                reader_started_tx.send(()).unwrap();
                let _ = reader_release_rx.recv_timeout(wait);
                crate::protocol::Response::success("held-reader", serde_json::json!({}))
            }),
        );
        reader_started_rx
            .recv_timeout(wait)
            .expect("the reader started");

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let held = AtomicBool::new(false);
        let (recovery_rx, _recovery_cancel) = session.executor.submit_bind_cancellable_async(
            session.root.clone(),
            "subc-bind-cache-recovery-test".into(),
            Arc::new(move |ctx| {
                // The generation check is about to pass; hold the job here,
                // before the configure, as a slow recovery would.
                if !held.swap(true, AtomicOrdering::SeqCst) {
                    started_tx
                        .send(crate::executor::current_bind_gate_is_shared())
                        .unwrap();
                    let _ = release_rx.lock().recv_timeout(wait);
                }
                crate::commands::configure::handle_cache_recovery(
                    &old_request,
                    generation,
                    ctx,
                    &prepared,
                )
            }),
        );
        let shared = started_rx.recv_timeout(wait).expect("the recovery started");
        assert!(
            shared,
            "the recovery was expected to start with a shared hold"
        );

        let new_storage = session.temp.path().join("storage-after-reconnect");
        let reconnect: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "subc-bind-reconnect-test", "command": "configure", "harness": "pi",
            "project_root": session.project, "storage_dir": new_storage,
            "cortexkit_user_config_path": session.user_file,
        }))
        .unwrap();
        let reconnect_entered = Arc::new(AtomicBool::new(false));
        let entered = Arc::clone(&reconnect_entered);
        let (reconnect_rx, _reconnect_cancel) = session.executor.submit_bind_cancellable_async(
            session.root.clone(),
            "subc-bind-reconnect-test".into(),
            Arc::new(move |ctx| {
                entered.store(true, AtomicOrdering::SeqCst);
                crate::commands::configure::handle_configure(&reconnect, ctx)
            }),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !reconnect_entered.load(AtomicOrdering::SeqCst),
            "the reconnect started while the recovery held the actor's writer slot"
        );
        assert_eq!(session.ctx.config().storage_dir, old_storage);

        release_tx.send(()).unwrap();
        reader_release_tx.send(()).unwrap();
        reader.recv_timeout(wait).unwrap();
        let recovery = recovery_rx.blocking_recv().unwrap();
        assert!(recovery.success, "{recovery:?}");
        let reconnected = reconnect_rx.blocking_recv().unwrap();
        assert!(reconnected.success, "{reconnected:?}");
        let config = session.ctx.config();
        assert_eq!(config.storage_dir.as_deref(), Some(new_storage.as_path()));
        assert_eq!(
            config.harness,
            Some(crate::harness::Harness::Pi),
            "the recovery's old settings outlived the reconnect"
        );
    }

    #[test]
    fn cache_root_refresh_keeps_a_deferred_cache_dir_edit_deferred() {
        let session = BoundCacheSession::start();
        let connected = session.ctx.config().cache_dir.clone();
        let unmounted = session.defer_cache_dir_edit();
        let now = std::time::Instant::now();
        let last_tick = now + std::time::Duration::from_secs(61 * 3);
        for tick in 0..4u64 {
            session.refresh(now + std::time::Duration::from_secs(61 * tick));
        }
        assert_eq!(
            session.ctx.config().cache_dir,
            connected,
            "a probe applied a cache_dir edit that live reload deferred"
        );
        assert!(
            session.caches_on(),
            "the running session lost its caches: {:?}",
            session.ctx.cache_unavailable_reason()
        );
        assert!(session.cache.join("aft").is_dir());
        assert!(!unmounted.exists());
        // A recovery that re-runs configure retains the request it ran, whose
        // id is the recovery id. The retained request still carrying the id
        // `connect` shows no probe re-ran configure, as none saw a change.
        let retained = session
            .ctx
            .cache_recovery_request_if_due(last_tick + std::time::Duration::from_secs(61))
            .expect("the connect request is retained for cache recovery")
            .0;
        assert_eq!(
            retained.id, "connect",
            "a probe that found the cache volume unchanged re-ran configure"
        );
    }

    /// Renaming the cache folder away models a volume that is unmounted while
    /// the session runs. Windows cannot rename a folder whose files the
    /// session's cache lease holds open, so this runs on Unix only.
    #[cfg(unix)]
    #[test]
    fn cache_root_refresh_restores_a_returning_volume_from_the_connected_config() {
        let session = BoundCacheSession::start();
        let connected = session.ctx.config().cache_dir.clone();
        session.defer_cache_dir_edit();
        let away = session.temp.path().join("cache-volume-unmounted");
        let now = std::time::Instant::now();

        std::fs::rename(&session.cache, &away).unwrap();
        session.refresh(now);
        assert!(
            session.ctx.cache_unavailable_reason().is_some(),
            "the probe did not notice the volume went away"
        );
        assert!(
            !session.cache.exists(),
            "the recovery recreated the missing cache volume"
        );

        std::fs::rename(&away, &session.cache).unwrap();
        session.refresh(now + std::time::Duration::from_secs(61));
        assert_eq!(session.ctx.config().cache_dir, connected);
        assert!(
            session.caches_on(),
            "the returning volume did not restore the caches: {:?}",
            session.ctx.cache_unavailable_reason()
        );
    }

    #[test]
    fn late_startup_reconciliation_preserves_a_bound_standing_snapshot() {
        use crate::config::{IndexConfig, IndexRootConfig};

        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let literal = root.path().to_string_lossy().into_owned();
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                index: IndexConfig {
                    roots: vec![IndexRootConfig {
                        path: literal.clone(),
                        indexes: vec![IndexKind::Search],
                    }],
                },
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let actor = StandingActor::new(ctx.app(), Arc::new(Executor::new()));
        // The frame loop may receive a bind before the standing worker runs.
        actor.begin_session_bind(&ctx);
        assert_eq!(actor.roots.entries().len(), 1);
        actor.reconcile_at_startup();
        assert_eq!(
            actor.roots.entries().len(),
            1,
            "late startup must not replace the session's snapshot with an empty one"
        );
        assert!(
            actor.roots.admit_build(&literal).is_none(),
            "the bind's publication revocation must remain in force"
        );
    }

    /// A session whose last bind left its identity provisional has no
    /// artifact key, so it claims the standing entry for its own folder by
    /// path: its bind pauses that entry, and maintenance ticks keep it paused
    /// while the session stays bound, without deriving a key for the session.
    #[test]
    fn provisional_session_keeps_the_standing_entry_for_its_folder_paused() {
        use crate::config::{IndexConfig, IndexRootConfig};

        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        let literal = canonical.to_string_lossy().into_owned();
        let ctx = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(canonical.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                index: IndexConfig {
                    roots: vec![IndexRootConfig {
                        path: literal.clone(),
                        indexes: vec![IndexKind::Search],
                    }],
                },
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(canonical.clone());
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_degraded_reasons(vec![format!(
            "{}: stubbed unreadable repository",
            crate::search_index::GIT_IDENTITY_UNAVAILABLE_REASON
        )]);
        assert!(ctx.artifact_identity_provisional());
        let executor = Arc::new(Executor::new());
        executor.register_actor(
            ProjectRootId::from_path(&canonical).unwrap(),
            Arc::clone(&ctx),
        );
        let actor = StandingActor::new(ctx.app(), Arc::clone(&executor))
            .with_startup_storage_dir(storage.path());

        actor.begin_session_bind(&ctx);
        assert_eq!(actor.roots.entries().len(), 1);
        assert!(
            actor.roots.admit_build(&literal).is_none(),
            "the bind pauses the standing entry for the session's folder"
        );
        for _ in 0..4 {
            actor.tick();
        }
        assert!(
            actor.roots.admit_build(&literal).is_none(),
            "maintenance resumed the standing entry of a bound provisional session's folder"
        );
        assert_eq!(
            ctx.artifact_cache_key_derivation_count_for_test(),
            0,
            "no key is derived for a provisional session"
        );
    }

    #[test]
    fn standing_worker_drop_waits_for_owned_sqlite_connection() {
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let connection = Arc::new(parking_lot::Mutex::new(
            crate::db::open(&temp.path().join("aft.db")).unwrap(),
        ));
        let owner = Arc::downgrade(&connection);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = StandingWorker::start(move || {
            assert!(connection.lock().is_autocommit());
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        worker.request();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (dropping_tx, dropping_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let dropping = std::thread::spawn(move || {
            dropping_tx.send(()).unwrap();
            drop(worker);
            done_tx.send(()).unwrap();
        });
        dropping_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let returned_early = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        // Release the controlled work even if teardown incorrectly returned,
        // so a failed assertion cannot strand its connection-owning thread.
        release_tx.send(()).unwrap();
        dropping.join().unwrap();
        assert!(
            !returned_early,
            "worker drop returned while SQLite was still owned"
        );
        assert!(
            owner.upgrade().is_none(),
            "connection owner survived worker drop"
        );
        #[cfg(target_os = "linux")]
        {
            let targets = std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(temp.path()))
                .collect::<Vec<_>>();
            assert!(
                targets.is_empty(),
                "SQLite descriptors survived teardown: {targets:?}"
            );
        }
    }

    #[test]
    fn standing_tick_requests_do_no_frame_thread_reconciliation() {
        let frame_thread = std::thread::current().id();
        let on_frame = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&on_frame);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = super::StandingWorker::start(move || {
            if std::thread::current().id() == frame_thread {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = tx.send(());
        });
        for _ in 0..12 {
            worker.request();
        }
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("standing pass executes");
        assert_eq!(
            on_frame.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "frame-thread reconciliation passes"
        );
    }
    #[test]
    fn reconciliation_failure_logging_tracks_error_state_changes_and_recovery() {
        let mut failures = ReconciliationFailureLog::default();

        assert_eq!(
            failures.record_failure("schema mismatch"),
            ReconciliationLogAction::WarnFirst
        );
        assert_eq!(
            failures.record_failure("schema mismatch"),
            ReconciliationLogAction::Suppress
        );
        assert_eq!(
            failures.record_failure("schema mismatch"),
            ReconciliationLogAction::Suppress
        );
        assert_eq!(
            failures.record_failure("database locked"),
            ReconciliationLogAction::WarnChanged {
                previous_error: "schema mismatch".to_string(),
                suppressed_repeats: 2,
            }
        );
        assert_eq!(
            failures.record_failure("database locked"),
            ReconciliationLogAction::Suppress
        );
        assert_eq!(
            failures.record_success(),
            Some(("database locked".to_string(), 1))
        );
        assert_eq!(failures.record_success(), None);
    }

    #[test]
    fn standing_interval_uses_the_existing_subc_maintenance_cadence() {
        assert_eq!(
            STANDING_MAINTENANCE_INTERVAL,
            super::super::DRAIN_TICK_PERIOD
        );
    }

    #[test]
    fn strict_search_verification_accepts_metadata_only_drift() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("main.rs");
        std::fs::write(&source, "fn stable() {}\n").unwrap();
        let resolved =
            crate::scoped_key::resolve_standing_root(root.path().to_str().unwrap()).unwrap();
        let entry = StandingRootEntry {
            literal_path: root.path().to_string_lossy().into_owned(),
            resolved_target: std::path::PathBuf::from(resolved.resolved_target),
            resolved_git_toplevel: resolved.resolved_git_toplevel.map(std::path::PathBuf::from),
            scoped_relative_path: resolved.scoped_relative_path.map(std::path::PathBuf::from),
            artifact_key: resolved.artifact_key,
            indexes: vec![IndexKind::Search],
            config_order: 0,
        };
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        let cache_dir = crate::search_index::resolve_cache_dir_with_key(
            &entry.artifact_key,
            Some(storage.path()),
        );
        let mut index = crate::search_index::SearchIndex::build_with_limit_to_cache_dir(
            &entry.resolved_target,
            1_048_576,
            &cache_dir,
        );
        assert!(index.write_to_disk(&cache_dir, None));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&source, "fn stable() {}\n").unwrap();
        let mut config = Config::default();
        config.storage_dir = Some(storage.path().to_path_buf());
        let ctx = AppContext::from_app(App::default_shared(), config);
        let _ = strict_verify_current_state(&ctx, &entry, IndexKind::Search);
        assert_eq!(
            LAST_STANDING_VERIFY_STRATEGY.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "standing search verification must use VerifyStrategy::Strict"
        );
    }

    #[test]
    fn kind_order_is_the_normalized_search_semantic_callgraph_order() {
        assert_eq!(
            IndexKind::ALL,
            [IndexKind::Search, IndexKind::Semantic, IndexKind::Callgraph]
        );
    }
}
