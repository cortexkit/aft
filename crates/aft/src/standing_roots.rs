//! Durable lifecycle coordination for configured standing index roots.
//!
//! The subc standing actor is the only background owner. This module keeps the
//! lifecycle and publication decisions independent from transport details so a
//! configuration replacement, a session bind, and a delayed worker all consume
//! the same admission epoch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Condvar, Mutex};

use crate::config::{Config, IndexKind};
use crate::db::standing_roots::{self, StandingRootRecord};
use crate::root_cache::{ArtifactPublishEpoch, WriterLease};
use crate::scoped_key::{
    reject_duplicate_artifact_keys, resolve_standing_root, ResolvedStandingRoot,
};

/// Configuration snapshots are observed only at standing maintenance-pass
/// boundaries. This keeps a pass internally coherent while allowing the next
/// pass to apply a later configured selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingRootEntry {
    pub literal_path: String,
    pub resolved_target: PathBuf,
    pub resolved_git_toplevel: Option<PathBuf>,
    pub scoped_relative_path: Option<PathBuf>,
    pub artifact_key: String,
    pub indexes: Vec<IndexKind>,
    pub config_order: usize,
}

#[derive(Debug)]
pub enum StandingRootsError {
    Resolution(crate::scoped_key::ScopedKeyError),
    Database(standing_roots::StandingRootError),
    OpenDatabase(crate::db::OpenError),
    UnknownEntry { literal_path: String },
}

impl std::fmt::Display for StandingRootsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolution(error) => {
                write!(formatter, "standing root resolution failed: {error}")
            }
            Self::Database(error) => write!(formatter, "standing roots database error: {error}"),
            Self::OpenDatabase(error) => {
                write!(formatter, "could not open standing roots database: {error}")
            }
            Self::UnknownEntry { literal_path } => {
                write!(
                    formatter,
                    "standing root {literal_path:?} has no active lifecycle"
                )
            }
        }
    }
}

impl std::error::Error for StandingRootsError {}

impl From<crate::scoped_key::ScopedKeyError> for StandingRootsError {
    fn from(error: crate::scoped_key::ScopedKeyError) -> Self {
        Self::Resolution(error)
    }
}

impl From<standing_roots::StandingRootError> for StandingRootsError {
    fn from(error: standing_roots::StandingRootError) -> Self {
        Self::Database(error)
    }
}

impl From<crate::db::OpenError> for StandingRootsError {
    fn from(error: crate::db::OpenError) -> Self {
        Self::OpenDatabase(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingReconcileReport {
    pub active_entries: Vec<StandingRootEntry>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub replaced: Vec<String>,
}

/// The typed result used when a contained path has no selected index kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StandingRouteError {
    NoContainingEntry,
    KindUnavailable {
        deepest_literal_path: String,
        deepest_selection: Vec<IndexKind>,
    },
    StrictVerificationRequired {
        literal_path: String,
        kind: IndexKind,
    },
}

/// A successful route discloses the selected configuration entry rather than
/// implying that the deepest containing entry necessarily served the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingRoute {
    pub entry: StandingRootEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandingPublicationAdmission {
    pub admission_epoch: u64,
    publication_epoch: u64,
}

/// An admitted build retains a lifecycle reference until it reaches a
/// checkpoint or exits. Its epoch is copied into the limiter permit by the
/// subc actor, so a checkpoint reacquisition cannot lose publication identity.
pub struct StandingBuildAdmission {
    lifecycle: Arc<ArtifactLifecycle>,
    pub publication: StandingPublicationAdmission,
}

impl StandingBuildAdmission {
    /// A batch boundary must acknowledge bind cancellation before doing more
    /// work. The bind operation waits at most two seconds for this signal.
    pub fn checkpoint(&self) -> bool {
        self.lifecycle.checkpoint()
    }

    pub fn cancellation_requested(&self) -> bool {
        self.lifecycle.cancellation_requested()
    }
}

impl Drop for StandingBuildAdmission {
    fn drop(&mut self) {
        self.lifecycle.finish_build();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindTransition {
    pub admission_epoch: u64,
    pub cancelled_builds: usize,
}

/// The sole state owner for standing roots. `subc::standing` invokes this at
/// startup and from the existing subc maintenance timer; daemonless paths may
/// read the same durable rows but cannot start background maintenance here.
#[derive(Clone, Default)]
pub struct StandingRoots {
    inner: Arc<StandingRootsInner>,
}

#[derive(Default)]
struct StandingRootsInner {
    database_path: Mutex<Option<PathBuf>>,
    state: Mutex<StandingRootsState>,
}

#[derive(Default)]
struct StandingRootsState {
    entries: BTreeMap<String, ManagedEntry>,
    observed_first_snapshot: bool,
}

struct ManagedEntry {
    entry: StandingRootEntry,
    lifecycle: Arc<ArtifactLifecycle>,
}

struct ArtifactLifecycle {
    state: Mutex<ArtifactLifecycleState>,
    checkpoint_acknowledged: Condvar,
    admission_epoch: AtomicU64,
    publication_epoch: ArtifactPublishEpoch,
}

#[derive(Default)]
struct ArtifactLifecycleState {
    standing_admission_open: bool,
    bind_pending: bool,
    session_bound: bool,
    active_builds: usize,
    cancellation_requested: bool,
    checkpoint_acknowledged: bool,
}

impl Default for ArtifactLifecycle {
    fn default() -> Self {
        Self {
            state: Mutex::new(ArtifactLifecycleState {
                standing_admission_open: true,
                ..ArtifactLifecycleState::default()
            }),
            checkpoint_acknowledged: Condvar::new(),
            admission_epoch: AtomicU64::new(0),
            publication_epoch: ArtifactPublishEpoch::default(),
        }
    }
}

impl ArtifactLifecycle {
    fn admit(self: &Arc<Self>) -> Option<StandingBuildAdmission> {
        let mut state = self.state.lock();
        if !state.standing_admission_open || state.bind_pending || state.session_bound {
            return None;
        }
        state.active_builds += 1;
        Some(StandingBuildAdmission {
            lifecycle: Arc::clone(self),
            publication: StandingPublicationAdmission {
                admission_epoch: self.admission_epoch.load(Ordering::SeqCst),
                publication_epoch: self.publication_epoch.current(),
            },
        })
    }

    fn revoke_and_mint(&self, bind_pending: bool) -> u64 {
        let mut state = self.state.lock();
        state.bind_pending = bind_pending;
        state.session_bound = bind_pending;
        state.standing_admission_open = false;
        state.cancellation_requested = true;
        state.checkpoint_acknowledged = state.active_builds == 0;
        let admission_epoch = self
            .admission_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        // Advance the publication epoch so any admission captured before this
        // lifecycle transition is rejected by `StandingRoots::publish_if_current`
        // while ArtifactPublishEpoch holds its mutex.
        self.publication_epoch.next();
        if state.checkpoint_acknowledged {
            self.checkpoint_acknowledged.notify_all();
        }
        admission_epoch
    }

    fn bind_pending(&self) -> bool {
        self.state.lock().bind_pending
    }

    fn resume_unbound(&self) {
        let mut state = self.state.lock();
        state.bind_pending = false;
        state.session_bound = false;
        state.standing_admission_open = true;
        state.cancellation_requested = false;
        state.checkpoint_acknowledged = false;
    }

    fn checkpoint(&self) -> bool {
        let mut state = self.state.lock();
        if !state.cancellation_requested {
            return false;
        }
        state.checkpoint_acknowledged = true;
        self.checkpoint_acknowledged.notify_all();
        true
    }

    fn cancellation_requested(&self) -> bool {
        self.state.lock().cancellation_requested
    }

    fn finish_build(&self) {
        let mut state = self.state.lock();
        state.active_builds = state.active_builds.saturating_sub(1);
        if state.cancellation_requested && state.active_builds == 0 {
            state.checkpoint_acknowledged = true;
            self.checkpoint_acknowledged.notify_all();
        }
    }

    fn wait_for_checkpoint_acknowledgement(&self, timeout: Duration) -> bool {
        let mut state = self.state.lock();
        if state.checkpoint_acknowledged {
            return true;
        }
        self.checkpoint_acknowledged.wait_for(&mut state, timeout);
        state.checkpoint_acknowledged
    }
}

impl StandingRoots {
    /// Reconcile one validated user-tier configuration snapshot. A configuration
    /// replacement revokes stale admissions before any obsolete worker can
    /// publish; new kinds remain durably strict until their own verification.
    pub fn reconcile(
        &self,
        config: &Config,
    ) -> Result<StandingReconcileReport, StandingRootsError> {
        let entries = resolve_entries(config)?;
        let configured_db_path =
            crate::bash_background::storage_dir(config.storage_dir.as_deref()).join("aft.db");
        // An empty snapshot removes entries from the last observed user-tier
        // storage namespace; it must not silently switch to the process default
        // and leave durable rows behind in the former namespace.
        let db_path = {
            let mut current = self.inner.database_path.lock();
            let path = if entries.is_empty() {
                current.clone().unwrap_or(configured_db_path)
            } else {
                configured_db_path
            };
            *current = Some(path.clone());
            path
        };
        let mut conn = crate::db::open(&db_path)?;

        let mut state = self.inner.state.lock();
        let previous_first_snapshot = state.observed_first_snapshot;
        let previous = std::mem::take(&mut state.entries);
        let mut next = BTreeMap::new();
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut replaced = Vec::new();

        for entry in &entries {
            let resolved = resolve_standing_root(&entry.literal_path)?;
            let record = StandingRootRecord {
                literal_path: entry.literal_path.clone(),
                resolved_target: resolved.resolved_target,
                resolved_git_toplevel: resolved.resolved_git_toplevel,
                scoped_relative_path: resolved.scoped_relative_path,
            };
            standing_roots::ensure_standing_root(&mut conn, &record, &entry.indexes)?;

            if let Some(old) = previous.get(&entry.literal_path) {
                let changed = old.entry.resolved_target != entry.resolved_target
                    || old.entry.resolved_git_toplevel != entry.resolved_git_toplevel
                    || old.entry.scoped_relative_path != entry.scoped_relative_path
                    || old.entry.artifact_key != entry.artifact_key
                    || old.entry.indexes != entry.indexes;
                if changed {
                    old.lifecycle.revoke_and_mint(false);
                    old.lifecycle.resume_unbound();
                    replaced.push(entry.literal_path.clone());
                }
                next.insert(
                    entry.literal_path.clone(),
                    ManagedEntry {
                        entry: entry.clone(),
                        lifecycle: Arc::clone(&old.lifecycle),
                    },
                );
            } else {
                added.push(entry.literal_path.clone());
                next.insert(
                    entry.literal_path.clone(),
                    ManagedEntry {
                        entry: entry.clone(),
                        lifecycle: Arc::new(ArtifactLifecycle::default()),
                    },
                );
            }
        }

        for (literal_path, old) in &previous {
            if !next.contains_key(literal_path) {
                old.lifecycle.revoke_and_mint(false);
                standing_roots::delete_standing_root(&conn, literal_path)?;
                removed.push(literal_path.clone());
            }
        }

        // Daemon restart is an observation gap. Startup first reconciles an
        // empty default before subc has received a RouteBind configuration, so
        // the first non-empty observed snapshot marks every retained row strict.
        // Only the transactional strict-verification clear below can consume it.
        if !previous_first_snapshot && !entries.is_empty() {
            for entry in next.values() {
                mark_kinds_needing_strict_verify(
                    &mut conn,
                    &entry.entry.literal_path,
                    &entry.entry.indexes,
                )?;
            }
        }
        state.observed_first_snapshot |= !entries.is_empty();
        state.entries = next;

        Ok(StandingReconcileReport {
            active_entries: entries,
            added,
            removed,
            replaced,
        })
    }

    pub fn entries(&self) -> Vec<StandingRootEntry> {
        self.inner
            .state
            .lock()
            .entries
            .values()
            .map(|entry| entry.entry.clone())
            .collect()
    }

    /// Admit a standing build and capture its lifecycle epoch. The caller must
    /// carry `publication.admission_epoch` through its Standing limiter permit.
    pub fn admit_build(&self, literal_path: &str) -> Option<StandingBuildAdmission> {
        self.inner
            .state
            .lock()
            .entries
            .get(literal_path)
            .and_then(|entry| entry.lifecycle.admit())
    }

    /// Begin a session bind by revoking standing admissions, advancing the
    /// publication epoch, marking relevant freshness rows, and signaling
    /// cancellation. The bounded worker join occurs after releasing the lock.
    pub fn begin_case_a_bind(
        &self,
        literal_path: &str,
    ) -> Result<BindTransition, StandingRootsError> {
        let (lifecycle, indexes) = {
            let state = self.inner.state.lock();
            let entry = state.entries.get(literal_path).ok_or_else(|| {
                StandingRootsError::UnknownEntry {
                    literal_path: literal_path.to_string(),
                }
            })?;
            (Arc::clone(&entry.lifecycle), entry.entry.indexes.clone())
        };
        let admission_epoch = lifecycle.revoke_and_mint(true);
        let mut conn = self.open_database()?;
        mark_kinds_needing_strict_verify(&mut conn, literal_path, &indexes)?;
        let cancelled_builds = lifecycle.state.lock().active_builds;
        Ok(BindTransition {
            admission_epoch,
            cancelled_builds,
        })
    }

    /// Bind may wait for one checkpoint acknowledgement but never indefinitely.
    pub fn wait_for_case_a_checkpoint(
        &self,
        literal_path: &str,
    ) -> Result<bool, StandingRootsError> {
        let lifecycle = self.lifecycle(literal_path)?;
        Ok(lifecycle.wait_for_checkpoint_acknowledgement(Duration::from_secs(2)))
    }

    /// Resume standing ownership after a session unbind. Any standing kind that
    /// the session did not prove current is set strict before the next pass.
    pub fn resume_after_session(
        &self,
        literal_path: &str,
        session_maintained: &[IndexKind],
    ) -> Result<(), StandingRootsError> {
        let (lifecycle, missing) = {
            let state = self.inner.state.lock();
            let entry = state.entries.get(literal_path).ok_or_else(|| {
                StandingRootsError::UnknownEntry {
                    literal_path: literal_path.to_string(),
                }
            })?;
            let missing = entry
                .entry
                .indexes
                .iter()
                .copied()
                .filter(|kind| !session_maintained.contains(kind))
                .collect::<Vec<_>>();
            (Arc::clone(&entry.lifecycle), missing)
        };
        if !lifecycle.bind_pending() {
            return Ok(());
        }
        if !missing.is_empty() {
            let mut conn = self.open_database()?;
            mark_kinds_needing_strict_verify(&mut conn, literal_path, &missing)?;
        }
        lifecycle.resume_unbound();
        Ok(())
    }

    /// Mark an observation gap such as suspension/resume, a watcher gap (for a
    /// root that has a watcher), or CLI-snapshot-to-query handoff. This durable
    /// set is paired with `record_strict_verification` below.
    pub fn mark_observation_gap(
        &self,
        literal_path: &str,
        kinds: &[IndexKind],
    ) -> Result<(), StandingRootsError> {
        let mut conn = self.open_database()?;
        mark_kinds_needing_strict_verify(&mut conn, literal_path, kinds)
    }

    /// Clear a durable gap only after a successful current-state strict verify.
    /// The outcome timestamp and clear share one SQLite transaction in the
    /// standing-roots database API, so a crash before commit leaves the flag set.
    pub fn record_strict_verification(
        &self,
        literal_path: &str,
        kind: IndexKind,
    ) -> Result<(), StandingRootsError> {
        let mut conn = self.open_database()?;
        standing_roots::record_successful_strict_verification(
            &mut conn,
            literal_path,
            kind,
            now_ms(),
        )?;
        Ok(())
    }

    /// Route an explicit path through overlapping entries by deepest recorded
    /// path then configuration order. If no candidate supports the requested
    /// kind, preserve the deepest entry in the typed unavailable result.
    pub fn route_explicit_path(
        &self,
        query_path: &Path,
        kind: IndexKind,
    ) -> Result<StandingRoute, StandingRouteError> {
        let query_path = canonicalize_existing_ancestor(query_path);
        let mut candidates = self
            .entries()
            .into_iter()
            .filter(|entry| query_path.starts_with(&entry.resolved_target))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            path_depth(&right.resolved_target)
                .cmp(&path_depth(&left.resolved_target))
                .then_with(|| left.config_order.cmp(&right.config_order))
        });
        let Some(deepest) = candidates.first().cloned() else {
            return Err(StandingRouteError::NoContainingEntry);
        };
        let Some(entry) = candidates
            .into_iter()
            .find(|entry| entry.indexes.contains(&kind))
        else {
            return Err(StandingRouteError::KindUnavailable {
                deepest_literal_path: deepest.literal_path,
                deepest_selection: deepest.indexes,
            });
        };
        let conn = self
            .open_database()
            .map_err(|_| StandingRouteError::NoContainingEntry)?;
        if standing_roots::needs_strict_verify(&conn, &entry.literal_path, kind)
            .ok()
            .flatten()
            .unwrap_or(true)
        {
            return Err(StandingRouteError::StrictVerificationRequired {
                literal_path: entry.literal_path,
                kind,
            });
        }
        Ok(StandingRoute { entry })
    }

    /// Execute a standing publication while `ArtifactPublishEpoch` holds its
    /// exclusive per-root mutex continuously across WriterLease validation,
    /// admission comparison, caller-supplied fingerprint/generation comparisons,
    /// and the final rename closure. A failed comparison is a no-op.
    pub fn publish_if_current<R>(
        &self,
        literal_path: &str,
        admission: StandingPublicationAdmission,
        writer_lease: &WriterLease,
        fingerprint_is_current: impl FnOnce() -> bool,
        generation_is_current: impl FnOnce() -> bool,
        publish_and_rename: impl FnOnce() -> R,
    ) -> Result<Option<R>, StandingRootsError> {
        let lifecycle = self.lifecycle(literal_path)?;
        Ok(lifecycle
            .publication_epoch
            .run_if_current(admission.publication_epoch, || {
                // Compare the worker's captured admission epoch with the epoch
                // advanced by a bind or configuration replacement. A mismatch
                // rejects stale publication before the final rename can run.
                if lifecycle.admission_epoch.load(Ordering::SeqCst) != admission.admission_epoch
                    || !lifecycle.state.lock().standing_admission_open
                    || !writer_lease.verify().unwrap_or(false)
                    || !fingerprint_is_current()
                    || !generation_is_current()
                {
                    return None;
                }
                Some(publish_and_rename())
            })
            .flatten())
    }

    fn lifecycle(&self, literal_path: &str) -> Result<Arc<ArtifactLifecycle>, StandingRootsError> {
        self.inner
            .state
            .lock()
            .entries
            .get(literal_path)
            .map(|entry| Arc::clone(&entry.lifecycle))
            .ok_or_else(|| StandingRootsError::UnknownEntry {
                literal_path: literal_path.to_string(),
            })
    }

    fn open_database(&self) -> Result<crate::db::TrackedConnection, StandingRootsError> {
        let path = self
            .inner
            .database_path
            .lock()
            .clone()
            .unwrap_or_else(|| crate::bash_background::storage_dir(None).join("aft.db"));
        Ok(crate::db::open(&path)?)
    }
}

fn resolve_entries(config: &Config) -> Result<Vec<StandingRootEntry>, StandingRootsError> {
    let resolved = config
        .index
        .roots
        .iter()
        .map(|root| resolve_standing_root(&root.path))
        .collect::<Result<Vec<ResolvedStandingRoot>, _>>()?;
    reject_duplicate_artifact_keys(&resolved)?;
    Ok(config
        .index
        .roots
        .iter()
        .zip(resolved)
        .enumerate()
        .map(|(config_order, (root, resolved))| StandingRootEntry {
            literal_path: root.path.clone(),
            resolved_target: PathBuf::from(resolved.resolved_target),
            resolved_git_toplevel: resolved.resolved_git_toplevel.map(PathBuf::from),
            scoped_relative_path: resolved.scoped_relative_path.map(PathBuf::from),
            artifact_key: resolved.artifact_key,
            indexes: root.indexes.clone(),
            config_order,
        })
        .collect())
}

fn mark_kinds_needing_strict_verify(
    conn: &mut rusqlite::Connection,
    literal_path: &str,
    kinds: &[IndexKind],
) -> Result<(), StandingRootsError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(standing_roots::StandingRootError::from)?;
    for kind in kinds {
        let updated = tx
            .execute(
                "UPDATE standing_root_freshness
             SET needs_strict_verify = 1, strict_verified_at = NULL
             WHERE literal_path = ?1 AND index_kind = ?2",
                rusqlite::params![literal_path, kind.as_str()],
            )
            .map_err(standing_roots::StandingRootError::from)?;
        if updated != 1 {
            return Err(StandingRootsError::Database(
                standing_roots::StandingRootError::MissingFreshnessRow {
                    literal_path: literal_path.to_string(),
                    kind: *kind,
                },
            ));
        }
    }
    tx.commit()
        .map_err(standing_roots::StandingRootError::from)?;
    Ok(())
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

/// Route containment must compare the same canonical spelling recorded for an
/// entry, while still accepting an explicit path whose final file is absent.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut missing = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return path.to_path_buf();
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
    let mut canonical = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    canonical
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IndexConfig, IndexRootConfig};
    use tempfile::tempdir;

    fn config(storage: &Path, roots: Vec<IndexRootConfig>) -> Config {
        Config {
            storage_dir: Some(storage.to_path_buf()),
            index: IndexConfig { roots },
            ..Config::default()
        }
    }

    fn root(path: &Path, indexes: Vec<IndexKind>) -> IndexRootConfig {
        IndexRootConfig {
            path: path.to_string_lossy().into_owned(),
            indexes,
        }
    }

    #[test]
    fn reconciliation_marks_restart_and_new_kind_for_strict_verification() {
        let storage = tempdir().unwrap();
        let first = tempdir().unwrap();
        let roots = StandingRoots::default();
        let mut cfg = config(
            storage.path(),
            vec![root(first.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&cfg).unwrap();
        roots
            .record_strict_verification(first.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        cfg.index.roots[0].indexes = vec![IndexKind::Search, IndexKind::Callgraph];
        let report = roots.reconcile(&cfg).unwrap();
        assert_eq!(report.replaced, vec![first.path().to_string_lossy()]);
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        assert!(standing_roots::needs_strict_verify(
            &conn,
            first.path().to_str().unwrap(),
            IndexKind::Callgraph
        )
        .unwrap()
        .unwrap());
    }

    #[test]
    fn daemon_startup_empty_pass_marks_existing_snapshot_strict_on_first_configured_pass() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let first = StandingRoots::default();
        first.reconcile(&cfg).unwrap();
        first
            .record_strict_verification(root_dir.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();

        let restarted = StandingRoots::default();
        restarted.reconcile(&Config::default()).unwrap();
        restarted.reconcile(&cfg).unwrap();
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        assert!(standing_roots::needs_strict_verify(
            &conn,
            root_dir.path().to_str().unwrap(),
            IndexKind::Search
        )
        .unwrap()
        .unwrap());
    }

    #[test]
    fn maintenance_snapshot_preserves_configuration_and_fixed_kind_order() {
        let storage = tempdir().unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![
                root(first.path(), vec![IndexKind::Callgraph, IndexKind::Search]),
                root(second.path(), vec![IndexKind::Semantic]),
            ],
        );
        let report = roots.reconcile(&cfg).unwrap();
        let scheduled = report
            .active_entries
            .iter()
            .flat_map(|entry| {
                IndexKind::ALL
                    .into_iter()
                    .filter(move |kind| entry.indexes.contains(kind))
                    .map(move |kind| (entry.literal_path.clone(), kind))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            scheduled,
            vec![
                (
                    first.path().to_string_lossy().into_owned(),
                    IndexKind::Search
                ),
                (
                    first.path().to_string_lossy().into_owned(),
                    IndexKind::Callgraph
                ),
                (
                    second.path().to_string_lossy().into_owned(),
                    IndexKind::Semantic
                ),
            ]
        );
    }

    #[test]
    fn route_falls_back_to_shallower_entry_and_discloses_it() {
        let storage = tempdir().unwrap();
        let outer = tempdir().unwrap();
        let inner = outer.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![
                root(outer.path(), vec![IndexKind::Search]),
                root(&inner, vec![IndexKind::Callgraph]),
            ],
        );
        roots.reconcile(&cfg).unwrap();
        roots
            .record_strict_verification(outer.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        let route = roots
            .route_explicit_path(&inner.join("file.rs"), IndexKind::Search)
            .unwrap();
        assert_eq!(route.entry.literal_path, outer.path().to_string_lossy());
    }

    #[test]
    fn publication_is_a_noop_after_bind_epoch_revocation() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut report = roots.reconcile(&cfg).unwrap();
        let entry = report.active_entries.remove(0);
        let build = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        roots.begin_case_a_bind(&entry.literal_path).unwrap();
        let published = std::sync::atomic::AtomicBool::new(false);
        let result = roots
            .publish_if_current(
                &entry.literal_path,
                build.publication,
                &lease,
                || true,
                || true,
                || published.store(true, Ordering::SeqCst),
            )
            .unwrap();
        assert!(result.is_none());
        assert!(!published.load(Ordering::SeqCst));
    }

    #[test]
    fn publication_fence_holds_epoch_mutex_through_final_rename() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut report = roots.reconcile(&cfg).unwrap();
        let entry = report.active_entries.remove(0);
        let admission = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        let (rename_entered_tx, rename_entered_rx) = std::sync::mpsc::channel();
        let (release_rename_tx, release_rename_rx) = std::sync::mpsc::channel();
        let publish_roots = roots.clone();
        let publish_literal = entry.literal_path.clone();
        let publisher = std::thread::spawn(move || {
            publish_roots
                .publish_if_current(
                    &publish_literal,
                    admission.publication,
                    &lease,
                    || true,
                    || true,
                    || {
                        rename_entered_tx.send(()).unwrap();
                        release_rename_rx.recv().unwrap();
                    },
                )
                .unwrap()
        });
        rename_entered_rx.recv().unwrap();
        let (bind_done_tx, bind_done_rx) = std::sync::mpsc::channel();
        let bind_roots = roots.clone();
        let bind_literal = entry.literal_path.clone();
        let binder = std::thread::spawn(move || {
            bind_done_tx
                .send(bind_roots.begin_case_a_bind(&bind_literal))
                .unwrap();
        });
        assert!(bind_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        release_rename_tx.send(()).unwrap();
        assert!(publisher.join().unwrap().is_some());
        bind_done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        binder.join().unwrap();
    }

    #[test]
    fn superseded_snapshot_publication_is_a_noop() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let mut cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut initial = roots.reconcile(&cfg).unwrap();
        let entry = initial.active_entries.remove(0);
        let build = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        cfg.index.roots[0].indexes = vec![IndexKind::Semantic];
        let report = roots.reconcile(&cfg).unwrap();
        assert_eq!(report.replaced, vec![entry.literal_path.clone()]);
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        assert!(roots
            .publish_if_current(
                &entry.literal_path,
                build.publication,
                &lease,
                || true,
                || true,
                || (),
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn crash_before_freshness_clear_blocks_verify_on_query_until_transactional_clear() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let first = StandingRoots::default();
        first.reconcile(&cfg).unwrap();
        first
            .record_strict_verification(root_dir.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        first
            .mark_observation_gap(root_dir.path().to_str().unwrap(), &[IndexKind::Search])
            .unwrap();
        drop(first);

        // Restarting between verification and its transaction commit must retain
        // the durable gap and reject a query rather than serving its baseline.
        let restarted = StandingRoots::default();
        restarted.reconcile(&cfg).unwrap();
        assert!(matches!(
            restarted.route_explicit_path(&root_dir.path().join("missing.rs"), IndexKind::Search),
            Err(StandingRouteError::StrictVerificationRequired { .. })
        ));
        restarted
            .record_strict_verification(root_dir.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        assert!(restarted
            .route_explicit_path(&root_dir.path().join("missing.rs"), IndexKind::Search)
            .is_ok());
    }

    #[test]
    fn verify_on_query_retains_observation_gap_between_passes() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let literal = root_dir.path().to_str().unwrap();
        roots.reconcile(&cfg).unwrap();
        roots
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        roots
            .mark_observation_gap(literal, &[IndexKind::Search])
            .unwrap();
        assert!(matches!(
            roots.route_explicit_path(&root_dir.path().join("query.rs"), IndexKind::Search),
            Err(StandingRouteError::StrictVerificationRequired { .. })
        ));
    }

    #[test]
    fn suspension_edit_resume_requires_strict_verification_before_query() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&cfg).unwrap();
        roots
            .record_strict_verification(root_dir.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        let build = roots
            .admit_build(root_dir.path().to_str().unwrap())
            .unwrap();
        roots
            .begin_case_a_bind(root_dir.path().to_str().unwrap())
            .unwrap();
        assert!(build.checkpoint());
        std::fs::write(root_dir.path().join("edited.rs"), "fn changed() {}\n").unwrap();
        roots
            .resume_after_session(root_dir.path().to_str().unwrap(), &[])
            .unwrap();
        assert!(matches!(
            roots.route_explicit_path(&root_dir.path().join("edited.rs"), IndexKind::Search),
            Err(StandingRouteError::StrictVerificationRequired { .. })
        ));
        roots
            .record_strict_verification(root_dir.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        assert!(roots
            .route_explicit_path(&root_dir.path().join("edited.rs"), IndexKind::Search)
            .is_ok());
    }

    #[test]
    fn shared_key_handoff_preserves_session_proven_kind_and_marks_other_kind() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(
                root_dir.path(),
                vec![IndexKind::Search, IndexKind::Semantic],
            )],
        );
        roots.reconcile(&cfg).unwrap();
        let literal = root_dir.path().to_str().unwrap();
        roots
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        roots
            .record_strict_verification(literal, IndexKind::Semantic)
            .unwrap();
        roots.begin_case_a_bind(literal).unwrap();
        // The session verified only the Search index. After it ends, Semantic
        // still needs a durable strict-verification record before it is eligible.
        roots
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        roots
            .resume_after_session(literal, &[IndexKind::Search])
            .unwrap();
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        assert!(
            !standing_roots::needs_strict_verify(&conn, literal, IndexKind::Search)
                .unwrap()
                .unwrap()
        );
        assert!(
            standing_roots::needs_strict_verify(&conn, literal, IndexKind::Semantic)
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn configuration_add_modify_and_remove_mint_boundaries_and_delete_rows() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let mut cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let literal = root_dir.path().to_str().unwrap().to_string();
        assert_eq!(roots.reconcile(&cfg).unwrap().added, vec![literal.clone()]);
        let admitted = roots.admit_build(&literal).unwrap();
        cfg.index.roots[0].indexes = vec![IndexKind::Callgraph];
        let report = roots.reconcile(&cfg).unwrap();
        assert_eq!(report.replaced, vec![literal.clone()]);
        let replacement = roots
            .admit_build(&literal)
            .expect("replacement reopens only a new admission");
        assert_ne!(
            admitted.publication.admission_epoch,
            replacement.publication.admission_epoch
        );
        let removed = roots.reconcile(&Config::default()).unwrap();
        assert_eq!(removed.removed, vec![literal.clone()]);
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        assert!(standing_roots::get_standing_root(&conn, &literal)
            .unwrap()
            .is_none());
    }

    #[test]
    fn bounded_join_proceeds_after_two_seconds_without_checkpoint() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&cfg).unwrap();
        let _build = roots
            .admit_build(root_dir.path().to_str().unwrap())
            .unwrap();
        roots
            .begin_case_a_bind(root_dir.path().to_str().unwrap())
            .unwrap();
        let before = std::time::Instant::now();
        assert!(!roots
            .wait_for_case_a_checkpoint(root_dir.path().to_str().unwrap())
            .unwrap());
        assert!(before.elapsed() >= Duration::from_secs(2));
        assert!(before.elapsed() < Duration::from_millis(2300));
    }

    #[test]
    fn bind_revokes_admission_and_checkpoint_join_is_bounded() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&cfg).unwrap();
        let build = roots
            .admit_build(root_dir.path().to_str().unwrap())
            .unwrap();
        let before = std::time::Instant::now();
        roots
            .begin_case_a_bind(root_dir.path().to_str().unwrap())
            .unwrap();
        assert!(build.checkpoint());
        assert!(roots
            .wait_for_case_a_checkpoint(root_dir.path().to_str().unwrap())
            .unwrap());
        assert!(before.elapsed() < Duration::from_secs(2));
        assert!(roots
            .admit_build(root_dir.path().to_str().unwrap())
            .is_none());
    }
}
