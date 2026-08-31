//! Subc-owned standing-root maintenance.
//!
//! This module deliberately has no watcher, scheduler, or timer. `subc::mod`
//! calls `tick` from its existing maintenance timer arm, and every root pass is
//! submitted through the existing executor's coalescable maintenance lane.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Instant;

use parking_lot::Mutex;

type CachedSemanticModel = Arc<SyncMutex<Option<crate::semantic_index::EmbeddingModel>>>;

#[derive(Clone)]
struct SemanticBuildCache {
    config_key: String,
    files_key: String,
    files: Arc<Vec<PathBuf>>,
    model: CachedSemanticModel,
}

use crate::config::{Config, IndexKind};
use crate::context::{App, AppContext};
use crate::executor::{Executor, Lane, MaintenanceCoalesceKey};
use crate::path_identity::ProjectRootId;
use crate::resource_policy::{sample_resources, AdmissionDecision, ResourceAdmissionGate};
use crate::root_cache;
use crate::standing_roots::{StandingRootEntry, StandingRoots};
use crate::standing_scheduler::DeficitRoundRobin;

/// The standing cadence is intentionally the same arm cadence that already
/// drives `due_maintenance_jobs`; no standing timer or scheduler is created.
pub(super) const STANDING_MAINTENANCE_INTERVAL: std::time::Duration = super::DRAIN_TICK_PERIOD;

#[cfg(test)]
static LAST_STANDING_VERIFY_STRATEGY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);
const STANDING_SERVICE_QUANTUM_MS: u64 = 250;

#[derive(Clone, Debug, PartialEq, Eq)]
struct StandingReconcileKey {
    storage_dir: Option<std::path::PathBuf>,
    roots: Vec<crate::config::IndexRootConfig>,
}

impl StandingReconcileKey {
    fn from_config(config: &Config) -> Self {
        Self {
            storage_dir: config.storage_dir.clone(),
            roots: config.index.roots.clone(),
        }
    }

    fn requires_reconcile(&self, config: &Config) -> bool {
        self != &Self::from_config(config)
    }
}

struct PendingStandingSlice {
    receiver: tokio::sync::oneshot::Receiver<crate::protocol::Response>,
    started_at: Instant,
    generation: u64,
}

struct StandingScheduleState {
    queue: DeficitRoundRobin<String>,
    entries: HashMap<String, StandingRootEntry>,
    next_kind: HashMap<String, usize>,
    pending: HashMap<String, PendingStandingSlice>,
    resource_gate: ResourceAdmissionGate,
    completed_slices: u64,
    yielded_slices: u64,
    pause_reason: Option<String>,
    resource_policy: String,
}

impl Default for StandingScheduleState {
    fn default() -> Self {
        Self {
            queue: DeficitRoundRobin::new(STANDING_SERVICE_QUANTUM_MS),
            entries: HashMap::new(),
            next_kind: HashMap::new(),
            pending: HashMap::new(),
            resource_gate: ResourceAdmissionGate::default(),
            completed_slices: 0,
            yielded_slices: 0,
            pause_reason: None,
            resource_policy: "balanced".to_string(),
        }
    }
}

pub(super) struct StandingActor {
    app: Arc<App>,
    executor: Arc<Executor>,
    roots: StandingRoots,
    observed_config: Mutex<Config>,
    reconciled_config: Mutex<Option<StandingReconcileKey>>,
    /// Root ids registered solely to host unbound standing work. Session actors
    /// are never removed by this owner.
    owned_actors: Mutex<HashMap<String, (ProjectRootId, bool)>>,
    schedule: Mutex<StandingScheduleState>,
    semantic_cache: Arc<Mutex<HashMap<String, SemanticBuildCache>>>,
}

impl StandingActor {
    pub(super) fn new(app: Arc<App>, executor: Arc<Executor>) -> Self {
        Self {
            app,
            executor,
            roots: StandingRoots::default(),
            observed_config: Mutex::new(Config::default()),
            reconciled_config: Mutex::new(None),
            owned_actors: Mutex::new(HashMap::new()),
            schedule: Mutex::new(StandingScheduleState::default()),
            semantic_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Startup reconciliation is intentionally direct and empty until subc has
    /// observed a user-tier configuration snapshot from a successful RouteBind.
    pub(super) fn reconcile_at_startup(&self) {
        let config = Config::default();
        match self.roots.reconcile(&config) {
            Ok(_) => {
                *self.reconciled_config.lock() = Some(StandingReconcileKey::from_config(&config))
            }
            Err(error) => log::warn!("standing roots startup reconciliation failed: {error}"),
        }
    }

    /// Observe the current configuration only at a subc boundary. Session actor
    /// contexts provide validated user-tier snapshots; unbound contexts created
    /// by this actor have no harness marker and cannot preserve a retired config.
    pub(super) fn observe_config_snapshot(&self) {
        let owned = self.owned_actors.lock().clone();
        let mut snapshots = self
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
            .map(|(root_id, ctx)| (root_id, ctx.config().as_ref().clone()))
            .collect::<Vec<_>>();
        snapshots.sort_by(|(left, _), (right, _)| left.as_path().cmp(right.as_path()));
        if let Some((_, snapshot)) = snapshots.into_iter().next() {
            *self.observed_config.lock() = snapshot;
        }
    }

    /// Begin the standing half of a session bind before the session owns the
    /// shared artifact family. The bounded wait is part of the bind transition,
    /// not a timer or scheduler, and stale standing publication remains fenced
    /// even if a worker reaches its checkpoint late.
    pub(super) fn begin_session_bind(&self, ctx: &AppContext) {
        let snapshot = ctx.config();
        let snapshot = snapshot.as_ref().clone();
        let report = match self.roots.reconcile(&snapshot) {
            Ok(report) => report,
            Err(error) => {
                log::warn!("standing roots bind reconciliation refused: {error}");
                return;
            }
        };
        self.retire_removed_actors(&report.removed);
        *self.reconciled_config.lock() = Some(StandingReconcileKey::from_config(&snapshot));
        let Some(session_root) = ctx
            .canonical_cache_root_opt()
            .or_else(|| snapshot.project_root.clone())
        else {
            return;
        };
        let session_key = ctx.memoized_artifact_cache_key(&session_root);
        for entry in self
            .roots
            .entries()
            .into_iter()
            .filter(|entry| entry.artifact_key == session_key)
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
    /// Reconcile configured roots, collect completed slices, and fill available slots.
    pub(super) fn tick(&self) {
        self.observe_config_snapshot();
        let snapshot = self.observed_config.lock().clone();
        let reconcile_key = StandingReconcileKey::from_config(&snapshot);
        let entries = if self
            .reconciled_config
            .lock()
            .as_ref()
            .is_none_or(|previous| previous.requires_reconcile(&snapshot))
        {
            let report = match self.roots.reconcile(&snapshot) {
                Ok(report) => report,
                Err(error) => {
                    log::warn!("standing roots reconciliation refused: {error}");
                    return;
                }
            };
            self.retire_removed_actors(&report.removed);
            *self.reconciled_config.lock() = Some(reconcile_key);
            report.active_entries
        } else {
            self.roots.entries()
        };
        self.resume_entries_without_bound_session(&entries);
        self.reconcile_schedule(entries);
        self.drain_completed_slices();
        let decision = self
            .schedule
            .lock()
            .resource_gate
            .observe(snapshot.index.resource_policy, sample_resources());
        {
            let mut schedule = self.schedule.lock();
            schedule.resource_policy = snapshot.index.resource_policy.as_str().to_string();
            schedule.pause_reason = match decision {
                AdmissionDecision::Admit => None,
                AdmissionDecision::Paused(reason) => Some(format!("{reason:?}").to_lowercase()),
            };
            Self::publish_schedule_telemetry(&schedule);
        }
        if decision == AdmissionDecision::Admit {
            self.dispatch_ready_slices(&snapshot);
        }
    }

    fn reconcile_schedule(&self, entries: Vec<StandingRootEntry>) {
        let mut schedule = self.schedule.lock();
        let keys = entries
            .iter()
            .map(|entry| entry.literal_path.clone())
            .collect::<Vec<_>>();
        schedule.queue.reconcile(keys.iter().cloned());
        Self::reconcile_kind_cursors(&mut schedule, &entries);
        schedule.entries = entries
            .into_iter()
            .map(|entry| (entry.literal_path.clone(), entry))
            .collect();
        Self::publish_schedule_telemetry(&schedule);
    }

    fn reconcile_kind_cursors(schedule: &mut StandingScheduleState, entries: &[StandingRootEntry]) {
        schedule
            .next_kind
            .retain(|key, _| entries.iter().any(|entry| entry.literal_path == *key));
        for entry in entries {
            let selection_changed = schedule
                .entries
                .get(&entry.literal_path)
                .is_some_and(|previous| previous.indexes != entry.indexes);
            if selection_changed {
                schedule.next_kind.insert(entry.literal_path.clone(), 0);
            } else {
                schedule
                    .next_kind
                    .entry(entry.literal_path.clone())
                    .or_insert(0);
            }
        }
    }

    fn publish_schedule_telemetry(schedule: &StandingScheduleState) {
        crate::standing_scheduler::publish_telemetry(
            crate::standing_scheduler::StandingSchedulerTelemetry {
                queued_roots: schedule.queue.len().saturating_sub(schedule.pending.len()),
                running_slices: schedule.pending.len(),
                completed_slices: schedule.completed_slices,
                yielded_slices: schedule.yielded_slices,
                pause_reason: schedule.pause_reason.clone(),
                resource_policy: schedule.resource_policy.clone(),
            },
        );
    }

    fn drain_completed_slices(&self) {
        let mut schedule = self.schedule.lock();
        let completed = schedule
            .pending
            .iter_mut()
            .filter_map(|(key, pending)| match pending.receiver.try_recv() {
                Ok(response) => Some((
                    key.clone(),
                    pending.generation,
                    response,
                    pending.started_at.elapsed(),
                )),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => Some((
                    key.clone(),
                    pending.generation,
                    crate::protocol::Response::error(
                        "standing",
                        "standing_slice_closed",
                        "standing slice response channel closed",
                    ),
                    pending.started_at.elapsed(),
                )),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            })
            .collect::<Vec<_>>();
        for (key, generation, response, elapsed) in completed {
            schedule.pending.remove(&key);
            let has_more = response
                .data
                .get("has_more")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let kind_complete = response
                .data
                .get("kind_complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if kind_complete {
                let next = schedule.next_kind.get(&key).copied().unwrap_or(0) + 1;
                schedule.next_kind.insert(key.clone(), next);
            }
            if !has_more {
                schedule.next_kind.insert(key.clone(), 0);
            }
            let cost = u64::try_from(elapsed.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            schedule.completed_slices = schedule.completed_slices.saturating_add(1);
            if response
                .data
                .get("yielded")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                schedule.yielded_slices = schedule.yielded_slices.saturating_add(1);
            }
            Self::publish_schedule_telemetry(&schedule);
            schedule
                .queue
                .complete_generation(key, generation, cost, has_more);
        }
    }

    fn dispatch_ready_slices(&self, snapshot: &Config) {
        loop {
            let (entry, generation) = {
                let mut schedule = self.schedule.lock();
                if schedule.pending.len() >= crate::cold_build_limiter::limit() {
                    return;
                }
                let Some(key) = schedule.queue.next() else {
                    return;
                };
                let generation = schedule.queue.generation(&key).unwrap_or(0);
                let Some(entry) = schedule.entries.get(&key).cloned() else {
                    schedule
                        .queue
                        .complete_generation(key, generation, 1, false);
                    continue;
                };
                (entry, generation)
            };
            let Some(receiver) = self.submit_entry_slice(entry.clone(), snapshot) else {
                self.schedule
                    .lock()
                    .queue
                    .complete(entry.literal_path, 1, true);
                continue;
            };
            let mut schedule = self.schedule.lock();
            schedule.pending.insert(
                entry.literal_path,
                PendingStandingSlice {
                    receiver,
                    started_at: Instant::now(),
                    generation,
                },
            );
            Self::publish_schedule_telemetry(&schedule);
        }
    }

    /// Session selection and standing selection are exclusive for one shared
    /// artifact key. A session unbind is detected from the normal subc lifecycle
    /// state and resumes only the paused standing lifecycle.
    fn resume_entries_without_bound_session(&self, entries: &[StandingRootEntry]) {
        let sessions = self
            .executor
            .actor_entries()
            .into_iter()
            .filter_map(|(_, ctx)| {
                if ctx.subc_unbound_quiesced() {
                    return None;
                }
                let root = ctx
                    .canonical_cache_root_opt()
                    .or_else(|| ctx.config().project_root.clone())?;
                Some(ctx.memoized_artifact_cache_key(&root))
            })
            .collect::<Vec<_>>();
        for entry in entries {
            if !sessions.contains(&entry.artifact_key) {
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

    fn submit_entry_slice(
        &self,
        entry: StandingRootEntry,
        snapshot: &Config,
    ) -> Option<tokio::sync::oneshot::Receiver<crate::protocol::Response>> {
        let root_id = self.ensure_actor(&entry, snapshot)?;
        let kind_index = *self
            .schedule
            .lock()
            .next_kind
            .get(&entry.literal_path)
            .unwrap_or(&0);
        let semantic_cache = Arc::clone(&self.semantic_cache);
        let selected = IndexKind::ALL
            .iter()
            .copied()
            .enumerate()
            .skip(kind_index)
            .find(|(_, kind)| entry.indexes.contains(kind));
        let Some((kind_index, kind)) = selected else {
            return None;
        };

        let roots = self.roots.clone();
        let literal_path = entry.literal_path.clone();
        let executor_request_id = format!("subc-standing-slice-{}-{}", literal_path, kind.as_str());
        let response_request_id = executor_request_id.clone();
        let job = Box::new(move |ctx: &AppContext| {
            let Some(admission) = roots.admit_build(&literal_path) else {
                return crate::protocol::Response::success(
                    response_request_id,
                    serde_json::json!({"standing": true, "entry": literal_path, "admitted": false, "has_more": true}),
                );
            };
            let Some(permit) = crate::cold_build_limiter::try_acquire_standing_with_limiter(
                &ctx.cold_build_limiter(),
                format!("standing:{}", literal_path),
                admission.publication.admission_epoch,
            ) else {
                return crate::protocol::Response::success(
                    response_request_id,
                    serde_json::json!({"standing": true, "entry": literal_path, "admitted": false, "yielded": true, "has_more": true}),
                );
            };
            debug_assert_eq!(
                permit.admission_epoch,
                admission.publication.admission_epoch
            );
            let (kind_complete, yielded) = if crate::executor::current_job_cancelled() {
                (false, true)
            } else if strict_verify_current_state(ctx, &entry, kind) {
                (true, false)
            } else if kind == IndexKind::Search {
                build_missing_search_after_strict_check(
                    ctx,
                    &roots,
                    &entry,
                    &admission,
                    permit.admission_epoch,
                )
            } else if kind == IndexKind::Semantic {
                build_missing_semantic_after_strict_check(
                    ctx,
                    &roots,
                    &entry,
                    &admission,
                    permit.admission_epoch,
                    &semantic_cache,
                )
            } else {
                build_missing_callgraph_after_strict_check(
                    ctx,
                    &roots,
                    &entry,
                    &admission,
                    permit.admission_epoch,
                )
            };
            if kind_complete {
                if let Err(error) = roots.record_strict_verification(&literal_path, kind) {
                    log::warn!(
                        "standing strict verification outcome could not commit for {} {}: {}",
                        literal_path,
                        kind.as_str(),
                        error
                    );
                }
            }
            let has_later_kind = entry.indexes.iter().any(|candidate| {
                IndexKind::ALL
                    .iter()
                    .position(|kind| kind == candidate)
                    .is_some_and(|index| index > kind_index)
            });
            let has_more = !kind_complete || has_later_kind;
            crate::protocol::Response::success(
                response_request_id,
                serde_json::json!({
                    "standing": true,
                    "entry": literal_path,
                    "kind": kind.as_str(),
                    "kind_complete": kind_complete,
                    "yielded": yielded,
                    "has_more": has_more,
                }),
            )
        });
        Some(self.executor.submit_coalescable_maintenance_async(
            root_id,
            Lane::MaintenanceCommit,
            executor_request_id,
            MaintenanceCoalesceKey::StandingPass,
            job,
        ))
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
        config.search_index = entry.indexes.contains(&IndexKind::Search);
        config.semantic_search = entry.indexes.contains(&IndexKind::Semantic);
        config.callgraph_store = entry.indexes.contains(&IndexKind::Callgraph);
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
                ctx.config().storage_dir.as_deref(),
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
) -> (bool, bool) {
    if admission.cancellation_requested() || crate::executor::current_job_cancelled() {
        return (false, true);
    }
    let config = ctx.config();
    let cache_dir = crate::search_index::resolve_cache_dir_with_key(
        &entry.artifact_key,
        config.storage_dir.as_deref(),
    );
    let max_file_size = config.search_index_max_file_size;
    drop(config);
    let configure_generation = ctx.configure_generation();
    let lease = match crate::root_cache::WriterLease::acquire_shared(
        crate::root_cache::RootCacheDomain::Index,
        &cache_dir,
        &entry.artifact_key,
        &entry.resolved_target,
    ) {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(_) => return (false, true),
    };
    let outcome = roots
        .publish_if_current(
            &entry.literal_path,
            admission.publication,
            &lease,
            || true,
            || {
                permit_epoch == admission.publication.admission_epoch
                    && ctx.configure_generation() == configure_generation
                    && !admission.cancellation_requested()
            },
            || {
                crate::search_index::SearchIndex::resume_cold_build_slice(
                    &entry.resolved_target,
                    max_file_size,
                    &cache_dir,
                )
                .ok()
            },
        )
        .ok()
        .flatten()
        .flatten();
    match outcome {
        Some(crate::search_index::SearchBuildSliceOutcome::Complete) => (true, false),
        Some(crate::search_index::SearchBuildSliceOutcome::Yielded) | None => (false, true),
    }
}

fn build_missing_semantic_after_strict_check(
    ctx: &AppContext,
    roots: &StandingRoots,
    entry: &StandingRootEntry,
    admission: &crate::standing_roots::StandingBuildAdmission,
    permit_epoch: u64,
    cache: &Arc<Mutex<HashMap<String, SemanticBuildCache>>>,
) -> (bool, bool) {
    if admission.cancellation_requested() || crate::executor::current_job_cancelled() {
        return (false, true);
    }
    let config = ctx.config();
    let semantic_config = config.semantic.clone();
    let storage_dir = config.storage_dir.clone();
    let configure_generation = ctx.configure_generation();
    drop(config);
    let Some(storage_dir) = storage_dir else {
        return (false, true);
    };

    // Deny borrow-only roots before creating staging state or constructing an
    // embedding model. The writer lease is the artifact-level authority.
    let cache_dir = storage_dir.join("semantic").join(&entry.artifact_key);
    let lease = match crate::root_cache::WriterLease::acquire_shared(
        crate::root_cache::RootCacheDomain::Index,
        &cache_dir,
        &entry.artifact_key,
        &entry.resolved_target,
    ) {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(_) => return (false, true),
    };

    let config_key = serde_json::to_string(&semantic_config).unwrap_or_default();
    let files_key = format!(
        "{}:{}:{}",
        config_key, semantic_config.max_files, entry.artifact_key
    );
    let cached = {
        let mut cache = cache.lock();
        let needs_refresh = cache
            .get(&entry.literal_path)
            .is_none_or(|cached| cached.config_key != config_key || cached.files_key != files_key);
        if needs_refresh {
            let files = match crate::commands::configure::walk_semantic_project_files_bounded(
                &entry.resolved_target,
                semantic_config.max_files,
            ) {
                Ok(files) => Arc::new(files),
                Err(_) => return (false, true),
            };
            cache.insert(
                entry.literal_path.clone(),
                SemanticBuildCache {
                    config_key: config_key.clone(),
                    files_key: files_key.clone(),
                    files,
                    model: Arc::new(SyncMutex::new(None)),
                },
            );
        }
        cache
            .get(&entry.literal_path)
            .cloned()
            .expect("cache inserted")
    };
    let mut model_guard = cached
        .model
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if model_guard.is_none() {
        match crate::semantic_index::EmbeddingModel::from_config(&semantic_config) {
            Ok(model) => *model_guard = Some(model),
            Err(error) => {
                log::warn!("standing semantic model initialization failed: {}", error);
                return (false, true);
            }
        }
    }
    let model = model_guard.as_mut().expect("model initialized");
    let outcome = roots
        .publish_if_current(
            &entry.literal_path,
            admission.publication,
            &lease,
            || true,
            || {
                permit_epoch == admission.publication.admission_epoch
                    && ctx.configure_generation() == configure_generation
                    && !admission.cancellation_requested()
            },
            || {
                crate::semantic_index::SemanticIndex::resume_cold_build_slice(
                    &entry.resolved_target,
                    &cached.files,
                    model,
                    &semantic_config,
                    &storage_dir,
                    &entry.artifact_key,
                )
                .is_ok()
            },
        )
        .ok()
        .flatten();
    if outcome == Some(true) {
        // Force a fresh inventory at the start of the next complete build.
        cache.lock().remove(&entry.literal_path);
        (true, false)
    } else {
        (false, true)
    }
}

fn build_missing_callgraph_after_strict_check(
    ctx: &AppContext,
    roots: &StandingRoots,
    entry: &StandingRootEntry,
    admission: &crate::standing_roots::StandingBuildAdmission,
    permit_epoch: u64,
) -> (bool, bool) {
    if admission.cancellation_requested() || crate::executor::current_job_cancelled() {
        return (false, true);
    }
    let config = ctx.config();
    let storage_dir = config.storage_dir.clone();
    let configure_generation = ctx.configure_generation();
    drop(config);
    let Some(storage_dir) = storage_dir else {
        return (false, true);
    };
    let files = crate::callgraph::walk_project_files(&entry.resolved_target).collect::<Vec<_>>();
    let cache_dir = storage_dir.join("callgraph").join(&entry.artifact_key);
    let lease = match crate::root_cache::WriterLease::acquire_shared(
        crate::root_cache::RootCacheDomain::Callgraph,
        &cache_dir,
        &entry.artifact_key,
        &entry.resolved_target,
    ) {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(_) => return (false, true),
    };
    let outcome = roots
        .publish_if_current(
            &entry.literal_path,
            admission.publication,
            &lease,
            || true,
            || {
                permit_epoch == admission.publication.admission_epoch
                    && ctx.configure_generation() == configure_generation
                    && !admission.cancellation_requested()
            },
            || {
                matches!(
                    crate::callgraph_store::CallGraphStore::resume_cold_build_slice_with_lease(
                        cache_dir.clone(),
                        entry.resolved_target.clone(),
                        &files,
                        0,
                    ),
                    Ok(crate::callgraph_store::ColdBuildSlice::Complete { .. })
                )
            },
        )
        .ok()
        .flatten();
    match outcome {
        Some(true) => (true, false),
        Some(false) | None => (false, true),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standing_interval_uses_the_existing_subc_maintenance_cadence() {
        assert_eq!(
            STANDING_MAINTENANCE_INTERVAL,
            super::super::DRAIN_TICK_PERIOD
        );
    }

    #[test]
    fn unchanged_standing_config_does_not_require_reconciliation() {
        let mut config = Config::default();
        config.storage_dir = Some(std::path::PathBuf::from("/tmp/aft-standing-test"));
        config.index.roots.push(crate::config::IndexRootConfig {
            path: "/tmp/root".to_string(),
            indexes: vec![IndexKind::Search],
        });

        let key = StandingReconcileKey::from_config(&config);
        assert!(!key.requires_reconcile(&config));

        config.index.resource_policy = crate::config::IndexResourcePolicy::Performance;
        assert!(!key.requires_reconcile(&config));

        config.index.roots.push(crate::config::IndexRootConfig {
            path: "/tmp/root-two".to_string(),
            indexes: vec![IndexKind::Search],
        });
        assert!(key.requires_reconcile(&config));
    }

    #[test]
    fn index_selection_change_resets_kind_cursor() {
        let mut schedule = StandingScheduleState::default();
        let mut entry = StandingRootEntry {
            literal_path: "/tmp/root".to_string(),
            resolved_target: std::path::PathBuf::from("/tmp/root"),
            resolved_git_toplevel: None,
            scoped_relative_path: None,
            artifact_key: "root".to_string(),
            indexes: vec![IndexKind::Search, IndexKind::Semantic],
            config_order: 0,
        };
        schedule
            .entries
            .insert(entry.literal_path.clone(), entry.clone());
        schedule.next_kind.insert(entry.literal_path.clone(), 1);

        entry.indexes = vec![IndexKind::Search];
        StandingActor::reconcile_kind_cursors(&mut schedule, std::slice::from_ref(&entry));

        assert_eq!(schedule.next_kind.get(&entry.literal_path), Some(&0));
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
