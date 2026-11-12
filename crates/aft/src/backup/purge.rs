//! Early removal of undo backups for the `aft backups purge` management command.
//!
//! Backups normally leave only through the per-file depth cap or the stale
//! session sweep, which never fires for a session that stays active. This module
//! removes chosen stacks early, by recorded path, by session, or both.
//!
//! A stack lives in three places: its directory under
//! `<storage>/<harness>/backups/<session_hash>/<path_hash>/`, its mirror rows in
//! `aft.db`, and the in-memory caches of every [`BackupStore`] that has touched
//! it. Removing only some of them breaks undo: rows without content files are
//! history that previews still list but undo cannot restore, and a cache that
//! still holds the stack keeps showing, or restoring, a stack that is gone. So
//! each stack is removed as one unit:
//!
//! 1. Every live store in the same storage namespace is locked, in address
//!    order, before the stack's cross-process disk lock. Stores take their own
//!    mutex before that disk lock too, so the order matches theirs and no store
//!    can be waiting on the disk lock while holding a mutex this code needs.
//! 2. Under the disk lock the database rows go first. If that fails nothing else
//!    is touched, and disk plus rows still agree.
//! 3. `meta.json` goes next. Readers find a stack only through that file, so
//!    once it is gone the stack is invisible even if content files remain.
//! 4. The directory is removed and every locked store forgets the stack.
//!
//! When no process owns the storage, the same code runs with no live stores.
//! Other processes that still cache a purged stack re-read disk under the same
//! lock before every undo or append, find nothing, and drop their copy; with the
//! rows gone too, their undo reports `no_undo_history`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::{canonicalize_key, hash_session, is_loadable_backup_path, BackupStore};
use crate::db::TrackedConnection;

/// Management operation name for the purge request.
pub const BACKUPS_PURGE_OPERATION: &str = "backups.purge";

/// How many matched paths and skipped directories a report lists by name.
const SAMPLE_LIMIT: usize = 5;

/// Label for the pre-harness layout that keeps backups directly in
/// `<storage>/backups`. That namespace has no database rows.
const UNSCOPED_NAMESPACE: &str = "(unscoped)";

/// One purge request, as the CLI sends it to an owning process or runs it
/// itself. `dry_run` defaults to true so a request missing the field only
/// reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeRequest {
    pub storage_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

impl PurgeRequest {
    /// Parse and validate the params object of a management request.
    pub fn from_params(params: &serde_json::Value) -> Result<Self, String> {
        let request: PurgeRequest = serde_json::from_value(params.clone())
            .map_err(|error| format!("invalid backups.purge params: {error}"))?;
        request.validate()?;
        Ok(request)
    }

    /// Reject requests that could match more than the caller meant.
    pub fn validate(&self) -> Result<(), String> {
        if !self.storage_dir.is_absolute() {
            return Err(format!(
                "storage_dir must be absolute: {}",
                self.storage_dir.display()
            ));
        }
        if self.path.is_none() && self.session.is_none() {
            return Err("backups.purge needs a path, a session, or both".to_string());
        }
        if let Some(path) = &self.path {
            // A relative path would resolve against the owner's working
            // directory, which is not the caller's.
            if !path.is_absolute() {
                return Err(format!("path must be absolute: {}", path.display()));
            }
        }
        if self.session.as_deref().is_some_and(str::is_empty) {
            return Err("session must not be empty".to_string());
        }
        if let Some(harness) = &self.harness {
            if harness.is_empty()
                || harness == "."
                || harness == ".."
                || harness.contains(['/', '\\'])
            {
                return Err(format!("invalid harness name: {harness:?}"));
            }
        }
        Ok(())
    }
}

/// Resolve a user-supplied path to the key backups are recorded under, so a
/// comparison is made in the same key space the stores write.
pub fn backup_key_for_path(path: &Path) -> PathBuf {
    canonicalize_key(path)
}

/// How a saved path relates to a [`PurgeTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedPathMatch {
    /// The saved path is the purged path or lies under it.
    Selected,
    /// The saved path is somewhere else.
    Unrelated,
    /// The saved path's parent directory no longer exists, so whether it was
    /// another spelling of the purged path cannot be told. It is kept, and
    /// the purge must report it rather than claim it removed everything.
    Unknown,
}

/// The path a purge removes saved copies of, resolved once so every saved
/// path is compared by the location it names rather than by its spelling.
///
/// A location resolves every parent directory through symlinks but keeps the
/// final component as written: a symlink is its own entry, distinct from the
/// file it points at, so purging the link never purges its target's copies.
#[derive(Debug, Clone)]
pub struct PurgeTarget {
    /// The path as the caller gave it.
    given: PathBuf,
    /// The path's location: parents resolved, final component kept.
    location: PathBuf,
    /// A final symlink is only the link entry, just as it is for undo and
    /// checkpoint restore. Saved copies of its target or of paths beneath
    /// the link require a separate purge naming their own location.
    entry_only: bool,
    /// For an existing non-directory, its file name. A saved path with any
    /// other final name can be neither the purged file nor under it.
    existing_file_name: Option<std::ffi::OsString>,
}

impl PurgeTarget {
    pub fn new(path: &Path) -> Self {
        let location = canonicalize_key(path);
        let metadata = std::fs::symlink_metadata(path).ok();
        let entry_only = metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink());
        let existing_file_name = metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_dir())
            .then(|| path.file_name().map(std::ffi::OsStr::to_os_string))
            .flatten();
        Self {
            given: path.to_path_buf(),
            location,
            entry_only,
            existing_file_name,
        }
    }

    /// How `saved` relates to this target. `recorded_resolved` is true for
    /// undo-backup keys: stores record them with every parent already
    /// resolved, so the key already is the location it was saved at and is
    /// compared as stored. Resolving it again would follow symlinks created
    /// since: a backup of `/repo/old/file.txt`, where `old` was later
    /// replaced by a link to `/repo/new`, would otherwise be taken for
    /// `/repo/new/file.txt` and purged with it. A parent that is gone now was
    /// a real directory then, so such a key still names its location.
    /// Checkpoint keys keep the caller's spelling. When their parent exists,
    /// they are resolved here before comparing; when it is missing, the
    /// result is `Unknown` (or `Unrelated` for a different file name),
    /// because that parent may have been a symlink to anywhere.
    pub fn classify(&self, saved: &Path, recorded_resolved: bool) -> SavedPathMatch {
        if self.entry_only {
            return if saved == self.given
                || saved == self.location
                || (!recorded_resolved && canonicalize_key(saved) == self.location)
            {
                SavedPathMatch::Selected
            } else {
                SavedPathMatch::Unrelated
            };
        }
        if saved.starts_with(&self.given) || saved.starts_with(&self.location) {
            return SavedPathMatch::Selected;
        }
        let parent_exists = saved
            .parent()
            .is_some_and(|parent| std::fs::canonicalize(parent).is_ok());
        if !parent_exists && !recorded_resolved {
            let other_name = self
                .existing_file_name
                .as_deref()
                .is_some_and(|name| saved.file_name() != Some(name));
            return if other_name {
                SavedPathMatch::Unrelated
            } else {
                SavedPathMatch::Unknown
            };
        }
        let saved_location = if recorded_resolved {
            saved.to_path_buf()
        } else {
            canonicalize_key(saved)
        };
        if saved_location.starts_with(&self.location) {
            SavedPathMatch::Selected
        } else {
            SavedPathMatch::Unrelated
        }
    }

    fn selects(&self, saved: &Path, recorded_resolved: bool) -> bool {
        self.classify(saved, recorded_resolved) == SavedPathMatch::Selected
    }
}

/// Totals for one side of a report: what matched, or what was removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeTotals {
    /// `(harness, session, path)` stacks.
    pub stacks: usize,
    /// Backup entries in those stacks (undo steps).
    pub entries: usize,
    /// Files on disk, content files plus each stack's `meta.json`.
    pub files: usize,
    pub bytes: u64,
    pub db_rows: usize,
    /// Distinct session ids.
    pub sessions: Vec<String>,
}

/// What the purge looked at, whether or not it matched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeExamined {
    /// Harness namespaces scanned (storage segment names).
    pub namespaces: Vec<String>,
    pub session_dirs: usize,
    pub disk_stacks: usize,
    pub db_stacks: usize,
    pub memory_stacks: usize,
    /// Directories or rows that could not be attributed to a session and path,
    /// so no filter could match them.
    pub skipped: usize,
    pub skipped_samples: Vec<String>,
}

/// One stack the purge could not fully remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeFailure {
    pub harness: String,
    pub session: String,
    pub path: String,
    /// True when undo can no longer see the stack but leftover files remain.
    pub hidden: bool,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    pub dry_run: bool,
    pub storage_dir: String,
    pub examined: PurgeExamined,
    pub matched: PurgeTotals,
    pub removed: PurgeTotals,
    /// Paths of fully removed stacks; unlike samples this is not capped.
    #[serde(default)]
    pub removed_paths: Vec<String>,
    /// A few matched paths, for the operator to sanity-check the filter.
    pub samples: Vec<String>,
    pub failures: Vec<PurgeFailure>,
    /// In-process stores whose caches were updated together with disk.
    pub live_stores: usize,
}

impl PurgeReport {
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Default)]
struct TotalsBuilder {
    totals: PurgeTotals,
    sessions: BTreeSet<String>,
}

impl TotalsBuilder {
    fn add(&mut self, session: &str, measure: &StackMeasure) {
        self.totals.stacks += 1;
        self.totals.entries += measure.entries;
        self.totals.files += measure.files;
        self.totals.bytes += measure.bytes;
        self.totals.db_rows += measure.db_rows;
        self.sessions.insert(session.to_string());
    }

    fn finish(mut self) -> PurgeTotals {
        self.totals.sessions = self.sessions.into_iter().collect();
        self.totals
    }
}

#[derive(Debug, Default)]
struct StackMeasure {
    entries: usize,
    files: usize,
    bytes: u64,
    db_rows: usize,
}

impl StackMeasure {
    fn is_empty(&self) -> bool {
        self.entries == 0 && self.files == 0 && self.db_rows == 0
    }
}

struct StackPurgeFailure {
    hidden: bool,
    message: String,
}

type LiveStore<'a> = &'a parking_lot::Mutex<BackupStore>;

/// Remove, or with `dry_run` only count, every stack matching `request`.
///
/// `live_stores` are the stores this process keeps in memory; their caches are
/// updated under the same locks as disk and database. `db` must be this
/// process's existing handle for `<storage>/aft.db` when it has one, never a
/// second connection to the same file.
pub fn purge_backups(
    request: &PurgeRequest,
    db: Option<Arc<Mutex<TrackedConnection>>>,
    live_stores: &[LiveStore<'_>],
) -> PurgeReport {
    let storage_dir = request.storage_dir.as_path();
    let path_filter = request.path.as_deref().map(PurgeTarget::new);
    let mut report = PurgeReport {
        dry_run: request.dry_run,
        storage_dir: storage_dir.display().to_string(),
        ..PurgeReport::default()
    };

    // A fixed lock order across callers: two purges never wait on each other
    // while each holds a store the other needs.
    let mut stores = live_stores.to_vec();
    stores.sort_by_key(|store| *store as *const parking_lot::Mutex<BackupStore> as usize);
    stores.dedup_by_key(|store| *store as *const parking_lot::Mutex<BackupStore> as usize);

    let mut matched = TotalsBuilder::default();
    let mut removed = TotalsBuilder::default();
    let mut live_store_count = BTreeSet::new();

    for namespace in namespaces(storage_dir, db.as_ref(), request.harness.as_deref()) {
        let label = namespace
            .clone()
            .unwrap_or_else(|| UNSCOPED_NAMESPACE.to_string());
        report.examined.namespaces.push(label.clone());

        let mut working = namespace_working_store(storage_dir, namespace.as_ref(), db.as_ref());

        let namespace_stores = stores
            .iter()
            .copied()
            .filter(|store| {
                store
                    .lock()
                    .uses_namespace(storage_dir, namespace.as_deref())
            })
            .collect::<Vec<_>>();
        for store in &namespace_stores {
            live_store_count.insert(*store as *const parking_lot::Mutex<BackupStore> as usize);
        }

        let candidates = collect_candidates(
            &working,
            &namespace_stores,
            request.session.as_deref(),
            path_filter.as_ref(),
            &mut report.examined,
        );

        for (session, key) in candidates {
            // Store mutexes first, then the disk lock: the order every store
            // uses for its own reads and writes.
            let mut guards = Vec::new();
            if !request.dry_run {
                for store in &namespace_stores {
                    let guard = store.lock();
                    if guard.uses_namespace(storage_dir, namespace.as_deref()) {
                        guards.push(guard);
                    }
                }
            }
            let _disk_lock = if request.dry_run {
                None
            } else {
                match working.acquire_stack_disk_lock(&session, &key) {
                    Ok(lock) => lock,
                    Err(error) => {
                        report.failures.push(PurgeFailure {
                            harness: label.clone(),
                            session: session.clone(),
                            path: key.display().to_string(),
                            hidden: false,
                            error: format!("could not lock the stack: {error}"),
                        });
                        continue;
                    }
                }
            };

            let mut measure = working.measure_stack(&session, &key);
            let memory_entries = if request.dry_run {
                namespace_stores
                    .iter()
                    .map(|store| store.lock().in_memory_entry_count(&session, &key))
                    .max()
                    .unwrap_or(0)
            } else {
                guards
                    .iter()
                    .map(|guard| guard.in_memory_entry_count(&session, &key))
                    .max()
                    .unwrap_or(0)
            };
            measure.entries = measure.entries.max(memory_entries);
            if measure.is_empty() && memory_entries == 0 && request.dry_run {
                // Gone since the scan; nothing to report or remove.
                continue;
            }
            if !measure.is_empty() || memory_entries > 0 {
                matched.add(&session, &measure);
                if report.samples.len() < SAMPLE_LIMIT {
                    report.samples.push(key.display().to_string());
                }
            }
            if request.dry_run {
                continue;
            }

            match working.purge_stack_locked(&session, &key) {
                Ok(()) => {
                    for guard in guards.iter_mut() {
                        guard.evict_purged_stack(&session, &key);
                    }
                    if !measure.is_empty() || memory_entries > 0 {
                        removed.add(&session, &measure);
                        report.removed_paths.push(key.display().to_string());
                    }
                }
                Err(failure) => {
                    if failure.hidden {
                        // Undo can no longer see the stack, so cached copies
                        // must go too or they would outlive it.
                        for guard in guards.iter_mut() {
                            guard.evict_purged_stack(&session, &key);
                        }
                    }
                    report.failures.push(PurgeFailure {
                        harness: label.clone(),
                        session: session.clone(),
                        path: key.display().to_string(),
                        hidden: failure.hidden,
                        error: failure.message,
                    });
                }
            }
        }
    }

    report.matched = matched.finish();
    report.removed = removed.finish();
    report.live_stores = live_store_count.len();
    report
}

/// Run a purge in a process that owns no store for this storage. Opens the
/// database itself, which is safe only because this process holds no other
/// handle to it.
pub fn purge_offline(request: &PurgeRequest) -> Result<PurgeReport, String> {
    request.validate()?;
    let db = open_purge_db(&request.storage_dir, request.dry_run)?;
    Ok(purge_backups(request, db, &[]))
}

/// Why an owning process declined a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRefusal {
    pub code: &'static str,
    pub message: String,
}

/// Run a purge in a process that may own stores for the requested storage.
///
/// `resident_db` returns this process's open handle for a database path, if
/// any. The request is refused with `storage_not_owned` when neither a live
/// store nor the resident database uses the requested storage, so the caller
/// knows to run offline instead.
pub fn purge_as_owner(
    params: &serde_json::Value,
    live_stores: &[LiveStore<'_>],
    resident_db: impl FnOnce(&Path) -> Option<Arc<Mutex<TrackedConnection>>>,
) -> Result<PurgeReport, OwnerRefusal> {
    let request = PurgeRequest::from_params(params).map_err(|message| OwnerRefusal {
        code: "invalid_request",
        message,
    })?;
    let db_path = request.storage_dir.join("aft.db");
    let resident = resident_db(&db_path);
    let owns_store = live_stores.iter().any(|store| {
        let store = store.lock();
        store
            .storage_dir
            .as_deref()
            .is_some_and(|dir| same_dir(dir, &request.storage_dir))
    });
    if !owns_store && resident.is_none() {
        return Err(OwnerRefusal {
            code: "storage_not_owned",
            message: format!(
                "this process has no backup store for {}",
                request.storage_dir.display()
            ),
        });
    }
    let db = match resident {
        Some(db) => Some(db),
        // No handle is resident here, so this connection is the process's only one.
        None => open_purge_db(&request.storage_dir, request.dry_run).map_err(|message| {
            OwnerRefusal {
                code: "database_unavailable",
                message,
            }
        })?,
    };
    Ok(purge_backups(&request, db, live_stores))
}

/// Remove one session's undo stacks at or under `path`, for the agent-facing
/// `aft_safety` purge op.
///
/// Only `session` is matched, in every harness namespace of the store's
/// storage, so another session's stacks are never touched. The database
/// handle is the store's own (the connection that owns `aft.db` in this
/// process); `resident_db` is consulted only when the store has none attached
/// and must likewise return an already-open handle, never open a new one. The
/// caller must not hold `store`'s mutex: the purge takes it itself.
pub fn purge_session_path(
    store: &parking_lot::Mutex<BackupStore>,
    session: &str,
    path: &Path,
    resident_db: impl FnOnce(&Path) -> Option<Arc<Mutex<TrackedConnection>>>,
) -> Result<PurgeReport, String> {
    let (storage_dir, own_db) = {
        let guard = store.lock();
        (
            guard.storage_dir.clone(),
            guard.db_pool.read().ok().and_then(|slot| slot.clone()),
        )
    };
    let Some(storage_dir) = storage_dir else {
        return Ok(purge_session_path_in_memory(store, session, path));
    };
    let request = PurgeRequest {
        path: Some(path.to_path_buf()),
        session: Some(session.to_string()),
        harness: None,
        dry_run: false,
        storage_dir,
    };
    request.validate()?;
    let db = own_db.or_else(|| resident_db(&request.storage_dir.join("aft.db")));
    Ok(purge_backups(&request, db, &[store]))
}

/// A store with no storage directory keeps its stacks only in memory, so
/// forgetting them there removes them completely.
fn purge_session_path_in_memory(
    store: &parking_lot::Mutex<BackupStore>,
    session: &str,
    path: &Path,
) -> PurgeReport {
    let target = PurgeTarget::new(path);
    let mut guard = store.lock();
    let keys = guard
        .in_memory_stack_keys()
        .into_iter()
        .filter(|(owner, key)| owner == session && key_matches(key, Some(&target)))
        .collect::<Vec<_>>();
    let mut removed = TotalsBuilder::default();
    let mut removed_paths = Vec::new();
    for (owner, key) in keys {
        let measure = StackMeasure {
            entries: guard.in_memory_entry_count(&owner, &key),
            ..StackMeasure::default()
        };
        guard.evict_purged_stack(&owner, &key);
        removed.add(&owner, &measure);
        removed_paths.push(key.display().to_string());
    }
    let removed = removed.finish();
    PurgeReport {
        dry_run: false,
        matched: removed.clone(),
        removed,
        removed_paths,
        live_stores: 1,
        ..PurgeReport::default()
    }
}

/// The keys of one session's undo stacks at or under `path`: what
/// [`purge_session_path`] would remove, found the same way and with the same
/// database rules, without changing anything. Plugins ask permission for
/// each of these before they run the purge.
pub fn session_path_keys(
    store: &parking_lot::Mutex<BackupStore>,
    session: &str,
    path: &Path,
    resident_db: impl FnOnce(&Path) -> Option<Arc<Mutex<TrackedConnection>>>,
) -> Vec<PathBuf> {
    let (storage_dir, own_db) = {
        let guard = store.lock();
        (
            guard.storage_dir.clone(),
            guard.db_pool.read().ok().and_then(|slot| slot.clone()),
        )
    };
    let target = PurgeTarget::new(path);
    let Some(storage_dir) = storage_dir else {
        return store
            .lock()
            .in_memory_stack_keys()
            .into_iter()
            .filter(|(owner, key)| owner == session && key_matches(key, Some(&target)))
            .map(|(_, key)| key)
            .collect();
    };
    let db = own_db.or_else(|| resident_db(&storage_dir.join("aft.db")));
    let mut keys = BTreeSet::new();
    let mut examined = PurgeExamined::default();
    for namespace in namespaces(&storage_dir, db.as_ref(), None) {
        let working = namespace_working_store(&storage_dir, namespace.as_ref(), db.as_ref());
        let live_stores = [store]
            .into_iter()
            .filter(|store| {
                store
                    .lock()
                    .uses_namespace(&storage_dir, namespace.as_deref())
            })
            .collect::<Vec<_>>();
        let candidates = collect_candidates(
            &working,
            &live_stores,
            Some(session),
            Some(&target),
            &mut examined,
        );
        keys.extend(candidates.into_iter().map(|(_, key)| key));
    }
    keys.into_iter().collect()
}

/// A store that reads and writes exactly one namespace of `storage_dir`.
fn namespace_working_store(
    storage_dir: &Path,
    namespace: Option<&String>,
    db: Option<&Arc<Mutex<TrackedConnection>>>,
) -> BackupStore {
    let mut working = BackupStore::new();
    // The working store addresses this namespace explicitly; a route
    // harness installed on the calling thread must not redirect it.
    working.follow_request_harness = false;
    working.set_storage_dir_inner(storage_dir.to_path_buf(), namespace.cloned(), 0);
    if let (Some(db), Some(segment)) = (db, namespace) {
        working.set_db_pool(Arc::clone(db));
        working.set_db_harness_segment(segment.clone());
    }
    working
}

fn open_purge_db(
    storage_dir: &Path,
    dry_run: bool,
) -> Result<Option<Arc<Mutex<TrackedConnection>>>, String> {
    let db_path = storage_dir.join("aft.db");
    if !db_path.is_file() {
        return Ok(None);
    }
    // A dry run must not migrate or otherwise write the database.
    let conn = if dry_run {
        crate::db::open_readonly(&db_path)
    } else {
        crate::db::open(&db_path)
    }
    .map_err(|error| format!("could not open {}: {error}", db_path.display()))?;
    let conn = Arc::new(Mutex::new(conn));
    crate::db::deferred_checkpoint::register(&conn);
    Ok(Some(conn))
}

fn same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn key_matches(key: &Path, path_filter: Option<&PurgeTarget>) -> bool {
    // Undo-backup keys are recorded with their parents resolved; see
    // `PurgeTarget::classify`. `Path::starts_with` compares whole components,
    // so `/a/b` covers `/a/b` and `/a/b/c` but not `/a/bc`.
    path_filter.is_none_or(|target| target.selects(key, true))
}

fn note_skipped(examined: &mut PurgeExamined, what: String) {
    examined.skipped += 1;
    if examined.skipped_samples.len() < SAMPLE_LIMIT {
        examined.skipped_samples.push(what);
    }
}

/// Harness namespaces present on disk or in the database, narrowed to
/// `harness` when given.
fn namespaces(
    storage_dir: &Path,
    db: Option<&Arc<Mutex<TrackedConnection>>>,
    harness: Option<&str>,
) -> Vec<Option<String>> {
    let mut found = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(storage_dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name != "backups" && entry.path().join("backups").is_dir() {
                found.insert(Some(name));
            }
        }
    }
    if storage_dir.join("backups").is_dir() {
        found.insert(None);
    }
    if let Some(db) = db {
        if let Ok(conn) = db.lock() {
            if let Ok(harnesses) = crate::db::backups::list_backup_harnesses(&conn) {
                found.extend(harnesses.into_iter().map(Some));
            }
        }
    }
    found
        .into_iter()
        .filter(|namespace| match harness {
            Some(harness) => namespace.as_deref() == Some(harness),
            None => true,
        })
        .collect()
}

/// Every `(session, key)` in one namespace that matches the filters, from disk,
/// database rows, and live caches.
fn collect_candidates(
    working: &BackupStore,
    live_stores: &[LiveStore<'_>],
    session_filter: Option<&str>,
    path_filter: Option<&PurgeTarget>,
    examined: &mut PurgeExamined,
) -> BTreeSet<(String, PathBuf)> {
    let mut candidates = BTreeSet::new();

    if let Some(backups_dir) = working.backups_dir().filter(|dir| dir.is_dir()) {
        let session_dirs: Vec<(PathBuf, Option<String>)> = match session_filter {
            Some(session) => {
                let dir = backups_dir.join(hash_session(session));
                if dir.is_dir() {
                    vec![(dir, Some(session.to_string()))]
                } else {
                    Vec::new()
                }
            }
            None => std::fs::read_dir(&backups_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.is_dir())
                        .map(|path| (path, None))
                        .collect()
                })
                .unwrap_or_default(),
        };
        for (session_dir, known_session) in session_dirs {
            examined.session_dirs += 1;
            let session = match known_session.or_else(|| session_id_from_marker(&session_dir)) {
                Some(session) => session,
                None => {
                    note_skipped(
                        examined,
                        format!("{}: no session marker", session_dir.display()),
                    );
                    continue;
                }
            };
            let Ok(path_dirs) = std::fs::read_dir(&session_dir) else {
                note_skipped(examined, format!("{}: unreadable", session_dir.display()));
                continue;
            };
            for path_dir in path_dirs.flatten().map(|entry| entry.path()) {
                if !path_dir.is_dir() || path_dir.file_name().is_some_and(|name| name == ".locks") {
                    continue;
                }
                let Some(key) = recorded_key(&path_dir) else {
                    note_skipped(
                        examined,
                        format!("{}: no readable meta.json", path_dir.display()),
                    );
                    continue;
                };
                examined.disk_stacks += 1;
                crate::run_tool_call::progress::report();
                if key_matches(&key, path_filter) {
                    candidates.insert((session.clone(), key));
                }
            }
        }
    }

    if let Some((pool, harness)) = working.db_pool_and_harness() {
        let stacks = pool.lock().ok().and_then(|conn| {
            crate::db::backups::list_backup_stacks(&conn, Some(&harness), session_filter).ok()
        });
        for stack in stacks.unwrap_or_default() {
            let key = PathBuf::from(&stack.file_path);
            if BackupStore::path_hash(&key) != stack.path_hash {
                note_skipped(
                    examined,
                    format!(
                        "database rows for {} in session {}: path hash mismatch",
                        stack.file_path, stack.session_id
                    ),
                );
                continue;
            }
            examined.db_stacks += 1;
            crate::run_tool_call::progress::report();
            if key_matches(&key, path_filter) {
                candidates.insert((stack.session_id, key));
            }
        }
    }

    for store in live_stores {
        for (session, key) in store.lock().in_memory_stack_keys() {
            if session_filter.is_some_and(|filter| filter != session) {
                continue;
            }
            examined.memory_stacks += 1;
            crate::run_tool_call::progress::report();
            if key_matches(&key, path_filter) {
                candidates.insert((session, key));
            }
        }
    }

    candidates
}

fn session_id_from_marker(session_dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(session_dir.join("session.json")).ok()?;
    let marker: serde_json::Value = serde_json::from_str(&content).ok()?;
    let session = marker.get("session_id")?.as_str()?.to_string();
    // A marker naming a different session than the directory hash is not
    // trusted: purging by its id would compute a different directory.
    let dir_name = session_dir.file_name()?.to_str()?;
    (hash_session(&session) == dir_name).then_some(session)
}

fn recorded_key(path_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(path_dir.join("meta.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
    let key = PathBuf::from(meta.get("path")?.as_str()?);
    is_loadable_backup_path(&key, path_dir).then_some(key)
}

fn dir_file_totals(dir: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut files = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.path().symlink_metadata() else {
            continue;
        };
        if metadata.is_dir() {
            let (nested_files, nested_bytes) = dir_file_totals(&entry.path());
            files += nested_files;
            bytes += nested_bytes;
        } else {
            files += 1;
            bytes += metadata.len();
        }
    }
    (files, bytes)
}

impl BackupStore {
    fn set_db_harness_segment(&self, segment: String) {
        if let Ok(mut slot) = self.db_harness.write() {
            *slot = Some(segment);
        }
        self.clear_db_mirror_sync();
    }

    /// True when this store persists into `storage_dir` under `namespace`,
    /// either as its configured namespace or as one a request's route used.
    fn uses_namespace(&self, storage_dir: &Path, namespace: Option<&str>) -> bool {
        let serves_namespace = self.storage_harness.as_deref() == namespace
            || namespace.is_some_and(|namespace| {
                self.storage_harness.is_some()
                    && self
                        .request_namespaces
                        .read()
                        .is_ok_and(|namespaces| namespaces.contains(namespace))
            });
        serves_namespace
            && self
                .storage_dir
                .as_deref()
                .is_some_and(|dir| same_dir(dir, storage_dir))
    }

    fn in_memory_stack_keys(&self) -> Vec<(String, PathBuf)> {
        let mut keys = BTreeSet::new();
        for (session, files) in &self.entries {
            for (key, stack) in files {
                if !stack.is_empty() {
                    keys.insert((session.clone(), key.clone()));
                }
            }
        }
        for (session, files) in &self.disk_index {
            for key in files.keys() {
                keys.insert((session.clone(), key.clone()));
            }
        }
        keys.into_iter().collect()
    }

    fn in_memory_entry_count(&self, session: &str, key: &Path) -> usize {
        self.entries
            .get(session)
            .and_then(|files| files.get(key))
            .map_or(0, Vec::len)
    }

    /// Count a stack's entries, files, bytes, and rows without changing it.
    fn measure_stack(&self, session: &str, key: &Path) -> StackMeasure {
        let mut measure = StackMeasure::default();
        if let Some(session_dir) = self.session_dir(session) {
            let dir = session_dir.join(Self::path_hash(key));
            if let Ok(Some((meta, _))) = self.read_disk_meta_value(session, key) {
                measure.entries = meta.count;
            }
            let (files, bytes) = dir_file_totals(&dir);
            measure.files = files;
            measure.bytes = bytes;
        }
        if let Some((pool, harness)) = self.db_pool_and_harness() {
            if let Ok(conn) = pool.lock() {
                measure.db_rows = crate::db::backups::count_backups_for_path(
                    &conn,
                    &harness,
                    session,
                    &Self::path_hash(key),
                )
                .unwrap_or(0);
            }
        }
        measure.entries = measure.entries.max(measure.db_rows);
        measure
    }

    /// Remove one stack's rows and files. The caller holds its disk lock.
    fn purge_stack_locked(&mut self, session: &str, key: &Path) -> Result<(), StackPurgeFailure> {
        // Rows first: a row whose content file is gone is history that previews
        // list but undo cannot restore, while files without rows are still read
        // from disk correctly.
        self.remove_db_backups(session, key)
            .map_err(|error| StackPurgeFailure {
                hidden: false,
                message: format!("database rows not deleted: {error}"),
            })?;
        if let Some(session_dir) = self.session_dir(session) {
            let meta = session_dir.join(Self::path_hash(key)).join("meta.json");
            match std::fs::remove_file(&meta) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(StackPurgeFailure {
                        hidden: false,
                        message: format!("could not remove {}: {error}", meta.display()),
                    });
                }
            }
        }
        self.remove_disk_backups_locked(session, key)
            .map_err(|error| StackPurgeFailure {
                hidden: true,
                message: format!("undo no longer sees the stack, but files remain: {error}"),
            })
    }

    /// Forget a stack another store already removed from disk and database.
    fn evict_purged_stack(&mut self, session: &str, key: &Path) {
        self.restore_in_memory_stack(session, key, None);
        if let Some(files) = self.disk_index.get_mut(session) {
            files.remove(key);
            if files.is_empty() {
                self.disk_index.remove(session);
            }
        }
        self.set_db_mirror_synced(session, key, false);
    }
}

#[cfg(test)]
mod tests;
