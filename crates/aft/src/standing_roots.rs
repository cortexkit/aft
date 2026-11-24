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
use crate::scoped_key::{reject_duplicate_artifact_keys, ResolvedStandingRoot};

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
    /// Configured roots this pass could not reconcile, in configuration order,
    /// with the error that refused each one. A refused root is left out of
    /// `active_entries` and admits no standing build; the other roots carry on.
    pub refused: Vec<StandingRootRefusal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingRootRefusal {
    pub literal_path: String,
    pub error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandingPublicationAdmission {
    pub admission_epoch: u64,
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
    reconciled_empty: bool,
}

struct ManagedEntry {
    entry: StandingRootEntry,
    lifecycle: Arc<ArtifactLifecycle>,
    /// The last reconciliation refused this root (for example its resolved
    /// path drifted), or could not delete the durable row of a root removed
    /// from configuration. The entry stays managed so a later pass can retry
    /// it, but it admits no standing build meanwhile.
    refused: bool,
}

struct ArtifactLifecycle {
    state: Mutex<ArtifactLifecycleState>,
    checkpoint_acknowledged: Condvar,
    admission_epoch: AtomicU64,
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
        if state.checkpoint_acknowledged {
            self.checkpoint_acknowledged.notify_all();
        }
        admission_epoch
    }

    fn bind_pending(&self) -> bool {
        self.state.lock().bind_pending
    }

    /// Stop standing work on a root that reconciliation refused: cancel running
    /// builds and fence their publication, keeping any session bind in place.
    fn refuse(&self) {
        let bound = {
            let state = self.state.lock();
            state.bind_pending || state.session_bound
        };
        self.revoke_and_mint(bound);
    }

    /// Reopen standing admission once a refused root reconciles again. A root
    /// that a session took over in the meantime stays with that session; its
    /// unbind resumes standing work as usual.
    fn recover_from_refusal(&self) {
        if !self.bind_pending() {
            self.resume_unbound();
        }
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
    /// Idle passes retain verified identity until repository evidence changes.
    pub(crate) fn reconcile_background(
        &self,
        config: &Config,
    ) -> Result<StandingReconcileReport, StandingRootsError> {
        self.reconcile_with_recheck(config, false)
    }

    /// Reconcile one validated user-tier configuration snapshot. A configuration
    /// replacement revokes stale admissions before any obsolete worker can
    /// publish; new kinds remain durably strict until their own verification.
    pub fn reconcile(
        &self,
        config: &Config,
    ) -> Result<StandingReconcileReport, StandingRootsError> {
        self.reconcile_with_recheck(config, true)
    }

    fn reconcile_with_recheck(
        &self,
        config: &Config,
        force: bool,
    ) -> Result<StandingReconcileReport, StandingRootsError> {
        let configured_db_path =
            crate::storage_path::storage_dir(config.storage_dir.as_deref()).join("aft.db");
        // An empty snapshot removes entries from the last observed user-tier
        // storage namespace; it must not silently switch to the process default
        // and leave durable rows behind in the former namespace.
        let db_path = {
            let mut current = self.inner.database_path.lock();
            let path = if config.index.roots.is_empty() {
                current.clone().unwrap_or(configured_db_path)
            } else {
                configured_db_path
            };
            *current = Some(path.clone());
            path
        };
        #[cfg(test)]
        let benchmark_baseline = RECONCILE_BENCH_BASELINE.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let benchmark_baseline = false;
        // Nonempty lists keep the database-open-before-state-lock path; the
        // extra mutex check is needed only to skip recurring empty-list work.
        if !benchmark_baseline && config.index.roots.is_empty() {
            #[cfg(test)]
            RECONCILE_PREOPEN_STATE_LOCKS.with(|count| count.set(count.get() + 1));
            let state = self.inner.state.lock();
            // Initialize aft.db in the chosen storage directory once. Removing
            // previously configured roots must still delete their rows and stop
            // their background builds. Only after an empty root list was applied
            // successfully is there no database work left until roots are added.
            if state.entries.is_empty() && state.reconciled_empty {
                return Ok(StandingReconcileReport {
                    active_entries: Vec::new(),
                    added: Vec::new(),
                    removed: Vec::new(),
                    replaced: Vec::new(),
                    refused: Vec::new(),
                });
            }
        }
        #[cfg(test)]
        RECONCILE_DB_OPENS.with(|count| count.set(count.get() + 1));
        let mut conn = match crate::db::open(&db_path) {
            Ok(conn) => conn,
            Err(error) => {
                // A failed storage switch has not initialized its database.
                // Do not let an earlier empty snapshot suppress later retries.
                self.inner.state.lock().reconciled_empty = false;
                return Err(error.into());
            }
        };
        // Validate the storage namespace before persisting first-bind identities.
        // A failed namespace switch must not inherit an earlier empty fast path.
        let entries = resolve_entries(config, force)?;
        #[cfg(test)]
        conn.trace(Some(|sql| {
            if sql == "BEGIN IMMEDIATE" {
                RECONCILE_IMMEDIATE.with(|count| count.set(count.get() + 1));
            }
        }));

        let mut state = self.inner.state.lock();
        state.reconciled_empty = false;
        let previous_first_snapshot = state.observed_first_snapshot;
        // The durable steps below run while the current map stays in place.
        // The map and the lifecycle transitions change only after them, so a
        // root that is refused (for example its symlink now points elsewhere)
        // or a failed database step never drops the other roots' entries.
        let mut refused = Vec::new();
        let mut accepted = Vec::new();
        for entry in entries {
            // resolve_entries already captured this live identity. Resolving
            // again would spawn git twice more and could mix two filesystem
            // observations in one durable record.
            let record = StandingRootRecord {
                literal_path: entry.literal_path.clone(),
                resolved_target: entry.resolved_target.to_string_lossy().into_owned(),
                resolved_git_toplevel: entry
                    .resolved_git_toplevel
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                scoped_relative_path: entry
                    .scoped_relative_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            };
            match standing_roots::ensure_standing_root(&mut conn, &record, &entry.indexes) {
                Ok(outcome) => accepted.push((entry, outcome)),
                Err(error) => refused.push(StandingRootRefusal {
                    literal_path: entry.literal_path,
                    error: StandingRootsError::from(error).to_string(),
                }),
            }
        }
        let configured = |literal_path: &String| {
            accepted
                .iter()
                .any(|(entry, _)| &entry.literal_path == literal_path)
                || refused
                    .iter()
                    .any(|refusal| &refusal.literal_path == literal_path)
        };
        let mut removed = Vec::new();
        let mut removal_failed = Vec::new();
        for literal_path in state.entries.keys().filter(|path| !configured(path)) {
            match standing_roots::delete_standing_root(&conn, literal_path) {
                Ok(()) => removed.push(literal_path.clone()),
                Err(error) => removal_failed.push(StandingRootRefusal {
                    literal_path: literal_path.clone(),
                    error: StandingRootsError::from(error).to_string(),
                }),
            }
        }

        // Daemon restart is an observation gap. Startup first reconciles an
        // empty default before subc has received a RouteBind configuration, so
        // the first non-empty observed snapshot marks every retained row strict.
        // A root that was refused went unmaintained too, so its first accepted
        // pass marks its existing row the same way. Only the transactional
        // strict-verification clear below can consume either mark.
        for (entry, outcome) in &accepted {
            let unobserved = match state.entries.get(&entry.literal_path) {
                Some(old) => old.refused,
                None => *outcome == standing_roots::EnsureStandingRoot::Reloaded,
            };
            if !previous_first_snapshot || unobserved {
                mark_kinds_needing_strict_verify(
                    &mut conn,
                    &entry.literal_path,
                    &entry.indexes,
                )?;
            }
        }

        let mut previous = std::mem::take(&mut state.entries);
        let mut next = BTreeMap::new();
        let mut added = Vec::new();
        let mut replaced = Vec::new();
        for (entry, _) in &accepted {
            if let Some(old) = previous.remove(&entry.literal_path) {
                let changed = old.entry.resolved_target != entry.resolved_target
                    || old.entry.resolved_git_toplevel != entry.resolved_git_toplevel
                    || old.entry.scoped_relative_path != entry.scoped_relative_path
                    || old.entry.artifact_key != entry.artifact_key
                    || old.entry.indexes != entry.indexes;
                if changed {
                    old.lifecycle.revoke_and_mint(false);
                    old.lifecycle.resume_unbound();
                    replaced.push(entry.literal_path.clone());
                } else if old.refused {
                    old.lifecycle.recover_from_refusal();
                }
                next.insert(
                    entry.literal_path.clone(),
                    ManagedEntry {
                        entry: entry.clone(),
                        lifecycle: old.lifecycle,
                        refused: false,
                    },
                );
            } else {
                added.push(entry.literal_path.clone());
                next.insert(
                    entry.literal_path.clone(),
                    ManagedEntry {
                        entry: entry.clone(),
                        lifecycle: Arc::new(ArtifactLifecycle::default()),
                        refused: false,
                    },
                );
            }
        }
        // A refused root keeps its last accepted entry, so removing it from
        // configuration later still deletes its row and retires its actor. A
        // removed root whose row could not be deleted stays for a retry.
        for refusal in refused.iter().chain(&removal_failed) {
            if let Some(old) = previous.remove(&refusal.literal_path) {
                if !old.refused {
                    old.lifecycle.refuse();
                }
                next.insert(
                    refusal.literal_path.clone(),
                    ManagedEntry {
                        refused: true,
                        ..old
                    },
                );
            }
        }
        for old in previous.into_values() {
            old.lifecycle.revoke_and_mint(false);
        }
        state.observed_first_snapshot |= !accepted.is_empty();
        state.reconciled_empty = accepted.is_empty() && refused.is_empty();
        state.entries = next;
        refused.extend(removal_failed);

        Ok(StandingReconcileReport {
            active_entries: accepted.into_iter().map(|(entry, _)| entry).collect(),
            added,
            removed,
            replaced,
            refused,
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
            .filter(|entry| !entry.refused)
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
            .unwrap_or_else(|| crate::storage_path::storage_dir(None).join("aft.db"));
        Ok(crate::db::open(&path)?)
    }
}

#[cfg(test)]
thread_local! {
    static ROOT_RESOLUTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RECONCILE_DB_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RECONCILE_IMMEDIATE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RECONCILE_PREOPEN_STATE_LOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // The release fixture compares both paths in one process on the same warmed
    // database. Baseline mode bypasses the whole new pre-open state-lock block.
    static RECONCILE_BENCH_BASELINE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn reconciliation_work_for_test() -> [i64; 6] {
    use crate::db::lifecycle::{thread_counts, SqliteStore};
    let opens = thread_counts::total_opens_on_this_thread(SqliteStore::AftDb) as i64;
    [
        RECONCILE_DB_OPENS.with(|count| count.get() as i64),
        opens,
        opens - thread_counts::open_on_this_thread(SqliteStore::AftDb),
        crate::db::schema_peek_count_for_test() as i64,
        ROOT_RESOLUTIONS.with(|count| count.get() as i64),
        RECONCILE_IMMEDIATE.with(|count| count.get() as i64),
    ]
}

fn counted_resolve_standing_root(
    path: &str,
    storage: &Path,
    force: bool,
) -> Result<ResolvedStandingRoot, crate::scoped_key::ScopedKeyError> {
    #[cfg(test)]
    ROOT_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    crate::scoped_key::resolve_standing_root_with_recheck(path, storage, true, force)
}

fn resolve_entries(
    config: &Config,
    force: bool,
) -> Result<Vec<StandingRootEntry>, StandingRootsError> {
    let storage = crate::storage_path::storage_dir(config.storage_dir.as_deref());
    let resolved = config
        .index
        .roots
        .iter()
        .map(|root| counted_resolve_standing_root(&root.path, &storage, force))
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

    struct ReconcileMeasurement {
        wall_us: u128,
        process_cpu_us: Option<u64>,
        child_cpu_us: Option<u64>,
        work: [i64; 6],
        pre_open_state_locks: usize,
    }

    #[cfg(unix)]
    fn reconciliation_cpu_us() -> (Option<u64>, Option<u64>) {
        let read = |who| {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            // getrusage initializes the caller-owned structure on success.
            if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
                return None;
            }
            let usage = unsafe { usage.assume_init() };
            Some(
                (usage.ru_utime.tv_sec as u64 + usage.ru_stime.tv_sec as u64) * 1_000_000
                    + usage.ru_utime.tv_usec as u64
                    + usage.ru_stime.tv_usec as u64,
            )
        };
        (read(libc::RUSAGE_SELF), read(libc::RUSAGE_CHILDREN))
    }

    #[cfg(not(unix))]
    fn reconciliation_cpu_us() -> (Option<u64>, Option<u64>) {
        (None, None)
    }

    fn measure_reconciliation(
        roots: &StandingRoots,
        snapshot: &Config,
        baseline: bool,
        ticks: i64,
    ) -> ReconcileMeasurement {
        struct RestoreBaseline(bool);
        impl Drop for RestoreBaseline {
            fn drop(&mut self) {
                RECONCILE_BENCH_BASELINE.with(|mode| mode.set(self.0));
            }
        }
        let _restore =
            RestoreBaseline(RECONCILE_BENCH_BASELINE.with(|mode| mode.replace(baseline)));
        let work_before = reconciliation_work_for_test();
        let pre_open_locks_before = RECONCILE_PREOPEN_STATE_LOCKS.with(std::cell::Cell::get);
        let cpu_before = reconciliation_cpu_us();
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            roots.reconcile(std::hint::black_box(snapshot)).unwrap();
        }
        let wall_us = start.elapsed().as_micros();
        let cpu_after = reconciliation_cpu_us();
        let work_after = reconciliation_work_for_test();
        ReconcileMeasurement {
            wall_us,
            process_cpu_us: cpu_after
                .0
                .zip(cpu_before.0)
                .map(|(after, before)| after - before),
            child_cpu_us: cpu_after
                .1
                .zip(cpu_before.1)
                .map(|(after, before)| after - before),
            work: std::array::from_fn(|i| work_after[i] - work_before[i]),
            pre_open_state_locks: RECONCILE_PREOPEN_STATE_LOCKS.with(std::cell::Cell::get)
                - pre_open_locks_before,
        }
    }

    #[test]
    fn nonempty_reconciliation_does_not_take_an_extra_state_lock() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        let snapshot = config(
            storage.path(),
            vec![root(project.path(), vec![IndexKind::Search])],
        );
        let roots = StandingRoots::default();
        roots.reconcile(&snapshot).unwrap();
        let before = measure_reconciliation(&roots, &snapshot, true, 3);
        let after = measure_reconciliation(&roots, &snapshot, false, 3);
        eprintln!(
            "nonempty pre-open state locks: before={} after={}",
            before.pre_open_state_locks, after.pre_open_state_locks
        );
        assert_eq!(before.work, after.work);
        assert_eq!(before.pre_open_state_locks, 0);
        assert_eq!(after.pre_open_state_locks, 0);
    }

    #[test]
    fn failed_storage_change_does_not_reuse_an_empty_snapshot() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        let roots = StandingRoots::default();
        roots.reconcile(&config(storage.path(), vec![])).unwrap();
        let blocked = storage.path().join("blocked-storage");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let nonempty = config(
            &blocked,
            vec![root(project.path(), vec![IndexKind::Search])],
        );
        let before = RECONCILE_DB_OPENS.with(std::cell::Cell::get);
        assert!(roots.reconcile(&nonempty).is_err());
        let retry = roots.reconcile(&config(&blocked, vec![]));
        let opens = RECONCILE_DB_OPENS.with(std::cell::Cell::get) - before;
        eprintln!(
            "failed storage change: logical open attempts={opens}, empty retry refused={}",
            retry.is_err()
        );
        assert_eq!(
            opens, 2,
            "an uninitialized storage directory must still be retried"
        );
        assert!(retry.is_err());
    }

    #[test]
    fn reconciliation_benchmark_baseline_reaches_the_original_open_path() {
        let storage = tempdir().unwrap();
        let roots = StandingRoots::default();
        let empty = config(storage.path(), vec![]);
        roots.reconcile(&empty).unwrap();
        assert_eq!(
            measure_reconciliation(&roots, &empty, true, 3).work,
            [3, 6, 6, 3, 0, 0]
        );
        assert_eq!(
            measure_reconciliation(&roots, &empty, false, 3).work,
            [0; 6]
        );
        let project = tempdir().unwrap();
        let nonempty = config(
            storage.path(),
            vec![root(project.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&nonempty).unwrap();
        for baseline in [true, false] {
            assert_eq!(
                measure_reconciliation(&roots, &nonempty, baseline, 3).work,
                [3, 6, 6, 3, 3, 3]
            );
        }
        assert!(!RECONCILE_BENCH_BASELINE.with(std::cell::Cell::get));
    }

    #[test]
    #[ignore = "release reconciliation measurement"]
    fn bench_daemon_edge_standing_reconciliation() {
        for count in [0, 1, 18] {
            let storage = tempdir().unwrap();
            let projects = (0..count).map(|_| tempdir().unwrap()).collect::<Vec<_>>();
            for project in &projects {
                assert!(std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(project.path())
                    .status()
                    .unwrap()
                    .success());
            }
            let snapshot = config(
                storage.path(),
                projects
                    .iter()
                    .map(|project| root(project.path(), vec![IndexKind::Search]))
                    .collect(),
            );
            for bound in [false, true] {
                let owners = [StandingRoots::default(), StandingRoots::default()];
                for (mode, owner) in owners.iter().enumerate() {
                    measure_reconciliation(owner, &snapshot, mode == 0, 1);
                    if bound {
                        for entry in owner.entries() {
                            owner.begin_case_a_bind(&entry.literal_path).unwrap();
                        }
                    }
                }
                // Both owners use the same database and Git directories. Warm
                // each path before alternating which path runs first in a pair.
                for _ in 0..5 {
                    for (mode, owner) in owners.iter().enumerate() {
                        measure_reconciliation(owner, &snapshot, mode == 0, 1);
                    }
                }
                let mut samples = [Vec::new(), Vec::new()];
                for round in 0..6 {
                    for mode in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                        let sample =
                            measure_reconciliation(&owners[mode], &snapshot, mode == 0, 100);
                        let expected = if count == 0 && mode == 1 {
                            [0; 6]
                        } else {
                            [100, 200, 200, 100, count * 100, count * 100]
                        };
                        assert_eq!(sample.work, expected);
                        eprintln!("standing_pair roots={count} bound={bound} round={} mode={} ticks=100 wall_us={} process_cpu_us={:?} child_cpu_us={:?} pre_open_state_locks={} work={:?}",
                            round + 1, if mode == 0 { "before" } else { "after" },
                            sample.wall_us, sample.process_cpu_us, sample.child_cpu_us, sample.pre_open_state_locks, sample.work);
                        assert_eq!(
                            sample.pre_open_state_locks,
                            if count == 0 && mode == 1 { 100 } else { 0 }
                        );
                        samples[mode].push(sample.wall_us);
                    }
                }
                for (mode, mut walls) in samples.into_iter().enumerate() {
                    eprintln!(
                        "standing_samples roots={count} bound={bound} mode={} wall_us={walls:?}",
                        if mode == 0 { "before" } else { "after" }
                    );
                    walls.sort_unstable();
                    let median = (walls[2] + walls[3]) / 2;
                    eprintln!("standing_summary roots={count} bound={bound} mode={} n=6 median_us={median} min_us={} max_us={}",
                        if mode == 0 { "before" } else { "after" }, walls[0], walls[5]);
                }
            }
        }
    }

    #[test]
    fn reconciliation_resolves_each_live_root_once() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(project.path())
            .status()
            .unwrap()
            .success());
        let roots = StandingRoots::default();
        let snapshot = config(
            storage.path(),
            vec![root(project.path(), vec![IndexKind::Search])],
        );
        ROOT_RESOLUTIONS.with(|count| count.set(0));
        roots.reconcile(&snapshot).unwrap();
        assert_eq!(
            ROOT_RESOLUTIONS.with(std::cell::Cell::get),
            1,
            "git-root resolutions"
        );
    }

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
    fn empty_fast_path_preserves_storage_transitions_and_restart_strict_flags() {
        let first_storage = tempdir().unwrap();
        let second_storage = tempdir().unwrap();
        let third_storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        let roots = StandingRoots::default();
        roots
            .reconcile(&config(first_storage.path(), vec![]))
            .unwrap();
        roots
            .reconcile(&config(second_storage.path(), vec![]))
            .unwrap();
        assert!(!second_storage.path().join("aft.db").exists());
        let populated = config(
            second_storage.path(),
            vec![root(project.path(), vec![IndexKind::Search])],
        );
        roots.reconcile(&populated).unwrap();
        let admission = roots.admit_build(project.path().to_str().unwrap()).unwrap();
        let empty = config(third_storage.path(), vec![]);
        let removed = roots.reconcile(&empty).unwrap();
        assert_eq!(removed.removed, vec![project.path().to_string_lossy()]);
        assert!(admission.checkpoint());
        let conn = crate::db::open(&second_storage.path().join("aft.db")).unwrap();
        assert!(
            standing_roots::get_standing_root(&conn, project.path().to_str().unwrap())
                .unwrap()
                .is_none()
        );
        assert!(!third_storage.path().join("aft.db").exists());
        let before = reconciliation_work_for_test();
        for _ in 0..100 {
            roots.reconcile(&empty).unwrap();
        }
        assert_eq!(reconciliation_work_for_test(), before);
        let third = config(third_storage.path(), populated.index.roots);
        roots.reconcile(&third).unwrap();
        roots
            .record_strict_verification(project.path().to_str().unwrap(), IndexKind::Search)
            .unwrap();
        let restarted = StandingRoots::default();
        restarted
            .reconcile(&config(third_storage.path(), vec![]))
            .unwrap();
        restarted.reconcile(&third).unwrap();
        let conn = crate::db::open(&third_storage.path().join("aft.db")).unwrap();
        assert_eq!(
            standing_roots::needs_strict_verify(
                &conn,
                project.path().to_str().unwrap(),
                IndexKind::Search
            )
            .unwrap(),
            Some(true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_fast_path_does_not_cache_nonempty_symlink_identity() {
        let storage = tempdir().unwrap();
        let targets = tempdir().unwrap();
        std::fs::create_dir(targets.path().join("first")).unwrap();
        std::fs::create_dir(targets.path().join("second")).unwrap();
        let link = targets.path().join("link");
        std::os::unix::fs::symlink(targets.path().join("first"), &link).unwrap();
        let roots = StandingRoots::default();
        roots.reconcile(&config(storage.path(), vec![])).unwrap();
        let snapshot = config(storage.path(), vec![root(&link, vec![IndexKind::Search])]);
        roots.reconcile(&snapshot).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(targets.path().join("second"), &link).unwrap();
        let report = roots.reconcile(&snapshot).unwrap();
        assert_eq!(report.refused.len(), 1, "{report:?}");
        let error = &report.refused[0].error;
        assert!(error.contains("resolved-path-drift"), "{error}");
        assert!(report.active_entries.is_empty());
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        let recorded = standing_roots::get_standing_root(&conn, link.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            recorded.resolved_target,
            targets
                .path()
                .join("first")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
    }

    /// One root whose symlink now resolves elsewhere is refused on its own: the
    /// other root stays managed and admissible, the refused root's running
    /// build is cancelled, and removing both later deletes both rows.
    #[cfg(unix)]
    #[test]
    fn a_refused_root_leaves_the_other_entries_in_place() {
        let storage = tempdir().unwrap();
        let healthy = tempdir().unwrap();
        let targets = tempdir().unwrap();
        std::fs::create_dir(targets.path().join("first")).unwrap();
        std::fs::create_dir(targets.path().join("second")).unwrap();
        let link = targets.path().join("link");
        std::os::unix::fs::symlink(targets.path().join("first"), &link).unwrap();
        let healthy_path = healthy.path().to_string_lossy().into_owned();
        let link_path = link.to_string_lossy().into_owned();
        let snapshot = config(
            storage.path(),
            vec![
                root(healthy.path(), vec![IndexKind::Search]),
                root(&link, vec![IndexKind::Search]),
            ],
        );
        let roots = StandingRoots::default();
        roots.reconcile(&snapshot).unwrap();
        roots
            .record_strict_verification(&link_path, IndexKind::Search)
            .unwrap();
        let healthy_build = roots.admit_build(&healthy_path).unwrap();
        let link_build = roots.admit_build(&link_path).unwrap();

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(targets.path().join("second"), &link).unwrap();
        let report = roots.reconcile_background(&snapshot).unwrap();
        assert_eq!(
            report
                .refused
                .iter()
                .map(|refusal| refusal.literal_path.as_str())
                .collect::<Vec<_>>(),
            vec![link_path.as_str()]
        );
        assert_eq!(
            report
                .active_entries
                .iter()
                .map(|entry| entry.literal_path.as_str())
                .collect::<Vec<_>>(),
            vec![healthy_path.as_str()]
        );
        assert_eq!(roots.entries().len(), 2, "no entry may be dropped");
        assert!(!healthy_build.cancellation_requested());
        assert!(link_build.cancellation_requested());
        drop((healthy_build, link_build));
        assert!(roots.admit_build(&healthy_path).is_some());
        assert!(roots.admit_build(&link_path).is_none());

        // Pointing the link back recovers the refused root.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(targets.path().join("first"), &link).unwrap();
        let report = roots.reconcile_background(&snapshot).unwrap();
        assert!(report.refused.is_empty(), "{report:?}");
        let recovered = roots.admit_build(&link_path).unwrap();
        assert!(!recovered.cancellation_requested());
        drop(recovered);
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        assert_eq!(
            standing_roots::needs_strict_verify(&conn, &link_path, IndexKind::Search).unwrap(),
            Some(true),
            "a root that went unmaintained must verify strictly again"
        );

        // Removing both while the link is refused still removes both rows.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(targets.path().join("second"), &link).unwrap();
        assert_eq!(roots.reconcile(&snapshot).unwrap().refused.len(), 1);
        let mut removed = roots
            .reconcile(&config(storage.path(), vec![]))
            .unwrap()
            .removed;
        removed.sort();
        let mut expected = vec![healthy_path.clone(), link_path.clone()];
        expected.sort();
        assert_eq!(removed, expected);
        assert!(roots.entries().is_empty());
        for literal_path in [&healthy_path, &link_path] {
            assert!(standing_roots::get_standing_root(&conn, literal_path)
                .unwrap()
                .is_none());
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
        restarted
            .reconcile(&config(storage.path(), Vec::new()))
            .unwrap();
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
    fn crash_before_freshness_clear_blocks_verify_on_query_until_transactional_clear() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let literal = root_dir.path().to_str().unwrap();
        let first = StandingRoots::default();
        first.reconcile(&cfg).unwrap();
        first
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        first
            .mark_observation_gap(literal, &[IndexKind::Search])
            .unwrap();
        drop(first);

        // Restarting between verification and its transaction commit must retain
        // the durable gap, so a query reading the flag rejects the baseline.
        let restarted = StandingRoots::default();
        restarted.reconcile(&cfg).unwrap();
        assert!(strict(storage.path(), literal, IndexKind::Search));
        restarted
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        assert!(!strict(storage.path(), literal, IndexKind::Search));
    }

    /// The durable strict-verification flag, as queries read it.
    fn strict(storage: &Path, literal_path: &str, kind: IndexKind) -> bool {
        let conn = crate::db::open(&storage.join("aft.db")).unwrap();
        standing_roots::needs_strict_verify(&conn, literal_path, kind)
            .unwrap()
            .expect("a freshness row")
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
        assert!(strict(storage.path(), literal, IndexKind::Search));
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
        let literal = root_dir.path().to_str().unwrap();
        roots.reconcile(&cfg).unwrap();
        roots
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        let build = roots.admit_build(literal).unwrap();
        roots.begin_case_a_bind(literal).unwrap();
        assert!(build.checkpoint());
        std::fs::write(root_dir.path().join("edited.rs"), "fn changed() {}\n").unwrap();
        roots.resume_after_session(literal, &[]).unwrap();
        assert!(strict(storage.path(), literal, IndexKind::Search));
        roots
            .record_strict_verification(literal, IndexKind::Search)
            .unwrap();
        assert!(!strict(storage.path(), literal, IndexKind::Search));
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
        // The build is still held, so returning without an acknowledgement
        // proves the checkpoint wait expired. A delayed wakeup is not a longer budget.
        assert!(before.elapsed() >= Duration::from_secs(2));
        let lifecycle = roots.lifecycle(root_dir.path().to_str().unwrap()).unwrap();
        let state = lifecycle.state.lock();
        assert_eq!(state.active_builds, 1);
        assert!(!state.checkpoint_acknowledged);
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
