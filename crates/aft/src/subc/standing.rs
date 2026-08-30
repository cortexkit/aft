//! Subc-owned standing-root maintenance.
//!
//! This module deliberately has no watcher, scheduler, or timer. `subc::mod`
//! calls `tick` from its existing maintenance timer arm, and every root pass is
//! submitted through the existing executor's coalescable maintenance lane.

use std::collections::HashMap;
use std::sync::Arc;

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

#[cfg(test)]
static LAST_STANDING_VERIFY_STRATEGY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

pub(super) struct StandingActor {
    app: Arc<App>,
    executor: Arc<Executor>,
    roots: StandingRoots,
    observed_config: Mutex<Config>,
    /// Root ids registered solely to host unbound standing work. Session actors
    /// are never removed by this owner.
    owned_actors: Mutex<HashMap<String, (ProjectRootId, bool)>>,
}

impl StandingActor {
    pub(super) fn new(app: Arc<App>, executor: Arc<Executor>) -> Self {
        Self {
            app,
            executor,
            roots: StandingRoots::default(),
            observed_config: Mutex::new(Config::default()),
            owned_actors: Mutex::new(HashMap::new()),
        }
    }

    /// Startup reconciliation is intentionally direct and empty until subc has
    /// observed a user-tier configuration snapshot from a successful RouteBind.
    pub(super) fn reconcile_at_startup(&self) {
        if let Err(error) = self.roots.reconcile(&Config::default()) {
            log::warn!("standing roots startup reconciliation failed: {error}");
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
        if let Err(error) = self.roots.reconcile(&snapshot) {
            log::warn!("standing roots bind reconciliation refused: {error}");
            return;
        }
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

    /// Reconcile the observed snapshot and enqueue one coalesced pass per root.
    /// Entry order and `search`, `semantic`, `callgraph` kind order are retained
    /// by `StandingRoots::entries` and `IndexKind::ALL` respectively.
    pub(super) fn tick(&self) {
        self.observe_config_snapshot();
        let snapshot = self.observed_config.lock().clone();
        let report = match self.roots.reconcile(&snapshot) {
            Ok(report) => report,
            Err(error) => {
                log::warn!("standing roots reconciliation refused: {error}");
                return;
            }
        };

        self.retire_removed_actors(&report.removed);
        self.resume_entries_without_bound_session(&report.active_entries);
        for entry in report.active_entries {
            self.submit_entry_pass(entry, &snapshot);
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

    fn submit_entry_pass(&self, entry: StandingRootEntry, snapshot: &Config) {
        let Some(root_id) = self.ensure_actor(&entry, snapshot) else {
            return;
        };
        let roots = self.roots.clone();
        let literal_path = entry.literal_path.clone();
        let executor_request_id = format!("subc-standing-pass-{}", entry.literal_path);
        let response_request_id = executor_request_id.clone();
        let job = Box::new(move |ctx: &AppContext| {
            let Some(admission) = roots.admit_build(&literal_path) else {
                return crate::protocol::Response::success(
                    response_request_id,
                    serde_json::json!({"standing": true, "entry": literal_path, "admitted": false}),
                );
            };
            // Nonblocking standing admission: lifecycle admission stays inside
            // this serialized maintenance job, but the cold-build acquire is
            // immediate. When no slot is available the pass YIELDS — returning
            // without advancing publication state — and the 250ms standing
            // tick resubmits the coalesced pass. A yielded pass therefore never
            // occupies a maintenance worker waiting for a cold slot, so heavy
            // standing indexing cannot consume interactive reader capacity.
            let Some(permit) = crate::cold_build_limiter::try_acquire_standing_with_limiter(
                &ctx.cold_build_limiter(),
                format!("standing:{}", literal_path),
                admission.publication.admission_epoch,
            ) else {
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
) -> bool {
    if admission.cancellation_requested() || crate::executor::current_job_cancelled() {
        return false;
    }
    let config = ctx.config();
    let cache_dir = crate::search_index::resolve_cache_dir_with_key(
        &entry.artifact_key,
        config.storage_dir.as_deref(),
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
    use super::*;

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
