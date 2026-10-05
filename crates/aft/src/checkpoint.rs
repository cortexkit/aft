use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::backup::{hash_session, BackupStore, CapturedRegularFile, DiskFileVersion};
use crate::error::AftError;
use crate::fs_lock;

const CHECKPOINT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Named checkpoints are deliberately bounded per session so a busy session
/// cannot grow an unbounded durable artifact store.
const MAX_NAMED_CHECKPOINTS_PER_SESSION: usize = 20;
/// Durable named checkpoints keep decisions long enough to survive ordinary
/// work interruptions without becoming permanent storage.
const NAMED_CHECKPOINT_RETENTION_DAYS: u64 = 14;
const NAMED_CHECKPOINT_RETENTION_SECS: u64 = NAMED_CHECKPOINT_RETENTION_DAYS * 24 * 60 * 60;
const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const UNBOUND_HARNESS_SEGMENT: &str = "unbound";

static CHECKPOINT_MAINTENANCE_KEYS: LazyLock<Mutex<HashSet<(PathBuf, String)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// User-visible explanation when no durable checkpoints were found after hydration.
pub const CHECKPOINT_RESTART_NOTICE: &str =
    "no durable checkpoints found on disk; in-memory checkpoints do not survive restarts";
/// User-visible explanation when a checkpoint list was hydrated from disk.
pub const CHECKPOINT_HYDRATED_NOTICE: &str =
    "durable checkpoints are hydrated from disk and survive restarts";

/// Describe the durable location for a successful checkpoint.
pub fn checkpoint_durability(storage_path: &Path) -> String {
    format!(
        "durable on disk at {}; survives restarts",
        storage_path.display()
    )
}

/// Metadata about a checkpoint, returned by list/create/restore.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub name: String,
    pub file_count: usize,
    pub created_at: u64,
    /// Durable checkpoint directory, when the store has a storage namespace.
    pub storage_path: Option<PathBuf>,
    /// Older checkpoint names evicted to keep the per-session retention cap.
    pub evicted: Vec<String>,
    /// Paths that could not be snapshotted (e.g. deleted since last edit),
    /// paired with the OS-level error that stopped us from reading them.
    /// Empty on successful round-trips. Populated only on `create()` — the
    /// `list()` / `restore()` paths leave it empty.
    pub skipped: Vec<(PathBuf, String)>,
    /// Files the operation covered, sorted: the snapshotted paths for
    /// `create()`, the written paths for a restore. Empty for `list()`.
    pub paths: Vec<PathBuf>,
    /// Restore only: the subset of `paths` whose on-disk state already matched
    /// the checkpoint, so restoring them changed nothing.
    pub unchanged: Vec<PathBuf>,
}

/// A stored checkpoint: a snapshot of multiple file contents and metadata.
#[derive(Debug, Clone)]
struct Checkpoint {
    name: String,
    file_contents: HashMap<PathBuf, CheckpointFile>,
    created_at: u64,
    /// Nanosecond-resolution creation ordering prevents ties from making
    /// retention nondeterministic when callers create several checkpoints in a second.
    created_order: u64,
}

#[derive(Debug, Clone)]
struct CachedCheckpoint {
    metadata_bytes: Vec<u8>,
    blobs: Vec<(PathBuf, DiskFileVersion)>,
    checkpoint: Checkpoint,
}

#[derive(Debug, Clone)]
struct CheckpointFile {
    /// Fresh in-memory checkpoints retain the platform metadata so restore keeps
    /// its existing behavior. Disk hydration rebuilds from the portable mode.
    metadata: Option<fs::Metadata>,
    mode: Option<u32>,
    kind: CheckpointFileKind,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskCheckpointMeta {
    schema_version: u32,
    session_id: String,
    name: String,
    created_at: u64,
    created_order: u64,
    files: Vec<DiskCheckpointFileMeta>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskCheckpointFileMeta {
    original_path: String,
    blob: String,
    kind: DiskCheckpointFileKind,
    mode: Option<u32>,
    target_is_dir: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DiskCheckpointFileKind {
    Regular,
    Symlink,
}

#[derive(Debug, Clone)]
enum CheckpointFileKind {
    Regular {
        bytes: Arc<[u8]>,
    },
    Symlink {
        target: PathBuf,
        target_is_dir: bool,
    },
}

impl CheckpointFile {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let target = fs::read_link(path)?;
            let target_is_dir = fs::metadata(path)
                .map(|target_metadata| target_metadata.is_dir())
                .unwrap_or(false);
            return Ok(Self {
                mode: checkpoint_mode(&metadata),
                metadata: Some(metadata),
                kind: CheckpointFileKind::Symlink {
                    target,
                    target_is_dir,
                },
            });
        }

        if metadata.is_file() {
            let capture = CapturedRegularFile::read(path)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file changed while being captured",
                )
            })?;
            return Ok(Self::from_fresh_capture(capture));
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file or symlink",
        ))
    }

    /// Build a checkpoint from bytes captured earlier in the command.
    ///
    /// Size and modification time are checked immediately before the bytes enter
    /// the checkpoint. If either changed, the capture is refreshed from disk so
    /// rollback and undo never preserve stale pre-edit content. This constructor
    /// is only for regular files; symlinks continue through [`Self::read`].
    fn from_captured(path: &Path, capture: &mut CapturedRegularFile) -> io::Result<Self> {
        capture.refresh_if_stale(path)?;
        let metadata = capture.metadata().clone();
        Ok(Self {
            mode: checkpoint_mode(&metadata),
            metadata: Some(metadata),
            kind: CheckpointFileKind::Regular {
                bytes: capture.shared_bytes(),
            },
        })
    }

    fn from_fresh_capture(capture: CapturedRegularFile) -> Self {
        let metadata = capture.metadata().clone();
        Self {
            mode: checkpoint_mode(&metadata),
            metadata: Some(metadata),
            kind: CheckpointFileKind::Regular {
                bytes: capture.shared_bytes(),
            },
        }
    }

    fn read_optional(path: &Path) -> io::Result<Option<Self>> {
        match Self::read(path) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether this on-disk state is what restoring `stored` would produce:
    /// the same kind, the same bytes or link target, and (where the platform
    /// records one) the same mode.
    fn matches(&self, stored: &CheckpointFile) -> bool {
        let same_mode = stored.mode.is_none() || self.mode == stored.mode;
        match (&self.kind, &stored.kind) {
            (
                CheckpointFileKind::Regular { bytes: current },
                CheckpointFileKind::Regular { bytes: stored },
            ) => same_mode && current == stored,
            (
                CheckpointFileKind::Symlink {
                    target: current, ..
                },
                CheckpointFileKind::Symlink { target: stored, .. },
            ) => current == stored,
            _ => false,
        }
    }

    fn from_disk(meta: &DiskCheckpointFileMeta, bytes: Vec<u8>) -> Result<Self, String> {
        let kind = match &meta.kind {
            DiskCheckpointFileKind::Regular => CheckpointFileKind::Regular {
                bytes: bytes.into(),
            },
            DiskCheckpointFileKind::Symlink => {
                let target = String::from_utf8(bytes)
                    .map(PathBuf::from)
                    .map_err(|error| format!("checkpoint symlink target is not UTF-8: {error}"))?;
                CheckpointFileKind::Symlink {
                    target,
                    target_is_dir: meta.target_is_dir,
                }
            }
        };
        Ok(Self {
            metadata: None,
            mode: meta.mode,
            kind,
        })
    }
}

#[cfg(unix)]
fn checkpoint_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn checkpoint_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// Workspace-wide, per-session checkpoint store.
///
/// Partitioned by session: two sessions sharing one bridge can both create
/// checkpoints named `snap1` without collision, and restoring from one session
/// does not leak the other's file set. The durable disk tree is authoritative;
/// in-memory entries are rehydrated under the mutation lock before each read or
/// change that depends on them.
#[derive(Debug)]
pub struct CheckpointStore {
    /// session -> name -> checkpoint, derived from the durable disk tree.
    checkpoints: HashMap<String, HashMap<String, Checkpoint>>,
    hydrated_checkpoints: Mutex<HashMap<PathBuf, CachedCheckpoint>>,
    lock_path: PathBuf,
    lock_timeout: Duration,
    storage_dir: Option<PathBuf>,
    storage_harness: Option<String>,
    blob_counter: AtomicU64,
    /// Namespace selected by configure but not yet applied; see
    /// [`CheckpointNamespaceRequest`].
    namespace_request: CheckpointNamespaceRequest,
    /// `(storage_dir, harness)` whose `unbound` checkpoints still need moving
    /// into the harness directory. Set when the store leaves the unbound
    /// namespace, cleared once the move ran under the mutation lock.
    pending_unbound_migration: Option<(PathBuf, String)>,
    #[cfg(test)]
    blob_reads: AtomicU64,
}

/// The durable namespace a configure selected for a [`CheckpointStore`],
/// handed over without touching the store's own mutex.
///
/// Configure runs before the bind reply and must not wait: the store's mutex
/// can be held by a checkpoint command that is itself waiting up to
/// `CHECKPOINT_LOCK_TIMEOUT` for the cross-process checkpoint file lock.
/// Configure therefore only records the request here (a short in-memory
/// critical section), and the store applies it at the start of its next
/// operation, before it reads or writes any checkpoint.
#[derive(Debug, Clone, Default)]
pub struct CheckpointNamespaceRequest(Arc<Mutex<Option<(PathBuf, String)>>>);

impl CheckpointNamespaceRequest {
    /// Ask the store to use `<dir>/<harness>/checkpoints`. A later request
    /// replaces an earlier one that has not been applied yet.
    pub fn request(&self, dir: PathBuf, harness: crate::harness::Harness) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((dir, harness.storage_segment()));
    }

    fn take(&self) -> Option<(PathBuf, String)> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Owns a checkpoint mutation lock and removes its project scope directory after
/// the filesystem lock has released. The directory scopes only the transient
/// lockfile; durable checkpoint bytes live under the harness namespace instead.
///
/// Releasing the lock leaves the scope directory in place. Removing it on
/// release deleted the directory other acquirers were blocked on, so with two
/// or more waiters one of them failed with NotFound. Empty scope directories
/// are reaped by `sweep_empty_scope_dirs` during cleanup instead.
struct CheckpointLockGuard {
    _guard: fs_lock::LockGuard,
}

impl CheckpointStore {
    pub fn new() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let project_key = crate::path_identity::project_scope_key(&project_root);
        let storage_dir = crate::bash_background::storage_dir(None);
        let lock_path = storage_dir
            .join("checkpoints")
            .join(project_key)
            .join("checkpoint.lock");
        let mut store = Self::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        // Commands received before configure still need an honest durable home.
        // Configure replaces this isolated namespace with the concrete harness.
        store.storage_dir = Some(storage_dir);
        store.storage_harness = Some(UNBOUND_HARNESS_SEGMENT.to_string());
        store
    }

    /// A store in the state `new()` leaves it before configure (durable
    /// checkpoints under `<storage>/unbound/checkpoints`), but with `<storage>`
    /// a test-owned directory instead of the process-wide cache.
    #[cfg(test)]
    pub(crate) fn unbound_in_for_test(storage_dir: &Path) -> Self {
        let lock_path = storage_dir
            .join("checkpoints")
            .join("test-project")
            .join("checkpoint.lock");
        let mut store = Self::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        store.storage_dir = Some(storage_dir.to_path_buf());
        store.storage_harness = Some(UNBOUND_HARNESS_SEGMENT.to_string());
        store
    }

    /// Point this store's mutation lock at a private path. Tests use this for
    /// isolation instead of mutating the process-global `AFT_CACHE_DIR` env
    /// var, which races parallel lib tests that resolve storage paths.
    #[cfg(test)]
    pub(crate) fn set_lock_path_for_test(&mut self, lock_path: PathBuf) {
        self.storage_dir = lock_path.parent().map(Path::to_path_buf);
        self.storage_harness = Some("test".to_string());
        self.lock_path = lock_path;
    }

    /// Select the harness-scoped durable namespace and, when leaving the
    /// unbound namespace, move checkpoints saved before configure into it.
    /// Rebinding a different namespace drops only the derived in-memory cache;
    /// disk remains authoritative. This may wait for the checkpoint file lock,
    /// so it belongs in deferred maintenance, never before a bind reply; use
    /// [`CheckpointNamespaceRequest`] there instead.
    pub fn set_storage_dir_for_harness(&mut self, dir: PathBuf, harness: crate::harness::Harness) {
        self.select_namespace(dir, harness.storage_segment());
        // Configure may have published a namespace request after the job that
        // called this was queued; the latest request wins.
        self.apply_namespace_request();
        if self.pending_unbound_migration.is_some() {
            match self.acquire_mutation_lock() {
                Ok(_lock) => self.run_pending_unbound_migration_locked(),
                // Left pending: the next operation retries under its own lock.
                Err(error) => crate::slog_warn!(
                    "could not migrate unbound durable checkpoints yet: {}",
                    error
                ),
            }
        }
    }

    /// Handle through which configure selects this store's namespace without
    /// taking the store's mutex.
    pub fn namespace_request(&self) -> CheckpointNamespaceRequest {
        self.namespace_request.clone()
    }

    /// Make this store read namespace requests from `request`, the handle the
    /// `AppContext` that now owns this store gives configure.
    #[cfg(test)]
    pub(crate) fn use_namespace_request(&mut self, request: CheckpointNamespaceRequest) {
        self.namespace_request = request;
    }

    /// Switch the in-memory target directory. No I/O and no file lock; a move
    /// of unbound checkpoints is only recorded, for
    /// `run_pending_unbound_migration_locked`.
    fn select_namespace(&mut self, dir: PathBuf, harness: String) {
        if self.storage_dir.as_ref() == Some(&dir)
            && self.storage_harness.as_deref() == Some(harness.as_str())
        {
            return;
        }
        if self.storage_dir.as_ref() == Some(&dir)
            && self.storage_harness.as_deref() == Some(UNBOUND_HARNESS_SEGMENT)
        {
            self.pending_unbound_migration = Some((dir.clone(), harness.clone()));
        }
        self.storage_dir = Some(dir);
        self.storage_harness = Some(harness);
        self.checkpoints.clear();
        self.hydrated_checkpoints
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn apply_namespace_request(&mut self) {
        if let Some((dir, harness)) = self.namespace_request.take() {
            self.select_namespace(dir, harness);
        }
        // A root actor serves routes from several harnesses. Configure records
        // whichever one bound last, not necessarily the route issuing this
        // checkpoint operation. Use that route's namespace for both durable
        // lookup and creation, clearing the derived cache when it changes.
        if let (Some(dir), Some(harness)) = (
            self.storage_dir.clone(),
            crate::backup::request_harness_segment(),
        ) {
            self.select_namespace(dir, harness);
        }
    }

    /// Caller holds the checkpoint mutation lock.
    fn run_pending_unbound_migration_locked(&mut self) {
        if let Some((dir, harness)) = self.pending_unbound_migration.take() {
            migrate_unbound_checkpoint_namespace(&dir, &harness);
        }
    }

    fn with_lock_path(lock_path: PathBuf, lock_timeout: Duration) -> Self {
        CheckpointStore {
            checkpoints: HashMap::new(),
            hydrated_checkpoints: Mutex::new(HashMap::new()),
            lock_path,
            lock_timeout,
            storage_dir: None,
            storage_harness: None,
            blob_counter: AtomicU64::new(0),
            namespace_request: CheckpointNamespaceRequest::default(),
            pending_unbound_migration: None,
            #[cfg(test)]
            blob_reads: AtomicU64::new(0),
        }
    }

    fn acquire_mutation_lock(&self) -> Result<CheckpointLockGuard, AftError> {
        let scope_dir = self.lock_path.parent().map(Path::to_path_buf);
        let deadline = Instant::now() + self.lock_timeout;
        let acquire_result = loop {
            if let Some(parent) = scope_dir.as_deref() {
                crate::backup::create_private_dir_all(parent).map_err(|error| {
                    AftError::IoError {
                        path: parent.display().to_string(),
                        message: format!("failed to create checkpoint lock directory: {error}"),
                    }
                })?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match fs_lock::try_acquire(&self.lock_path, remaining) {
                // The cleanup sweep may remove an empty scope directory between
                // our create_dir_all and the lock-file creation. Recreate it and
                // keep trying until the caller's deadline, however many times it
                // happens, instead of failing the waiter.
                Err(fs_lock::AcquireError::Io(error))
                    if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline =>
                {
                    continue;
                }
                result => break result,
            }
        };
        let guard = acquire_result.map_err(|error| match error {
            fs_lock::AcquireError::Timeout => AftError::IoError {
                path: self.lock_path.display().to_string(),
                message: "timed out acquiring checkpoint mutation lock".to_string(),
            },
            fs_lock::AcquireError::Io(error) => AftError::IoError {
                path: self.lock_path.display().to_string(),
                message: format!("failed to acquire checkpoint mutation lock: {error}"),
            },
        })?;

        Ok(CheckpointLockGuard { _guard: guard })
    }

    /// Create a checkpoint by reading the given files, scoped to `session`.
    ///
    /// If `files` is empty, snapshots all tracked files for **that session**
    /// from the BackupStore (other sessions' tracked files are not visible).
    /// Overwrites any existing checkpoint with the same name in this session.
    ///
    /// Unreadable paths (e.g. deleted since their last edit) are skipped with
    /// a warning instead of failing the whole checkpoint. The paths and their
    /// errors are returned via `CheckpointInfo::skipped` so callers can
    /// surface them. A checkpoint is only rejected outright when *every*
    /// requested path fails — that case still returns a `FileNotFound`
    /// error so callers can distinguish "partial success" from "nothing
    /// snapshotted at all".
    pub fn create(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: &BackupStore,
    ) -> Result<CheckpointInfo, AftError> {
        self.create_impl(session, name, files, Some(backup_store), None)
    }

    /// Checkpoint exactly `files`, with no fallback to the backup store's
    /// tracked files. Used by undo to preserve content changed outside AFT
    /// while the backup store is already borrowed for the restore.
    pub fn create_for_files(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
    ) -> Result<CheckpointInfo, AftError> {
        self.create_impl(session, name, files, None, None)
    }

    pub(crate) fn create_from_captures(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: &BackupStore,
        captures: &mut HashMap<PathBuf, CapturedRegularFile>,
    ) -> Result<CheckpointInfo, AftError> {
        self.create_impl(session, name, files, Some(backup_store), Some(captures))
    }

    fn create_impl(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: Option<&BackupStore>,
        mut captures: Option<&mut HashMap<PathBuf, CapturedRegularFile>>,
    ) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        validate_checkpoint_name(name)?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let explicit_request = !files.is_empty();
        let file_list = match backup_store {
            Some(backup_store) if files.is_empty() => backup_store.tracked_files(session),
            _ => files,
        };

        let mut file_contents = HashMap::new();
        let mut skipped: Vec<(PathBuf, String)> = Vec::new();
        for path in &file_list {
            let seeded = captures
                .as_deref_mut()
                .and_then(|captures| captures.get_mut(path))
                .map(|capture| CheckpointFile::from_captured(path, capture));
            let snapshot = match seeded {
                Some(Err(error)) if error.kind() == io::ErrorKind::InvalidInput => {
                    if let Some(captures) = captures.as_deref_mut() {
                        captures.remove(path);
                    }
                    CheckpointFile::read(path)
                }
                Some(result) => result,
                None => CheckpointFile::read(path),
            };
            match snapshot {
                Ok(snapshot) => {
                    file_contents.insert(path.clone(), snapshot);
                }
                Err(e) => {
                    crate::slog_warn!(
                        "checkpoint {}: skipping unreadable file {}: {}",
                        name,
                        path.display(),
                        e
                    );
                    skipped.push((path.clone(), e.to_string()));
                }
            }
        }

        // A caller that named files and got none of them has no checkpoint at
        // all; report that as an error rather than an empty success. Every
        // named path either lands in the checkpoint or in `skipped`, so the
        // first skip carries the reason. For empty `files` (tracked-file
        // fallback) with no readable files, the empty checkpoint is a
        // legitimate "nothing to snapshot" outcome and we keep it.
        if explicit_request && file_contents.is_empty() {
            let path = match skipped.first() {
                Some((path, err)) => format!("{}: {}", path.display(), err),
                None => "none of the requested files could be checkpointed".to_string(),
            };
            return Err(AftError::FileNotFound { path });
        }

        let created_at = current_timestamp();
        let created_order = current_timestamp_nanos()
            .saturating_add(self.blob_counter.fetch_add(1, Ordering::Relaxed));
        let file_count = file_contents.len();
        let mut captured_paths = file_contents.keys().cloned().collect::<Vec<_>>();
        captured_paths.sort();
        let checkpoint = Checkpoint {
            name: name.to_string(),
            file_contents,
            created_at,
            created_order,
        };
        let storage_path = self.durable_checkpoint_dir(session, name);

        self.persist_checkpoint_locked(session, &checkpoint)?;
        self.checkpoints
            .entry(session.to_string())
            .or_default()
            .insert(name.to_string(), checkpoint);

        let evicted = self.evict_excess_checkpoints_locked(session)?;

        if skipped.is_empty() {
            crate::slog_info!("checkpoint created: {} ({} files)", name, file_count);
        } else {
            crate::slog_info!(
                "checkpoint created: {} ({} files, {} skipped)",
                name,
                file_count,
                skipped.len()
            );
        }

        Ok(CheckpointInfo {
            name: name.to_string(),
            file_count,
            created_at,
            storage_path,
            evicted,
            skipped,
            paths: captured_paths,
            unchanged: Vec::new(),
        })
    }

    /// Restore a checkpoint by overwriting files with stored content.
    pub fn restore(&mut self, session: &str, name: &str) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let storage_path = self.durable_checkpoint_dir(session, name);
        let checkpoint = self.get(session, name)?;
        let mut paths = checkpoint.file_contents.keys().cloned().collect::<Vec<_>>();
        paths.sort();

        let unchanged = restore_paths_atomically(checkpoint, &paths)?;
        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            storage_path,
            evicted: Vec::new(),
            skipped: Vec::new(),
            paths,
            unchanged,
        })
    }

    /// Restore a checkpoint using a caller-validated path list.
    pub fn restore_validated(
        &mut self,
        session: &str,
        name: &str,
        validated_paths: &[PathBuf],
    ) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let storage_path = self.durable_checkpoint_dir(session, name);
        let checkpoint = self.get(session, name)?;

        for path in validated_paths {
            checkpoint
                .file_contents
                .get(path)
                .ok_or_else(|| AftError::FileNotFound {
                    path: path.display().to_string(),
                })?;
        }
        let unchanged = restore_paths_atomically(checkpoint, validated_paths)?;
        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            storage_path,
            evicted: Vec::new(),
            skipped: Vec::new(),
            paths: validated_paths.to_vec(),
            unchanged,
        })
    }

    /// Return the file paths stored for a checkpoint.
    pub fn file_paths(&mut self, session: &str, name: &str) -> Result<Vec<PathBuf>, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let checkpoint = self.get(session, name)?;
        Ok(checkpoint.file_contents.keys().cloned().collect())
    }

    /// Return absolute file paths stored for a checkpoint without restoring it.
    pub fn absolute_file_paths(
        &mut self,
        session: &str,
        name: &str,
    ) -> Result<Vec<PathBuf>, AftError> {
        let mut paths: Vec<PathBuf> = self
            .file_paths(session, name)?
            .into_iter()
            .map(absolute_checkpoint_path)
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// Delete a checkpoint from a session. Returns true when a checkpoint was removed.
    pub fn delete(&mut self, session: &str, name: &str) -> bool {
        let _mutation_lock = match self.acquire_mutation_lock() {
            Ok(lock) => lock,
            Err(error) => {
                crate::slog_warn!("checkpoint delete lock failed for {}: {}", name, error);
                return false;
            }
        };
        if let Err(error) = self.run_process_maintenance_once_locked() {
            crate::slog_warn!(
                "checkpoint delete maintenance failed for {}: {}",
                name,
                error
            );
            return false;
        }
        if let Err(error) = self.hydrate_session_locked(session) {
            crate::slog_warn!("checkpoint delete hydration failed for {}: {}", name, error);
            return false;
        }
        if self
            .checkpoints
            .get(session)
            .is_none_or(|checkpoints| !checkpoints.contains_key(name))
        {
            return false;
        }
        if let Err(error) = self.remove_checkpoint_from_disk_locked(session, name) {
            crate::slog_warn!("checkpoint delete failed for {}: {}", name, error);
            return false;
        }
        let Some(session_checkpoints) = self.checkpoints.get_mut(session) else {
            return false;
        };
        let removed = session_checkpoints.remove(name).is_some();
        if session_checkpoints.is_empty() {
            self.checkpoints.remove(session);
        }
        removed
    }

    /// List all checkpoints for this session with metadata, hydrating from the
    /// authoritative durable tree before returning.
    pub fn list(&mut self, session: &str) -> Result<Vec<CheckpointInfo>, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let mut list = self
            .checkpoints
            .get(session)
            .map(|checkpoints| {
                checkpoints
                    .values()
                    .map(|checkpoint| CheckpointInfo {
                        name: checkpoint.name.clone(),
                        file_count: checkpoint.file_contents.len(),
                        created_at: checkpoint.created_at,
                        storage_path: self.durable_checkpoint_dir(session, &checkpoint.name),
                        evicted: Vec::new(),
                        skipped: Vec::new(),
                        paths: Vec::new(),
                        unchanged: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        list.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(list)
    }

    /// Total checkpoint count across all sessions already hydrated in this process.
    pub fn total_count(&self) -> usize {
        self.checkpoints
            .values()
            .map(|checkpoints| checkpoints.len())
            .sum()
    }

    /// Sweep checkpoints older than the fixed fourteen-day retention window.
    /// The limit is intentionally not configurable: named checkpoints protect
    /// irreplaceable decisions, while predictable retention keeps the store bounded.
    pub fn cleanup(&mut self) {
        let _mutation_lock = match self.acquire_mutation_lock() {
            Ok(lock) => lock,
            Err(error) => {
                crate::slog_warn!("checkpoint cleanup lock failed: {}", error);
                return;
            }
        };
        self.apply_namespace_request();
        self.run_pending_unbound_migration_locked();
        if let Err(error) = self.cleanup_locked() {
            crate::slog_warn!("checkpoint cleanup failed: {}", error);
        }
    }

    fn get(&self, session: &str, name: &str) -> Result<&Checkpoint, AftError> {
        self.checkpoints
            .get(session)
            .and_then(|checkpoints| checkpoints.get(name))
            .ok_or_else(|| AftError::CheckpointNotFound {
                name: name.to_string(),
            })
    }

    fn durable_checkpoints_dir(&self) -> Option<PathBuf> {
        self.storage_dir
            .as_ref()
            .zip(self.storage_harness.as_ref())
            .map(|(storage_dir, harness)| storage_dir.join(harness).join("checkpoints"))
    }

    fn durable_session_dir(&self, session: &str) -> Option<PathBuf> {
        self.durable_checkpoints_dir()
            .map(|checkpoints_dir| checkpoints_dir.join(hash_session(session)))
    }

    fn durable_checkpoint_dir(&self, session: &str, name: &str) -> Option<PathBuf> {
        self.durable_session_dir(session)
            .map(|session_dir| session_dir.join(name))
    }

    fn run_process_maintenance_once_locked(&mut self) -> Result<(), AftError> {
        // Every operation passes through here right after taking the mutation
        // lock, so a namespace configure selected, and any move of unbound
        // checkpoints it implies, takes effect before this operation reads or
        // writes a checkpoint.
        self.apply_namespace_request();
        self.run_pending_unbound_migration_locked();
        let Some(storage_dir) = self.storage_dir.clone() else {
            return Ok(());
        };
        let Some(harness) = self.storage_harness.clone() else {
            return Ok(());
        };
        if !CHECKPOINT_MAINTENANCE_KEYS
            .lock()
            .unwrap()
            .insert((storage_dir, harness))
        {
            return Ok(());
        }
        self.cleanup_locked()?;
        self.tighten_permissions_locked();
        Ok(())
    }

    /// Protect the namespace boundary without visiting historical blobs.
    fn tighten_permissions_locked(&self) {
        let Some(checkpoints_dir) = self.durable_checkpoints_dir() else {
            return;
        };
        if let Some(root) = self.storage_dir.as_deref() {
            crate::private_storage::tighten_root(root);
            crate::private_storage::tighten_open_dir(root, &checkpoints_dir);
        }
    }

    fn cleanup_locked(&mut self) -> Result<(), AftError> {
        self.hydrated_checkpoints
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let now = current_timestamp();
        self.checkpoints.retain(|_, session_checkpoints| {
            session_checkpoints.retain(|_, checkpoint| {
                now.saturating_sub(checkpoint.created_at) < NAMED_CHECKPOINT_RETENTION_SECS
            });
            !session_checkpoints.is_empty()
        });

        if let Some(checkpoints_dir) = self.durable_checkpoints_dir() {
            sweep_expired_durable_checkpoints(&checkpoints_dir, now);
        }
        if let Some(checkpoints_root) = self.lock_path.parent().and_then(Path::parent) {
            // Fail-closed guard: the sweep root is DERIVED from lock_path depth, and a
            // caller with a nonstandard (shallower) lock path would resolve this to an
            // unrelated directory - in tests, the OS temp root itself, where removing
            // "empty scope dirs" deletes other processes' freshly created temp dirs.
            // Only a directory actually named `checkpoints` is a legitimate sweep root.
            if checkpoints_root.file_name() == Some(std::ffi::OsStr::new("checkpoints")) {
                sweep_empty_scope_dirs(checkpoints_root);
            }
        }
        Ok(())
    }

    fn hydrate_session_locked(&mut self, session: &str) -> Result<(), AftError> {
        let Some(session_dir) = self.durable_session_dir(session) else {
            return Ok(());
        };
        if !session_dir.exists() {
            self.checkpoints.remove(session);
            return Ok(());
        }

        recover_replaced_checkpoints(&session_dir);
        let entries = fs::read_dir(&session_dir).map_err(|error| AftError::IoError {
            path: session_dir.display().to_string(),
            message: format!("failed to read durable checkpoint session: {error}"),
        })?;
        let mut hydrated = HashMap::new();
        for entry in entries {
            let entry = entry.map_err(|error| AftError::IoError {
                path: session_dir.display().to_string(),
                message: format!("failed to read durable checkpoint entry: {error}"),
            })?;
            let checkpoint_dir = entry.path();
            if !entry
                .file_type()
                .map_err(|error| AftError::IoError {
                    path: checkpoint_dir.display().to_string(),
                    message: format!("failed to inspect durable checkpoint entry: {error}"),
                })?
                .is_dir()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_safe_checkpoint_name(&name) || name.ends_with(REPLACED_CHECKPOINT_SUFFIX) {
                continue;
            }
            let meta_path = checkpoint_dir.join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            let checkpoint = match read_checkpoint_from_disk(self, &checkpoint_dir, session, &name)
            {
                Ok(checkpoint) => checkpoint,
                Err(AftError::IoError { path, message })
                    if message.starts_with("failed to parse durable checkpoint metadata:") =>
                {
                    // A torn metadata file damages one checkpoint, not its
                    // neighbours. Keep the bytes for diagnosis and report why
                    // this name was omitted; newer formats still fail closed.
                    crate::slog_warn!("checkpoint {name} skipped: reason=corrupt_metadata path={path} error={message}");
                    crate::durability::record(
                        crate::durability::EventKind::CorruptCheckpointSkipped,
                        &meta_path,
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            hydrated.insert(name, checkpoint);
        }
        if hydrated.is_empty() {
            self.checkpoints.remove(session);
        } else {
            self.checkpoints.insert(session.to_string(), hydrated);
            self.evict_excess_checkpoints_locked(session)?;
        }
        // Keep cache ownership bounded to names still present in this session.
        let names = self.checkpoints.get(session);
        self.hydrated_checkpoints
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|path, _| {
                path.parent() != Some(session_dir.as_path())
                    || path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| names.is_some_and(|names| names.contains_key(name)))
            });
        Ok(())
    }

    fn persist_checkpoint_locked(
        &self,
        session: &str,
        checkpoint: &Checkpoint,
    ) -> Result<(), AftError> {
        let Some(checkpoint_dir) = self.durable_checkpoint_dir(session, &checkpoint.name) else {
            return Ok(());
        };
        crate::backup::create_private_durable_dir(&checkpoint_dir).map_err(|error| {
            AftError::IoError {
                path: checkpoint_dir.display().to_string(),
                message: format!("failed to create durable checkpoint directory: {error}"),
            }
        })?;
        if let Some(root) = self.storage_dir.as_deref() {
            crate::private_storage::tighten_open_dir(root, &checkpoint_dir);
        }

        let mut files = Vec::with_capacity(checkpoint.file_contents.len());
        let mut ledger_bytes = 0_u64;
        for (index, (path, file)) in checkpoint.file_contents.iter().enumerate() {
            let blob = format!(
                "file_{}_{}_{}.blob",
                checkpoint.created_order,
                index,
                self.blob_counter.fetch_add(1, Ordering::Relaxed)
            );
            let bytes = checkpoint_file_bytes(file);
            #[cfg(test)]
            if let Cow::Owned(bytes) = &bytes {
                CHECKPOINT_COPIED_BYTES.with(|count| count.set(count.get() + bytes.len()));
            }
            write_temp_fsync_rename(&checkpoint_dir, &blob, &bytes).map_err(|error| {
                AftError::IoError {
                    path: checkpoint_dir.join(&blob).display().to_string(),
                    message: format!("failed to write durable checkpoint blob: {error}"),
                }
            })?;
            ledger_bytes = ledger_bytes.saturating_add(bytes.len() as u64);
            files.push(DiskCheckpointFileMeta {
                original_path: path.display().to_string(),
                blob,
                kind: match &file.kind {
                    CheckpointFileKind::Regular { .. } => DiskCheckpointFileKind::Regular,
                    CheckpointFileKind::Symlink { .. } => DiskCheckpointFileKind::Symlink,
                },
                mode: file.mode,
                target_is_dir: matches!(
                    &file.kind,
                    CheckpointFileKind::Symlink {
                        target_is_dir: true,
                        ..
                    }
                ),
            });
        }

        let meta = DiskCheckpointMeta {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            session_id: session.to_string(),
            name: checkpoint.name.clone(),
            created_at: checkpoint.created_at,
            created_order: checkpoint.created_order,
            files,
        };
        let bytes = serde_json::to_vec_pretty(&meta).map_err(|error| AftError::IoError {
            path: checkpoint_dir.join("meta.json").display().to_string(),
            message: format!("failed to serialize durable checkpoint metadata: {error}"),
        })?;
        write_temp_fsync_rename(&checkpoint_dir, "meta.json", &bytes).map_err(|error| {
            AftError::IoError {
                path: checkpoint_dir.join("meta.json").display().to_string(),
                message: format!("failed to write durable checkpoint metadata: {error}"),
            }
        })?;
        ledger_bytes = ledger_bytes.saturating_add(bytes.len() as u64);
        fsync_dir(&checkpoint_dir).map_err(|error| AftError::IoError {
            path: checkpoint_dir.display().to_string(),
            message: format!("failed to sync durable checkpoint metadata: {error}"),
        })?;
        prune_unreferenced_checkpoint_blobs(&checkpoint_dir, &meta.files).map_err(|error| {
            AftError::IoError {
                path: checkpoint_dir.display().to_string(),
                message: format!("failed to prune stale durable checkpoint blobs: {error}"),
            }
        })?;
        crate::write_ledger::credit(
            crate::write_ledger::Domain::Checkpoints,
            checkpoint_dir.display().to_string(),
            ledger_bytes,
            0,
        );
        Ok(())
    }

    fn remove_checkpoint_from_disk_locked(
        &self,
        session: &str,
        name: &str,
    ) -> Result<(), AftError> {
        let Some(checkpoint_dir) = self.durable_checkpoint_dir(session, name) else {
            return Ok(());
        };
        self.hydrated_checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&checkpoint_dir);
        match fs::remove_dir_all(&checkpoint_dir) {
            Ok(()) => {
                if let Some(session_dir) = checkpoint_dir.parent() {
                    let _ = fs::remove_dir(session_dir);
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AftError::IoError {
                path: checkpoint_dir.display().to_string(),
                message: format!("failed to remove durable checkpoint: {error}"),
            }),
        }
    }

    fn evict_excess_checkpoints_locked(&mut self, session: &str) -> Result<Vec<String>, AftError> {
        let Some(checkpoints) = self.checkpoints.get(session) else {
            return Ok(Vec::new());
        };
        let overflow = checkpoints
            .len()
            .saturating_sub(MAX_NAMED_CHECKPOINTS_PER_SESSION);
        let mut checkpoints = checkpoints
            .values()
            .map(|checkpoint| (checkpoint.created_order, checkpoint.name.clone()))
            .collect::<Vec<_>>();
        checkpoints.sort();
        let evicted = checkpoints
            .into_iter()
            .take(overflow)
            .map(|(_, name)| name)
            .collect::<Vec<_>>();

        for name in &evicted {
            self.remove_checkpoint_from_disk_locked(session, name)?;
        }
        if let Some(checkpoints) = self.checkpoints.get_mut(session) {
            for name in &evicted {
                checkpoints.remove(name);
            }
        }
        Ok(evicted)
    }

    pub fn session_is_empty(&self, session: &str) -> bool {
        self.checkpoints.get(session).is_none_or(HashMap::is_empty)
    }
}

fn migrate_unbound_checkpoint_namespace(storage_dir: &Path, harness: &str) {
    let source = storage_dir
        .join(UNBOUND_HARNESS_SEGMENT)
        .join("checkpoints");
    if !source.exists() {
        return;
    }
    let target = storage_dir.join(harness).join("checkpoints");
    if !target.exists() {
        if let Some(parent) = target.parent() {
            if let Err(error) = crate::backup::create_private_durable_dir(parent) {
                crate::slog_warn!(
                    "failed to create durable checkpoint harness directory {}: {}",
                    parent.display(),
                    error
                );
                return;
            }
        }
        if let Err(error) = fs::rename(&source, &target) {
            crate::slog_warn!(
                "failed to move unbound durable checkpoints into {}: {}",
                target.display(),
                error
            );
        }
        return;
    }

    // A vanished mounted child can make ReadDir::drop panic after closedir
    // returns ENXIO, aborting the daemon. Keep namespace migration on its root
    // filesystem before opening session directories.
    let Ok(boundary) = crate::walk_boundary::DeviceBoundary::for_root(&source) else {
        crate::slog_warn!(
            "cannot establish filesystem boundary for checkpoint migration {}",
            source.display()
        );
        return;
    };
    let mut skipped_foreign_mounts = 0usize;
    let Ok(session_entries) = fs::read_dir(&source) else {
        return;
    };
    for session_entry in session_entries.flatten() {
        let source_session = session_entry.path();
        if !source_session.is_dir() {
            continue;
        }
        if !boundary.should_descend(&source_session).unwrap_or(false) {
            skipped_foreign_mounts += 1;
            continue;
        }
        let target_session = target.join(session_entry.file_name());
        if !target_session.exists() {
            let _ = fs::rename(&source_session, &target_session);
            continue;
        }
        let Ok(checkpoint_entries) = fs::read_dir(&source_session) else {
            continue;
        };
        for checkpoint_entry in checkpoint_entries.flatten() {
            let source_checkpoint = checkpoint_entry.path();
            let target_checkpoint = target_session.join(checkpoint_entry.file_name());
            if !target_checkpoint.exists() {
                let _ = fs::rename(source_checkpoint, target_checkpoint);
                continue;
            }
            // `<storage>/unbound/checkpoints` (written before configure) and
            // `<storage>/<harness>/checkpoints` both hold a checkpoint with this
            // name for this session. Saving under an existing name replaces it,
            // so the newer one is the checkpoint the session last asked for.
            // Keeping the harness copy regardless let an older snapshot answer
            // the next restore.
            let source_order = durable_checkpoint_created_order(&source_checkpoint);
            let target_order = durable_checkpoint_created_order(&target_checkpoint);
            let source_is_newer = match (source_order, target_order) {
                (Some(source), Some(target)) => source > target,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !source_is_newer {
                continue;
            }
            if let Err(error) = replace_checkpoint_dir(&source_checkpoint, &target_checkpoint) {
                crate::slog_warn!(
                    "failed to replace durable checkpoint {} with the newer unbound one: {}",
                    target_checkpoint.display(),
                    error
                );
            }
        }
        let _ = fs::remove_dir(&source_session);
    }
    let _ = fs::remove_dir(&source);
    if skipped_foreign_mounts > 0 {
        crate::slog_warn!(
            "checkpoint migration skipped {} foreign filesystem mount(s) below {}",
            skipped_foreign_mounts,
            source.display()
        );
    }
}

/// Creation order recorded in a durable checkpoint's metadata, or `None` when
/// the metadata is missing or unreadable.
fn durable_checkpoint_created_order(checkpoint_dir: &Path) -> Option<u64> {
    let bytes = fs::read(checkpoint_dir.join("meta.json")).ok()?;
    serde_json::from_slice::<DiskCheckpointMeta>(&bytes)
        .ok()
        .map(|meta| meta.created_order)
}

/// Suffix of the name a checkpoint directory is moved to while
/// `replace_checkpoint_dir` swaps a newer copy into its place. Names with it
/// are not checkpoints: hydration restores or removes them, and
/// `validate_checkpoint_name` refuses them.
const REPLACED_CHECKPOINT_SUFFIX: &str = ".aft-replaced";

fn replaced_checkpoint_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(REPLACED_CHECKPOINT_SUFFIX);
    target.with_file_name(name)
}

/// Replace the checkpoint directory `target` with `source` so that a failure
/// or crash at any step leaves one complete copy under `target`'s name, or
/// under its `.aft-replaced` name for `recover_replaced_checkpoints` to put
/// back. `target` is moved aside first, `source` moved into place, and only
/// then is the old copy deleted; if the move into place fails, the old copy is
/// moved back.
fn replace_checkpoint_dir(source: &Path, target: &Path) -> io::Result<()> {
    replace_checkpoint_dir_with(source, target, |from, to| fs::rename(from, to))
}

fn replace_checkpoint_dir_with(
    source: &Path,
    target: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let aside = replaced_checkpoint_path(target);
    // A `.aft-replaced` copy left by an earlier replacement of this same
    // checkpoint that finished but could not delete it. `target` exists and is
    // the newer copy, so this one is superseded.
    if aside.exists() {
        fs::remove_dir_all(&aside)?;
    }
    rename(target, &aside)?;
    if let Err(error) = rename(source, target) {
        if let Err(restore_error) = rename(&aside, target) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; moving the previous checkpoint back from {} also failed: {restore_error}",
                    aside.display()
                ),
            ));
        }
        return Err(error);
    }
    if let Err(error) = fs::remove_dir_all(&aside) {
        crate::slog_warn!(
            "replaced durable checkpoint {} but could not delete the previous copy {}: {}",
            target.display(),
            aside.display(),
            error
        );
    }
    Ok(())
}

/// Finish any `replace_checkpoint_dir` a crash interrupted in `session_dir`:
/// a `<name>.aft-replaced` directory goes back to `<name>` when nothing took
/// its place, and is deleted when the newer copy did.
fn recover_replaced_checkpoints(session_dir: &Path) {
    let Ok(entries) = fs::read_dir(session_dir) else {
        return;
    };
    let leftovers = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let original = name.strip_suffix(REPLACED_CHECKPOINT_SUFFIX)?.to_string();
            Some((entry.path(), session_dir.join(original)))
        })
        .collect::<Vec<_>>();
    for (aside, original) in leftovers {
        let result = if original.exists() {
            fs::remove_dir_all(&aside)
        } else {
            fs::rename(&aside, &original)
        };
        if let Err(error) = result {
            crate::slog_warn!(
                "failed to recover interrupted checkpoint replacement {}: {}",
                aside.display(),
                error
            );
        }
    }
}

fn validate_checkpoint_name(name: &str) -> Result<(), AftError> {
    if name.ends_with(REPLACED_CHECKPOINT_SUFFIX) {
        return Err(AftError::InvalidRequest {
            message: format!(
                "checkpoint names ending in '{REPLACED_CHECKPOINT_SUFFIX}' are reserved"
            ),
        });
    }
    if is_safe_checkpoint_name(name) {
        Ok(())
    } else {
        Err(AftError::InvalidRequest {
            message: "checkpoint name must be a single non-empty path component".to_string(),
        })
    }
}

fn is_safe_checkpoint_name(name: &str) -> bool {
    matches!(
        Path::new(name).components().collect::<Vec<_>>().as_slice(),
        [std::path::Component::Normal(_)]
    ) && !name.chars().any(|character| {
        character.is_control()
            || matches!(character, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    })
}

fn is_safe_blob_name(name: &str) -> bool {
    is_safe_checkpoint_name(name) && name.starts_with("file_") && name.ends_with(".blob")
}

fn read_checkpoint_from_disk(
    store: &CheckpointStore,
    checkpoint_dir: &Path,
    session: &str,
    expected_name: &str,
) -> Result<Checkpoint, AftError> {
    let meta_path = checkpoint_dir.join("meta.json");
    let bytes = fs::read(&meta_path).map_err(|error| AftError::IoError {
        path: meta_path.display().to_string(),
        message: format!("failed to read durable checkpoint metadata: {error}"),
    })?;
    let meta = serde_json::from_slice::<DiskCheckpointMeta>(&bytes).map_err(|error| {
        AftError::IoError {
            path: meta_path.display().to_string(),
            message: format!("failed to parse durable checkpoint metadata: {error}"),
        }
    })?;
    if meta.schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err(AftError::IoError {
            path: meta_path.display().to_string(),
            message: format!(
                "unsupported durable checkpoint metadata schema {}",
                meta.schema_version
            ),
        });
    }
    if meta.session_id != session
        || meta.name != expected_name
        || !is_safe_checkpoint_name(&meta.name)
    {
        return Err(AftError::IoError {
            path: meta_path.display().to_string(),
            message: "durable checkpoint metadata does not match its session or directory"
                .to_string(),
        });
    }

    {
        let cache = store
            .hydrated_checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(checkpoint_dir) {
            if cached.metadata_bytes == bytes
                && cached
                    .blobs
                    .iter()
                    .all(|(path, version)| DiskFileVersion::of_path(path) == Some(*version))
            {
                return Ok(cached.checkpoint.clone());
            }
        }
    }
    let mut file_contents = HashMap::with_capacity(meta.files.len());
    let mut versions = Vec::with_capacity(meta.files.len());
    let mut cacheable = cfg!(unix);
    for file in &meta.files {
        if !is_safe_blob_name(&file.blob) {
            return Err(AftError::IoError {
                path: meta_path.display().to_string(),
                message: format!("invalid durable checkpoint blob name {}", file.blob),
            });
        }
        let blob_path = checkpoint_dir.join(&file.blob);
        let version = DiskFileVersion::of_path(&blob_path);
        #[cfg(test)]
        store.blob_reads.fetch_add(1, Ordering::Relaxed);
        let blob = fs::read(&blob_path).map_err(|error| AftError::IoError {
            path: blob_path.display().to_string(),
            message: format!("failed to read durable checkpoint blob: {error}"),
        })?;
        if let Some(version) =
            version.filter(|version| DiskFileVersion::of_path(&blob_path) == Some(*version))
        {
            versions.push((blob_path.clone(), version));
        } else {
            cacheable = false;
        }
        let path = PathBuf::from(&file.original_path);
        let checkpoint_file =
            CheckpointFile::from_disk(file, blob).map_err(|message| AftError::IoError {
                path: blob_path.display().to_string(),
                message,
            })?;
        if file_contents
            .insert(path.clone(), checkpoint_file)
            .is_some()
        {
            return Err(AftError::IoError {
                path: meta_path.display().to_string(),
                message: format!("duplicate durable checkpoint path {}", path.display()),
            });
        }
    }

    let checkpoint = Checkpoint {
        name: meta.name,
        file_contents,
        created_at: meta.created_at,
        created_order: meta.created_order,
    };
    if cacheable {
        store
            .hydrated_checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                checkpoint_dir.to_path_buf(),
                CachedCheckpoint {
                    metadata_bytes: bytes,
                    blobs: versions,
                    checkpoint: checkpoint.clone(),
                },
            );
    }
    Ok(checkpoint)
}

fn checkpoint_file_bytes(file: &CheckpointFile) -> Cow<'_, [u8]> {
    match &file.kind {
        CheckpointFileKind::Regular { bytes } => Cow::Borrowed(bytes),
        CheckpointFileKind::Symlink { target, .. } => {
            Cow::Owned(target.as_os_str().to_string_lossy().as_bytes().to_vec())
        }
    }
}

#[cfg(test)]
thread_local! {
    static CHECKPOINT_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn write_temp_fsync_rename(dir: &Path, final_name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        final_name,
        std::process::id(),
        current_timestamp_nanos()
    );
    let tmp_path = dir.join(tmp_name);
    let final_path = dir.join(final_name);
    {
        let mut options = crate::private_storage::options();
        options.write(true).create_new(true);
        // Checkpoint blobs are copies of user files (possibly secrets), so
        // they are owner-only from creation, like undo backups.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(crate::backup::PRIVATE_FILE_MODE);
        }
        let mut file = options.open(&tmp_path)?;
        file.write_all(bytes)?;
        crate::durability::sync_file(&file, &final_path)?;
    }
    fs::rename(tmp_path, final_path)
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> io::Result<()> {
    crate::durability::sync_dir(path)
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn prune_unreferenced_checkpoint_blobs(
    checkpoint_dir: &Path,
    files: &[DiskCheckpointFileMeta],
) -> io::Result<()> {
    let referenced = files
        .iter()
        .map(|file| file.blob.as_str())
        .collect::<HashSet<_>>();
    for entry in fs::read_dir(checkpoint_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if (name.starts_with("file_") && name.ends_with(".blob") && !referenced.contains(name))
            || name.contains(".tmp.")
            || name.ends_with(".tmp")
        {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn sweep_expired_durable_checkpoints(checkpoints_dir: &Path, now: u64) {
    // A vanished mounted child can make ReadDir::drop panic after closedir
    // returns ENXIO, aborting the daemon. This background sweep must not open
    // checkpoint directories on a different filesystem.
    let Ok(boundary) = crate::walk_boundary::DeviceBoundary::for_root(checkpoints_dir) else {
        crate::slog_warn!(
            "cannot establish filesystem boundary for checkpoint sweep {}",
            checkpoints_dir.display()
        );
        return;
    };
    let mut skipped_foreign_mounts = 0usize;
    let Ok(session_entries) = fs::read_dir(checkpoints_dir) else {
        return;
    };
    for session_entry in session_entries.flatten() {
        let session_dir = session_entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        if !boundary.should_descend(&session_dir).unwrap_or(false) {
            skipped_foreign_mounts += 1;
            continue;
        }
        let Ok(checkpoint_entries) = fs::read_dir(&session_dir) else {
            continue;
        };
        for checkpoint_entry in checkpoint_entries.flatten() {
            let checkpoint_dir = checkpoint_entry.path();
            if !checkpoint_dir.is_dir() {
                continue;
            }
            if !boundary.should_descend(&checkpoint_dir).unwrap_or(false) {
                skipped_foreign_mounts += 1;
                continue;
            }
            let meta_path = checkpoint_dir.join("meta.json");
            let Ok(bytes) = fs::read(&meta_path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<DiskCheckpointMeta>(&bytes) else {
                continue;
            };
            if now.saturating_sub(meta.created_at) < NAMED_CHECKPOINT_RETENTION_SECS {
                continue;
            }
            if let Err(error) = fs::remove_dir_all(&checkpoint_dir) {
                crate::slog_warn!(
                    "failed to remove expired durable checkpoint {}: {}",
                    checkpoint_dir.display(),
                    error
                );
            }
        }
        let _ = fs::remove_dir(&session_dir);
    }
    if skipped_foreign_mounts > 0 {
        crate::slog_warn!(
            "checkpoint sweep skipped {} foreign filesystem mount(s) below {}",
            skipped_foreign_mounts,
            checkpoints_dir.display()
        );
    }
}

fn absolute_checkpoint_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return normalize_checkpoint_path(&path);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    normalize_checkpoint_path(&cwd.join(path))
}

fn normalize_checkpoint_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Restore `paths` from `checkpoint`, all or nothing. Returns the paths whose
/// on-disk state already matched the checkpoint, so callers can tell a restore
/// that changed files from one that found them already in place.
fn restore_paths_atomically(
    checkpoint: &Checkpoint,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, AftError> {
    let mut pre_restore_snapshot: HashMap<PathBuf, Option<CheckpointFile>> = HashMap::new();
    let mut unchanged = Vec::new();
    for path in paths {
        let current = CheckpointFile::read_optional(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: format!("failed to snapshot pre-restore file metadata: {error}"),
        })?;
        if let (Some(current), Some(stored)) =
            (current.as_ref(), checkpoint.file_contents.get(path))
        {
            if current.matches(stored) {
                unchanged.push(path.clone());
            }
        }
        pre_restore_snapshot.insert(path.clone(), current);
    }

    let mut restored_paths: Vec<PathBuf> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for path in paths {
        let snapshot =
            checkpoint
                .file_contents
                .get(path)
                .ok_or_else(|| AftError::FileNotFound {
                    path: path.display().to_string(),
                })?;
        if let Err(e) = write_restored_file(path, snapshot, &mut created_dirs) {
            let mut rollback_errors = Vec::new();
            if let Some(snapshot) = pre_restore_snapshot.get(path) {
                if let Err(rollback_error) = restore_snapshot_file(path, snapshot.as_ref()) {
                    rollback_errors.push(format!("{}: {}", path.display(), rollback_error));
                }
            }
            for restored_path in restored_paths.iter().rev() {
                if let Some(snapshot) = pre_restore_snapshot.get(restored_path) {
                    if let Err(rollback_error) =
                        restore_snapshot_file(restored_path, snapshot.as_ref())
                    {
                        rollback_errors.push(format!(
                            "{}: {}",
                            restored_path.display(),
                            rollback_error
                        ));
                    }
                }
            }
            let dirs_rollback_ok = rollback_created_dirs(&created_dirs);
            if rollback_errors.is_empty() && dirs_rollback_ok {
                return Err(e);
            }
            return Err(AftError::IoError {
                path: path.display().to_string(),
                message: format!(
                    "{}; restore_checkpoint rollback_succeeded: {}; rollback_errors: {}",
                    e,
                    rollback_errors.is_empty() && dirs_rollback_ok,
                    if rollback_errors.is_empty() {
                        "none".to_string()
                    } else {
                        rollback_errors.join("; ")
                    }
                ),
            });
        }
        restored_paths.push(path.clone());
    }

    Ok(unchanged)
}

fn restore_snapshot_file(path: &Path, snapshot: Option<&CheckpointFile>) -> Result<(), AftError> {
    match snapshot {
        Some(snapshot) => write_restored_file(path, snapshot, &mut Vec::new()),
        None => remove_file_if_exists(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: format!("failed to remove file during checkpoint restore rollback: {error}"),
        }),
    }
}

fn write_restored_file(
    path: &Path,
    snapshot: &CheckpointFile,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<(), AftError> {
    create_parent_dirs(path, created_dirs)?;

    match &snapshot.kind {
        CheckpointFileKind::Regular { bytes } => {
            if path_is_symlink(path) {
                remove_file_if_exists(path).map_err(|error| AftError::IoError {
                    path: path.display().to_string(),
                    message: format!("failed to replace symlink with regular file: {error}"),
                })?;
            }
            fs::write(path, bytes).map_err(|error| AftError::IoError {
                path: path.display().to_string(),
                message: format!("failed to restore checkpoint file contents: {error}"),
            })?;
            restore_checkpoint_permissions(path, snapshot).map_err(|error| AftError::IoError {
                path: path.display().to_string(),
                message: format!("failed to restore checkpoint file permissions: {error}"),
            })
        }
        CheckpointFileKind::Symlink {
            target,
            target_is_dir,
        } => {
            remove_file_if_exists(path).map_err(|error| AftError::IoError {
                path: path.display().to_string(),
                message: format!("failed to replace file with checkpoint symlink: {error}"),
            })?;
            create_symlink(target, path, *target_is_dir).map_err(|error| AftError::IoError {
                path: path.display().to_string(),
                message: format!("failed to restore checkpoint symlink: {error}"),
            })
        }
    }
}

fn restore_checkpoint_permissions(path: &Path, snapshot: &CheckpointFile) -> io::Result<()> {
    if let Some(metadata) = &snapshot.metadata {
        return fs::set_permissions(path, metadata.permissions());
    }
    restore_checkpoint_mode(path, snapshot.mode)
}

#[cfg(unix)]
fn restore_checkpoint_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_checkpoint_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

fn create_parent_dirs(path: &Path, created_dirs: &mut Vec<PathBuf>) -> Result<(), AftError> {
    if let Some(parent) = path.parent() {
        let missing_dirs = missing_parent_dirs(parent);
        fs::create_dir_all(parent).map_err(|error| AftError::IoError {
            path: parent.display().to_string(),
            message: format!("failed to create checkpoint restore parent directories: {error}"),
        })?;
        created_dirs.extend(missing_dirs);
    }
    Ok(())
}

fn path_is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path, target_is_dir: bool) -> io::Result<()> {
    let _ = target_is_dir;
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path, target_is_dir: bool) -> io::Result<()> {
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path, _target_is_dir: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "checkpoint symlink restore is unsupported on this platform",
    ))
}

fn missing_parent_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(parent);

    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        dirs.push(dir.to_path_buf());
        current = dir.parent();
    }

    dirs
}

fn rollback_created_dirs(dirs: &[PathBuf]) -> bool {
    let mut dirs = dirs.to_vec();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    dirs.dedup();

    let mut ok = true;
    for dir in dirs {
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => ok = false,
        }
    }
    ok
}

/// Remove one project scope directory without ever deleting its contents.
/// Another process may acquire the lock or create a file between inspection and
/// removal, so every failure is intentionally ignored.
fn remove_empty_scope_dir(scope_dir: &Path) {
    let _ = fs::remove_dir(scope_dir);
}

/// Sweep only the direct children of the checkpoints root. Scope directories
/// contain lockfiles, not durable checkpoint data, so an empty one is safe to
/// remove while a non-empty one is left untouched by `remove_dir`.
fn sweep_empty_scope_dirs(checkpoints_root: &Path) {
    let entries = match fs::read_dir(checkpoints_root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            remove_empty_scope_dir(&entry.path());
        }
    }
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn current_timestamp_nanos() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DEFAULT_SESSION_ID;
    use std::fs;

    fn temp_file(name: &str, content: &str) -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("aft_checkpoint_tests_")
            .tempdir()
            .expect("create checkpoint temp dir");
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        (path, dir)
    }

    fn fresh_checkpoint_store(storage: &Path) -> CheckpointStore {
        let lock_path = storage
            .join("checkpoints")
            .join("test-project")
            .join("checkpoint.lock");
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        store.set_storage_dir_for_harness(storage.to_path_buf(), crate::harness::Harness::Opencode);
        store
    }

    fn checkpoint_store() -> (CheckpointStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (fresh_checkpoint_store(dir.path()), dir)
    }

    #[test]
    fn checkpoint_paths_follows_issuing_route_harness_after_rebind_and_restart() {
        use crate::backup::with_request_harness;
        use crate::harness::Harness;

        for (owner, sibling) in [
            (Harness::Pi, Harness::Opencode),
            (Harness::Opencode, Harness::Pi),
        ] {
            let storage = tempfile::tempdir().unwrap();
            let (file, _files) = temp_file("target.txt", "original");
            let session = "019de471-4fdc-762d-9286-624dfad0b5fe";
            let mut store = fresh_checkpoint_store(storage.path());
            store.set_storage_dir_for_harness(storage.path().to_path_buf(), owner.clone());
            let created = with_request_harness(&owner.wire_label(), || {
                store.create_for_files(session, "proof", vec![file.clone()])
            })
            .unwrap();
            assert!(created.storage_path.unwrap().join("meta.json").is_file());

            // Another harness binds the same root after creation. Configure
            // selects its namespace, but the existing owner's route is still live.
            store
                .namespace_request()
                .request(storage.path().to_path_buf(), sibling.clone());
            fs::write(&file, "modified").unwrap();
            let paths = with_request_harness(&owner.wire_label(), || {
                store.absolute_file_paths(session, "proof")
            })
            .expect("owner checkpoint_paths after sibling bind");
            assert_eq!(paths, vec![file.clone()]);
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                "modified",
                "preview must not restore"
            );
            with_request_harness(&owner.wire_label(), || store.restore(session, "proof")).unwrap();
            assert_eq!(fs::read_to_string(&file).unwrap(), "original");

            assert!(
                with_request_harness(&sibling.wire_label(), || {
                    store.absolute_file_paths(session, "proof")
                })
                .is_err(),
                "same session must not cross harness namespaces"
            );
            drop(store);

            // A fresh actor after restart has no in-memory checkpoints, and
            // may have been configured by the sibling harness first.
            let mut restarted = fresh_checkpoint_store(storage.path());
            restarted.set_storage_dir_for_harness(storage.path().to_path_buf(), sibling);
            fs::write(&file, "after restart").unwrap();
            let paths = with_request_harness(&owner.wire_label(), || {
                restarted.absolute_file_paths(session, "proof")
            })
            .expect("owner checkpoint_paths in fresh actor");
            assert_eq!(paths, vec![file.clone()]);
            with_request_harness(&owner.wire_label(), || {
                restarted.restore_validated(session, "proof", &paths)
            })
            .unwrap();
            assert_eq!(fs::read_to_string(&file).unwrap(), "original");
            assert!(
                with_request_harness(&owner.wire_label(), || {
                    restarted.absolute_file_paths("other-session", "proof")
                })
                .is_err(),
                "different sessions must remain isolated"
            );
        }
    }

    #[test]
    fn durability_checkpoint_create_list_restore_counts() {
        let (path, _files) = temp_file("durability.txt", "original");
        let (mut store, _storage) = checkpoint_store();
        crate::durability::take();
        store
            .create_for_files(DEFAULT_SESSION_ID, "snap", vec![path.clone()])
            .unwrap();
        let events = crate::durability::take();
        // Two data syncs, one directory commit and four first-use directories.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.0 == crate::durability::EventKind::DirectoryCreated)
                .count(),
            4
        );
        assert_eq!(
            crate::durability::sync_count(&events),
            if cfg!(unix) { 7 } else { 2 },
            "{events:?}"
        );
        // Replacing an existing checkpoint needs no new-directory flush.
        store
            .create_for_files(DEFAULT_SESSION_ID, "snap", vec![path.clone()])
            .unwrap();
        let events = crate::durability::take();
        assert_eq!(
            crate::durability::sync_count(&events),
            if cfg!(unix) { 3 } else { 2 },
            "{events:?}"
        );
        assert_eq!(store.list(DEFAULT_SESSION_ID).unwrap().len(), 1);
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);
        fs::write(&path, "edited").unwrap();
        store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn durability_checkpoint_parent_order() {
        use crate::durability::EventKind;
        let (path, _files) = temp_file("durability.txt", "original");
        let (mut store, _storage) = checkpoint_store();
        crate::durability::take();
        let info = store
            .create_for_files(DEFAULT_SESSION_ID, "snap", vec![path])
            .unwrap();
        let events = crate::durability::take();
        let dir = info.storage_path.unwrap();
        let created = events
            .iter()
            .position(|e| *e == (EventKind::DirectoryCreated, dir.clone()))
            .unwrap();
        let parent = events
            .iter()
            .position(|e| {
                *e == (
                    EventKind::DirectorySync,
                    dir.parent().unwrap().to_path_buf(),
                )
            })
            .unwrap();
        let meta = events
            .iter()
            .position(|e| *e == (EventKind::FileSync, dir.join("meta.json")))
            .unwrap();
        assert!(created < parent && parent < meta, "{events:?}");
    }

    #[test]
    fn durability_corrupt_checkpoint_does_not_poison_session() {
        let (path, _files) = temp_file("durability.txt", "original");
        let (mut store, storage) = checkpoint_store();
        let good = store
            .create_for_files(DEFAULT_SESSION_ID, "good", vec![path.clone()])
            .unwrap();
        let bad = store
            .create_for_files(DEFAULT_SESSION_ID, "bad", vec![path.clone()])
            .unwrap();
        for bytes in [&b"{"[..], &b""[..]] {
            fs::write(bad.storage_path.as_ref().unwrap().join("meta.json"), bytes).unwrap();
            let mut restarted = fresh_checkpoint_store(storage.path());
            crate::durability::take();
            let names: Vec<_> = restarted
                .list(DEFAULT_SESSION_ID)
                .unwrap()
                .into_iter()
                .map(|i| i.name)
                .collect();
            assert_eq!(names, vec!["good"]);
            assert!(crate::durability::take().iter().any(|e| *e
                == (
                    crate::durability::EventKind::CorruptCheckpointSkipped,
                    bad.storage_path.as_ref().unwrap().join("meta.json")
                )));
            fs::write(&path, "changed").unwrap();
            restarted.restore(DEFAULT_SESSION_ID, "good").unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
            restarted
                .create_for_files(DEFAULT_SESSION_ID, "another", vec![path.clone()])
                .unwrap();
            assert!(restarted.delete(DEFAULT_SESSION_ID, "another"));
            assert!(good
                .storage_path
                .as_ref()
                .unwrap()
                .join("meta.json")
                .is_file());
            assert_eq!(
                fs::read(bad.storage_path.as_ref().unwrap().join("meta.json")).unwrap(),
                bytes
            );
        }
    }

    fn checkpoint_file(content: &str) -> CheckpointFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), content).unwrap();
        CheckpointFile::read(file.path()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_operations_read_only_uncached_blobs() {
        let (mut store, _storage) = checkpoint_store();
        let files = tempfile::tempdir().unwrap();
        let paths = (0..8)
            .map(|i| files.path().join(format!("file{i}.bin")))
            .collect::<Vec<_>>();
        let bytes = vec![0xff; 256 * 1024];
        for path in &paths {
            fs::write(path, &bytes).unwrap();
        }
        for i in 0..20 {
            store
                .create_for_files("blob-work-count", &format!("cp{i:02}"), paths.clone())
                .unwrap();
        }
        // The newest checkpoint has not yet been hydrated. Prime it once.
        let before = store.list("blob-work-count").unwrap();
        store.blob_reads.store(0, Ordering::Relaxed);
        let after = store.list("blob-work-count").unwrap();
        store.file_paths("blob-work-count", "cp19").unwrap();
        assert!(store.delete("blob-work-count", "cp00"));
        for path in &paths {
            fs::write(path, b"edited").unwrap();
        }
        store.restore("blob-work-count", "cp19").unwrap();
        assert_eq!(
            store.blob_reads.load(Ordering::Relaxed),
            0,
            "warm checkpoint blob reads"
        );
        assert_eq!(
            before
                .iter()
                .map(|cp| (&cp.name, cp.file_count, cp.created_at))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|cp| (&cp.name, cp.file_count, cp.created_at))
                .collect::<Vec<_>>()
        );
        for path in paths {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn checkpoint_persistence_borrows_regular_file_bytes() {
        let (mut store, _storage) = checkpoint_store();
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("large.bin");
        let bytes = vec![0xff; 2 * 1024 * 1024];
        fs::write(&path, &bytes).unwrap();
        CHECKPOINT_COPIED_BYTES.with(|count| count.set(0));
        store
            .create_for_files("copy-work-count", "cp", vec![path.clone()])
            .unwrap();
        assert_eq!(
            CHECKPOINT_COPIED_BYTES.with(|count| count.get()),
            0,
            "regular blob serialization copied bytes"
        );
        fs::write(&path, b"edited").unwrap();
        store.restore("copy-work-count", "cp").unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn cached_checkpoint_reloads_blob_and_metadata_changes() {
        let (mut store, _storage) = checkpoint_store();
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("file.bin");
        fs::write(&path, b"old").unwrap();
        store
            .create_for_files("cache-change", "cp", vec![path.clone()])
            .unwrap();
        store.list("cache-change").unwrap();
        let dir = store.durable_checkpoint_dir("cache-change", "cp").unwrap();
        let meta_path = dir.join("meta.json");
        let mut meta: DiskCheckpointMeta =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        let blob = dir.join(&meta.files[0].blob);
        let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&blob).unwrap());
        fs::write(&blob, b"new").unwrap();
        filetime::set_file_mtime(&blob, mtime).unwrap();
        store.restore("cache-change", "cp").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        meta.created_at -= 1;
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
        assert_eq!(
            store.list("cache-change").unwrap()[0].created_at,
            meta.created_at
        );
        fs::remove_file(blob).unwrap();
        assert!(store.list("cache-change").is_err());
    }

    #[test]
    fn create_and_restore_round_trip() {
        let (path1, _dir1) = temp_file("cp_rt1.txt", "hello");
        let (path2, _dir2) = temp_file("cp_rt2.txt", "world");

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();

        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "snap1",
                vec![path1.clone(), path2.clone()],
                &backup_store,
            )
            .unwrap();
        assert_eq!(info.name, "snap1");
        assert_eq!(info.file_count, 2);

        // Modify files
        fs::write(&path1, "changed1").unwrap();
        fs::write(&path2, "changed2").unwrap();

        // Restore
        let info = store.restore(DEFAULT_SESSION_ID, "snap1").unwrap();
        assert_eq!(info.file_count, 2);
        assert_eq!(fs::read_to_string(&path1).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&path2).unwrap(), "world");
    }

    #[cfg(unix)]
    #[test]
    fn durable_checkpoint_hydrates_after_restart_with_bytes_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("durable-mode.bin");
        let original = b"draft decision\n\0byte exact\n";
        fs::write(&path, original).unwrap();
        let mut mode = fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o600);
        fs::set_permissions(&path, mode).unwrap();

        let backup_store = BackupStore::new();
        let (mut first, storage) = checkpoint_store();
        let info = first
            .create(
                DEFAULT_SESSION_ID,
                "restart-mode",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();
        let durable_path = info.storage_path.expect("durable checkpoint path");
        assert!(durable_path.join("meta.json").is_file());
        assert!(
            fs::read_dir(&durable_path)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "blob")),
            "checkpoint must persist one or more file blobs"
        );

        fs::write(&path, b"mutated\n").unwrap();
        let mut changed_mode = fs::metadata(&path).unwrap().permissions();
        changed_mode.set_mode(0o644);
        fs::set_permissions(&path, changed_mode).unwrap();
        drop(first);

        let mut restarted = fresh_checkpoint_store(storage.path());
        let listed = restarted.list(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(
            listed.len(),
            1,
            "fresh store must hydrate durable checkpoint"
        );
        assert_eq!(listed[0].name, "restart-mode");
        restarted
            .restore(DEFAULT_SESSION_ID, "restart-mode")
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_checkpoint_hydrates_symlink_without_following_target() {
        let files = tempfile::tempdir().unwrap();
        let target = files.path().join("target.txt");
        let link = files.path().join("link.txt");
        fs::write(&target, "target content").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let backup_store = BackupStore::new();
        let (mut first, storage) = checkpoint_store();
        first
            .create(
                DEFAULT_SESSION_ID,
                "restart-symlink",
                vec![link.clone()],
                &backup_store,
            )
            .unwrap();
        fs::remove_file(&link).unwrap();
        fs::write(&link, "plain replacement").unwrap();
        drop(first);

        let mut restarted = fresh_checkpoint_store(storage.path());
        restarted
            .restore(DEFAULT_SESSION_ID, "restart-symlink")
            .unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "target content");
    }

    #[test]
    fn checkpoint_retention_evicts_oldest_name_from_memory_and_disk() {
        let (path, _files) = temp_file("retention.txt", "version-0");
        let backup_store = BackupStore::new();
        let (mut store, storage) = checkpoint_store();

        for index in 0..=MAX_NAMED_CHECKPOINTS_PER_SESSION {
            fs::write(&path, format!("version-{index}")).unwrap();
            let info = store
                .create(
                    DEFAULT_SESSION_ID,
                    &format!("checkpoint-{index:02}"),
                    vec![path.clone()],
                    &backup_store,
                )
                .unwrap();
            if index == MAX_NAMED_CHECKPOINTS_PER_SESSION {
                assert_eq!(info.evicted, vec!["checkpoint-00"]);
            } else {
                assert!(info.evicted.is_empty());
            }
        }

        let listed = store.list(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(listed.len(), MAX_NAMED_CHECKPOINTS_PER_SESSION);
        assert!(listed.iter().all(|info| info.name != "checkpoint-00"));
        let old_dir = storage
            .path()
            .join("opencode")
            .join("checkpoints")
            .join(hash_session(DEFAULT_SESSION_ID))
            .join("checkpoint-00");
        assert!(!old_dir.exists(), "evicted checkpoint must leave disk too");
    }

    #[test]
    fn hydration_finishes_retention_after_interrupted_create() {
        let (path, _files) = temp_file("interrupted-retention.txt", "checkpoint content");
        let backup_store = BackupStore::new();
        let (mut first, storage) = checkpoint_store();

        for index in 0..MAX_NAMED_CHECKPOINTS_PER_SESSION {
            first
                .create(
                    DEFAULT_SESSION_ID,
                    &format!("checkpoint-{index:02}"),
                    vec![path.clone()],
                    &backup_store,
                )
                .unwrap();
        }

        // The create path persists the new checkpoint before evicting older
        // checkpoints. Write only the durable checkpoint here to simulate the
        // process exiting after persistence but before retention eviction.
        let checkpoint = Checkpoint {
            name: "checkpoint-20".to_string(),
            file_contents: HashMap::from([(path, checkpoint_file("newest"))]),
            created_at: current_timestamp(),
            created_order: u64::MAX,
        };
        {
            let _lock = first.acquire_mutation_lock().unwrap();
            first
                .persist_checkpoint_locked(DEFAULT_SESSION_ID, &checkpoint)
                .unwrap();
        }
        drop(first);

        let session_dir = storage
            .path()
            .join("opencode")
            .join("checkpoints")
            .join(hash_session(DEFAULT_SESSION_ID));
        assert_eq!(fs::read_dir(&session_dir).unwrap().count(), 21);

        let mut restarted = fresh_checkpoint_store(storage.path());
        let listed = restarted.list(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(listed.len(), MAX_NAMED_CHECKPOINTS_PER_SESSION);
        assert!(listed.iter().all(|info| info.name != "checkpoint-00"));
        assert!(
            !session_dir.join("checkpoint-00").exists(),
            "hydration must finish interrupted retention on disk"
        );
    }

    #[test]
    fn cleanup_refuses_to_sweep_scope_dirs_outside_a_checkpoints_root() {
        // Regression: edit_match tests set lock_path = <TempDir>/checkpoint.lock, so
        // lock_path.parent().parent() is the OS TEMP ROOT. Before the named-root guard,
        // cleanup_locked swept empty sibling directories there and deleted other tests'
        // freshly created TempDirs (observed live: read.rs fixture writes failing with
        // NotFound under the parallel suite). Mutation control: drop the file_name()
        // guard in cleanup_locked and this test fails.
        let temp_root = tempfile::tempdir().expect("temp root");
        // lock_path.parent().parent() == temp_root, which is NOT named `checkpoints`,
        // so the guard must refuse the sweep and the empty sibling must survive.
        let victim = temp_root.path().join("innocent-empty-sibling");
        fs::create_dir(&victim).expect("victim dir");
        let scope_dir = temp_root.path().join("scope");
        fs::create_dir(&scope_dir).expect("scope dir");
        let lock_path = scope_dir.join("checkpoint.lock");
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        store.cleanup_locked().expect("cleanup");
        assert!(
            victim.exists(),
            "cleanup must not sweep empty dirs outside a `checkpoints` root"
        );
    }

    #[test]
    fn cleanup_sweeps_durable_checkpoints_older_than_fourteen_days() {
        let (path, _files) = temp_file("durable-gc.txt", "original");
        let backup_store = BackupStore::new();
        let (mut store, _storage) = checkpoint_store();
        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "expired-durable",
                vec![path],
                &backup_store,
            )
            .unwrap();
        let durable_path = info.storage_path.unwrap();
        let meta_path = durable_path.join("meta.json");
        let mut meta: DiskCheckpointMeta =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta.created_at = current_timestamp()
            .saturating_sub(NAMED_CHECKPOINT_RETENTION_SECS)
            .saturating_sub(1);
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();

        store.cleanup();
        assert!(
            !durable_path.exists(),
            "fourteen-day cleanup must remove the durable checkpoint directory"
        );
    }

    #[test]
    fn durable_hydration_fails_when_a_referenced_blob_is_missing() {
        let (path, _files) = temp_file("hydration-control.txt", "original");
        let backup_store = BackupStore::new();
        let (mut first, storage) = checkpoint_store();
        let info = first
            .create(
                DEFAULT_SESSION_ID,
                "hydration-control",
                vec![path],
                &backup_store,
            )
            .unwrap();
        let durable_path = info.storage_path.unwrap();
        let meta: DiskCheckpointMeta =
            serde_json::from_slice(&fs::read(durable_path.join("meta.json")).unwrap()).unwrap();
        fs::remove_file(durable_path.join(&meta.files[0].blob)).unwrap();
        drop(first);

        let mut restarted = fresh_checkpoint_store(storage.path());
        let error = restarted.list(DEFAULT_SESSION_ID).unwrap_err();
        match error {
            AftError::IoError { message, .. } => {
                assert!(message.contains("failed to read durable checkpoint blob"));
            }
            other => panic!("expected durable hydration I/O error, got {other:?}"),
        }
    }

    #[test]
    fn binding_the_harness_keeps_the_newer_of_two_same_name_checkpoints() {
        let storage = tempfile::tempdir().unwrap();
        let (older_file, _older_dir) = temp_file("older.txt", "older");
        let (newer_file, _newer_dir) = temp_file("newer.txt", "newer");
        let backup_store = BackupStore::new();

        // `<storage>/opencode/checkpoints` already holds `snap` (older.txt).
        let mut harness_store = fresh_checkpoint_store(storage.path());
        harness_store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![older_file.clone()],
                &backup_store,
            )
            .unwrap();
        drop(harness_store);

        // A store that has not been configured yet saves a newer `snap`
        // (newer.txt) under `<storage>/unbound/checkpoints`.
        let mut store = CheckpointStore::unbound_in_for_test(storage.path());
        store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![newer_file.clone()],
                &backup_store,
            )
            .unwrap();
        store.set_storage_dir_for_harness(
            storage.path().to_path_buf(),
            crate::harness::Harness::Opencode,
        );

        fs::write(&older_file, "older changed").unwrap();
        fs::write(&newer_file, "newer changed").unwrap();
        let info = store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(info.file_count, 1);
        assert_eq!(fs::read_to_string(&newer_file).unwrap(), "newer");
        assert_eq!(fs::read_to_string(&older_file).unwrap(), "older changed");
    }

    #[test]
    fn binding_the_harness_keeps_a_newer_harness_checkpoint_over_an_older_unbound_one() {
        let storage = tempfile::tempdir().unwrap();
        let (older_file, _older_dir) = temp_file("older.txt", "older");
        let (newer_file, _newer_dir) = temp_file("newer.txt", "newer");
        let backup_store = BackupStore::new();

        let mut store = CheckpointStore::unbound_in_for_test(storage.path());
        store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![older_file.clone()],
                &backup_store,
            )
            .unwrap();
        let mut harness_store = fresh_checkpoint_store(storage.path());
        harness_store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![newer_file.clone()],
                &backup_store,
            )
            .unwrap();
        drop(harness_store);
        store.set_storage_dir_for_harness(
            storage.path().to_path_buf(),
            crate::harness::Harness::Opencode,
        );

        fs::write(&older_file, "older changed").unwrap();
        fs::write(&newer_file, "newer changed").unwrap();
        store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(fs::read_to_string(&newer_file).unwrap(), "newer");
        assert_eq!(fs::read_to_string(&older_file).unwrap(), "older changed");
    }

    #[test]
    fn restore_reports_files_that_already_matched_the_checkpoint() {
        let (edited, _edited_dir) = temp_file("edited.txt", "original");
        let (untouched, _untouched_dir) = temp_file("untouched.txt", "same");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![edited.clone(), untouched.clone()],
                &backup_store,
            )
            .unwrap();

        fs::write(&edited, "changed").unwrap();
        let info = store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        let mut expected_paths = vec![edited.clone(), untouched.clone()];
        expected_paths.sort();
        assert_eq!(info.paths, expected_paths);
        assert_eq!(info.unchanged, vec![untouched.clone()]);
        assert_eq!(fs::read_to_string(&edited).unwrap(), "original");

        let again = store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(again.unchanged, expected_paths);
    }

    fn checkpoint_dir_with_meta(parent: &Path, name: &str, marker: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("meta.json"), marker).unwrap();
        dir
    }

    #[test]
    fn replacing_a_checkpoint_dir_keeps_the_original_when_the_move_into_place_fails() {
        let root = tempfile::tempdir().unwrap();
        let source = checkpoint_dir_with_meta(&root.path().join("unbound"), "snap", "newer");
        let target = checkpoint_dir_with_meta(&root.path().join("harness"), "snap", "older");

        let result = replace_checkpoint_dir_with(&source, &target, |from, to| {
            if from == source.as_path() {
                Err(io::Error::other("injected failure moving the newer copy"))
            } else {
                fs::rename(from, to)
            }
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(target.join("meta.json")).unwrap(),
            "older"
        );
        assert!(!replaced_checkpoint_path(&target).exists());
        assert_eq!(
            fs::read_to_string(source.join("meta.json")).unwrap(),
            "newer"
        );
    }

    #[test]
    fn replacing_a_checkpoint_dir_swaps_in_the_new_copy_and_drops_the_old_one() {
        let root = tempfile::tempdir().unwrap();
        let source = checkpoint_dir_with_meta(&root.path().join("unbound"), "snap", "newer");
        let target = checkpoint_dir_with_meta(&root.path().join("harness"), "snap", "older");

        replace_checkpoint_dir(&source, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("meta.json")).unwrap(),
            "newer"
        );
        assert!(!replaced_checkpoint_path(&target).exists());
        assert!(!source.exists());
    }

    #[test]
    fn hydration_puts_back_a_checkpoint_a_crash_left_moved_aside() {
        let (file, _file_dir) = temp_file("aside.txt", "original");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![file.clone()],
                &backup_store,
            )
            .unwrap();
        // A crash between moving the old copy aside and moving the new copy
        // into place leaves only `snap.aft-replaced`.
        let checkpoint_dir = info.storage_path.unwrap();
        fs::rename(&checkpoint_dir, replaced_checkpoint_path(&checkpoint_dir)).unwrap();

        fs::write(&file, "changed").unwrap();
        store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert!(!replaced_checkpoint_path(&checkpoint_dir).exists());
        let names = store
            .list(DEFAULT_SESSION_ID)
            .unwrap()
            .into_iter()
            .map(|info| info.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["snap".to_string()]);
    }

    #[test]
    fn checkpoint_names_with_the_reserved_suffix_are_refused() {
        let (file, _file_dir) = temp_file("reserved.txt", "x");
        let (mut store, _store_dir) = checkpoint_store();
        let error = store
            .create(
                DEFAULT_SESSION_ID,
                "snap.aft-replaced",
                vec![file],
                &BackupStore::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    #[test]
    fn overwrite_existing_name() {
        let (path, _dir) = temp_file("cp_overwrite.txt", "v1");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();

        store
            .create(DEFAULT_SESSION_ID, "dup", vec![path.clone()], &backup_store)
            .unwrap();
        fs::write(&path, "v2").unwrap();
        store
            .create(DEFAULT_SESSION_ID, "dup", vec![path.clone()], &backup_store)
            .unwrap();

        // Restore should give v2 (the overwritten checkpoint)
        fs::write(&path, "v3").unwrap();
        store.restore(DEFAULT_SESSION_ID, "dup").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn list_returns_metadata_scoped_to_session() {
        let (path, _dir) = temp_file("cp_list.txt", "data");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();

        store
            .create(DEFAULT_SESSION_ID, "a", vec![path.clone()], &backup_store)
            .unwrap();
        store
            .create(DEFAULT_SESSION_ID, "b", vec![path.clone()], &backup_store)
            .unwrap();
        store
            .create("other_session", "c", vec![path.clone()], &backup_store)
            .unwrap();

        let default_list = store.list(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(default_list.len(), 2);
        let names: Vec<&str> = default_list.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));

        let other_list = store.list("other_session").unwrap();
        assert_eq!(other_list.len(), 1);
        assert_eq!(other_list[0].name, "c");
    }

    #[test]
    fn sessions_isolate_checkpoint_names() {
        // Same checkpoint name in two sessions does not collide on restore.
        let (path_a, _dir_a) = temp_file("cp_isolated_a.txt", "a-original");
        let (path_b, _dir_b) = temp_file("cp_isolated_b.txt", "b-original");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();

        // Both sessions create a checkpoint with the same name but different files.
        store
            .create("session_a", "snap", vec![path_a.clone()], &backup_store)
            .unwrap();
        store
            .create("session_b", "snap", vec![path_b.clone()], &backup_store)
            .unwrap();

        fs::write(&path_a, "a-modified").unwrap();
        fs::write(&path_b, "b-modified").unwrap();

        // Restoring session A's "snap" only touches path_a.
        store.restore("session_a", "snap").unwrap();
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-original");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-modified");

        // Restoring session B's "snap" only touches path_b.
        fs::write(&path_a, "a-modified").unwrap();
        store.restore("session_b", "snap").unwrap();
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-modified");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-original");
    }

    #[test]
    fn checkpoint_lock_scope_stays_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let scope_dir = dir.path().join("checkpoints").join("project-scope");
        let lock_path = scope_dir.join("checkpoint.lock");
        let path = dir.path().join("checkpoint.txt");
        fs::write(&path, "data").unwrap();
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::with_lock_path(lock_path.clone(), CHECKPOINT_LOCK_TIMEOUT);

        store
            .create(DEFAULT_SESSION_ID, "released", vec![path], &backup_store)
            .unwrap();

        // Blocked acquirers poll for the lock file inside this directory, so a
        // release must not remove it; empty scopes are reaped by cleanup.
        assert!(scope_dir.is_dir(), "released lock scope must stay");
        assert!(!lock_path.exists(), "the lock file itself is released");
    }

    /// Reproduces issue #342: a holder releases while several waiters are
    /// blocked on the same scope. When the release removed the scope
    /// directory, one waiter's lock-file creation failed with NotFound
    /// (20 of 20 trials failed with five waiters). Every waiter must acquire.
    #[test]
    fn every_waiter_acquires_after_a_contended_release() {
        for _ in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir
                .path()
                .join("checkpoints")
                .join("scope")
                .join("checkpoint.lock");
            let holder =
                CheckpointStore::with_lock_path(lock_path.clone(), CHECKPOINT_LOCK_TIMEOUT);
            let held = holder.acquire_mutation_lock().unwrap();
            let waiters: Vec<_> = (0..5)
                .map(|_| {
                    let path = lock_path.clone();
                    std::thread::spawn(move || {
                        let store = CheckpointStore::with_lock_path(path, CHECKPOINT_LOCK_TIMEOUT);
                        store.acquire_mutation_lock().map(|guard| {
                            std::thread::sleep(Duration::from_millis(50));
                            drop(guard);
                        })
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
            for waiter in waiters {
                if let Err(error) = waiter.join().unwrap() {
                    panic!("a waiter lost the contended release: {error:?}");
                }
            }
        }
    }

    #[test]
    fn cleanup_removes_expired_across_sessions() {
        let (path, _dir) = temp_file("cp_cleanup.txt", "data");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();

        store
            .create(
                DEFAULT_SESSION_ID,
                "recent",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();

        // Manually insert an expired checkpoint in another session.
        store
            .checkpoints
            .entry("other".to_string())
            .or_default()
            .insert(
                "old".to_string(),
                Checkpoint {
                    name: "old".to_string(),
                    file_contents: HashMap::new(),
                    created_at: 1000, // far in the past
                    created_order: 1000,
                },
            );

        assert_eq!(store.total_count(), 2);
        store.cleanup();
        assert_eq!(store.total_count(), 1);
        assert_eq!(store.list(DEFAULT_SESSION_ID).unwrap()[0].name, "recent");
        assert!(store.list("other").unwrap().is_empty());
    }

    #[test]
    fn cleanup_sweeps_empty_scope_dirs_but_keeps_live_lock_scope() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoints_root = dir.path().join("checkpoints");
        let empty_a = checkpoints_root.join("empty-a");
        let empty_b = checkpoints_root.join("empty-b");
        let live_scope = checkpoints_root.join("live-scope");
        fs::create_dir_all(&empty_a).unwrap();
        fs::create_dir_all(&empty_b).unwrap();
        fs::create_dir_all(&live_scope).unwrap();
        fs::write(live_scope.join("checkpoint.lock"), "live lock").unwrap();

        let lock_path = checkpoints_root
            .join("current-scope")
            .join("checkpoint.lock");
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        store.cleanup();

        assert!(!empty_a.exists());
        assert!(!empty_b.exists());
        assert!(live_scope.is_dir());
        assert!(live_scope.join("checkpoint.lock").is_file());
    }

    #[test]
    fn cleanup_ignores_non_empty_scope_dir_removal_failure() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoints_root = dir.path().join("checkpoints");
        let scope_dir = checkpoints_root.join("racing-scope");
        fs::create_dir_all(&scope_dir).unwrap();
        // Model the post-race state where a concurrent lock acquisition adds
        // this file after the root readdir but before remove_dir.
        fs::write(scope_dir.join("checkpoint.lock"), "lock appeared").unwrap();

        let lock_path = checkpoints_root
            .join("current-scope")
            .join("checkpoint.lock");
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        store.cleanup();

        assert!(scope_dir.is_dir());
        assert!(scope_dir.join("checkpoint.lock").is_file());
    }

    #[test]
    fn restore_nonexistent_returns_error() {
        let (mut store, _store_dir) = checkpoint_store();
        let result = store.restore(DEFAULT_SESSION_ID, "nope");
        assert!(result.is_err());
        match result.unwrap_err() {
            AftError::CheckpointNotFound { name } => {
                assert_eq!(name, "nope");
            }
            other => panic!("expected CheckpointNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn restore_nonexistent_in_other_session_returns_error() {
        // A "snap" that exists in session A must NOT be visible from session B.
        let (path, _dir) = temp_file("cp_cross_session.txt", "data");
        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create("session_a", "only_a", vec![path], &backup_store)
            .unwrap();
        assert!(store.restore("session_b", "only_a").is_err());
    }

    #[test]
    fn create_skips_missing_files_from_backup_tracked_set() {
        // Simulate the reported issue #15-follow-up: an agent deletes a
        // previously-edited file, then calls checkpoint with no explicit
        // file list. Before the fix, the stale backup-tracked entry caused
        // the whole checkpoint to fail on the missing path. Now the checkpoint
        // succeeds with the readable file and reports the skipped one.
        let (readable, _readable_dir) = temp_file("cp_skip_readable.txt", "still_here");
        let (deleted, _deleted_dir) = temp_file("cp_skip_deleted.txt", "about_to_vanish");

        // Backup store canonicalizes keys, so the skipped path in the
        // checkpoint result is the canonical form, not the raw temp path.
        let deleted_canonical = fs::canonicalize(&deleted).unwrap();

        let mut backup_store = BackupStore::new();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &readable, "auto")
            .unwrap();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &deleted, "auto")
            .unwrap();

        fs::remove_file(&deleted).unwrap();

        let (mut store, _store_dir) = checkpoint_store();
        let info = store
            .create(DEFAULT_SESSION_ID, "partial", vec![], &backup_store)
            .expect("checkpoint should succeed despite one missing file");
        assert_eq!(info.file_count, 1);
        assert_eq!(info.skipped.len(), 1);
        assert_eq!(info.skipped[0].0, deleted_canonical);
        assert!(!info.skipped[0].1.is_empty());
    }

    #[test]
    fn create_with_explicit_single_missing_file_errors() {
        // When the caller names a single file explicitly and it can't be read,
        // fail loudly — an empty checkpoint isn't what the caller asked for.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("cp_explicit_missing_does_not_exist.txt");

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        let result = store.create(
            DEFAULT_SESSION_ID,
            "explicit",
            vec![missing.clone()],
            &backup_store,
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            AftError::FileNotFound { path } => {
                assert!(path.contains(&missing.display().to_string()));
            }
            other => panic!("expected FileNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn create_with_explicit_mixed_files_keeps_readable_and_reports_skipped() {
        // Explicit file list with one readable + one missing: keep the
        // readable one in the checkpoint, report the missing one under
        // `skipped` instead of failing outright.
        let (good, _good_dir) = temp_file("cp_mixed_good.txt", "ok");
        let missing_dir = tempfile::tempdir().unwrap();
        let missing = missing_dir.path().join("cp_mixed_missing.txt");

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "mixed",
                vec![good.clone(), missing.clone()],
                &backup_store,
            )
            .expect("mixed checkpoint should succeed when any file is readable");
        assert_eq!(info.file_count, 1);
        assert_eq!(info.skipped.len(), 1);
        assert_eq!(info.skipped[0].0, missing);
    }

    #[test]
    fn create_with_empty_files_uses_backup_tracked() {
        let (path, _dir) = temp_file("cp_tracked.txt", "tracked_content");
        let mut backup_store = BackupStore::new();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &path, "auto")
            .unwrap();

        let (mut store, _store_dir) = checkpoint_store();
        let info = store
            .create(DEFAULT_SESSION_ID, "from_tracked", vec![], &backup_store)
            .unwrap();
        assert!(info.file_count >= 1);

        // Modify and restore
        fs::write(&path, "modified").unwrap();
        store.restore(DEFAULT_SESSION_ID, "from_tracked").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "tracked_content");
    }

    #[test]
    fn restore_recreates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("file.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "original nested content").unwrap();

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create(
                DEFAULT_SESSION_ID,
                "nested",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();

        fs::remove_dir_all(dir.path().join("nested")).unwrap();

        store.restore(DEFAULT_SESSION_ID, "nested").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "original nested content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_restore_rolls_back_on_partial_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        fs::write(&path_a, "checkpoint-a").unwrap();
        fs::write(&path_b, "checkpoint-b").unwrap();

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create(
                DEFAULT_SESSION_ID,
                "partial_failure",
                vec![path_a.clone(), path_b.clone()],
                &backup_store,
            )
            .unwrap();

        fs::write(&path_a, "pre-restore-a").unwrap();
        fs::write(&path_b, "pre-restore-b").unwrap();
        let mut readonly = fs::metadata(&path_b).unwrap().permissions();
        readonly.set_mode(0o444);
        fs::set_permissions(&path_b, readonly).unwrap();

        let result = store.restore(DEFAULT_SESSION_ID, "partial_failure");
        let mut writable = fs::metadata(&path_b).unwrap().permissions();
        writable.set_mode(0o644);
        fs::set_permissions(&path_b, writable).unwrap();

        assert!(result.is_err(), "restore should surface write failure");
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "pre-restore-a");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "pre-restore-b");
    }

    #[test]
    fn checkpoint_create_and_restore_use_mutation_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("locks").join("checkpoint.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let mut store =
            CheckpointStore::with_lock_path(lock_path.clone(), Duration::from_millis(50));
        let backup_store = BackupStore::new();
        let path = dir.path().join("locked.txt");
        fs::write(&path, "original").unwrap();

        let held_lock =
            fs_lock::try_acquire(&lock_path, Duration::from_secs(1)).expect("hold checkpoint lock");
        let create_result = store.create(
            DEFAULT_SESSION_ID,
            "locked",
            vec![path.clone()],
            &backup_store,
        );
        assert!(matches!(create_result, Err(AftError::IoError { .. })));
        drop(held_lock);

        store
            .create(
                DEFAULT_SESSION_ID,
                "locked",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();
        fs::write(&path, "changed").unwrap();

        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let held_lock =
            fs_lock::try_acquire(&lock_path, Duration::from_secs(1)).expect("hold checkpoint lock");
        let restore_result = store.restore(DEFAULT_SESSION_ID, "locked");
        assert!(matches!(restore_result, Err(AftError::IoError { .. })));
        drop(held_lock);

        store.restore(DEFAULT_SESSION_ID, "locked").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_restore_preserves_regular_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mode.txt");
        fs::write(&path, "original").unwrap();
        let mut original_permissions = fs::metadata(&path).unwrap().permissions();
        original_permissions.set_mode(0o600);
        fs::set_permissions(&path, original_permissions).unwrap();

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create(
                DEFAULT_SESSION_ID,
                "mode",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();

        fs::write(&path, "changed").unwrap();
        let mut changed_permissions = fs::metadata(&path).unwrap().permissions();
        changed_permissions.set_mode(0o644);
        fs::set_permissions(&path, changed_permissions).unwrap();

        store.restore(DEFAULT_SESSION_ID, "mode").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        let restored_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(restored_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_restore_recreates_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "target content").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let backup_store = BackupStore::new();
        let (mut store, _store_dir) = checkpoint_store();
        store
            .create(
                DEFAULT_SESSION_ID,
                "symlink",
                vec![link.clone()],
                &backup_store,
            )
            .unwrap();

        fs::remove_file(&link).unwrap();
        fs::write(&link, "plain file").unwrap();

        store.restore(DEFAULT_SESSION_ID, "symlink").unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read_to_string(&link).unwrap(), "target content");
    }

    #[test]
    fn captured_regular_file_is_shared_by_checkpoint_and_backup() {
        let (path, _dir) = temp_file("shared-capture.txt", "original bytes");
        crate::backup::reset_capture_read_count(&path);
        let mut capture = CapturedRegularFile::read(&path).unwrap().unwrap();
        assert_eq!(crate::backup::capture_read_count(&path), 1);

        let checkpoint = CheckpointFile::from_captured(&path, &mut capture).unwrap();
        let mut backup = BackupStore::new();
        backup
            .snapshot_with_op_from_capture(
                DEFAULT_SESSION_ID,
                &path,
                "shared capture",
                Some("shared-op"),
                &capture,
            )
            .unwrap();

        let history = backup.history(DEFAULT_SESSION_ID, &path);
        let CheckpointFileKind::Regular { bytes } = checkpoint.kind else {
            panic!("regular capture must create a regular checkpoint");
        };
        assert_eq!(bytes.as_ref(), b"original bytes");
        assert_eq!(history[0].content_bytes.as_ref(), b"original bytes");
        assert!(Arc::ptr_eq(&bytes, &history[0].content_bytes));
        assert_eq!(crate::backup::capture_read_count(&path), 1);
    }

    #[test]
    fn stale_capture_refreshes_before_checkpoint_and_backup() {
        let (path, _dir) = temp_file("stale-capture.txt", "old");
        crate::backup::reset_capture_read_count(&path);
        let mut capture = CapturedRegularFile::read(&path).unwrap().unwrap();
        fs::write(&path, "fresh disk truth").unwrap();

        let checkpoint = CheckpointFile::from_captured(&path, &mut capture).unwrap();
        let mut backup = BackupStore::new();
        backup
            .snapshot_with_op_from_capture(
                DEFAULT_SESSION_ID,
                &path,
                "freshened capture",
                Some("fresh-op"),
                &capture,
            )
            .unwrap();

        let history = backup.history(DEFAULT_SESSION_ID, &path);
        let CheckpointFileKind::Regular { bytes } = checkpoint.kind else {
            panic!("regular capture must create a regular checkpoint");
        };
        assert_eq!(bytes.as_ref(), b"fresh disk truth");
        assert_eq!(history[0].content_bytes.as_ref(), b"fresh disk truth");
        assert!(Arc::ptr_eq(&bytes, &history[0].content_bytes));
        assert_eq!(crate::backup::capture_read_count(&path), 2);
    }

    #[test]
    fn checkpoint_restore_failure_removes_created_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("created");
        let path_a = missing_root.join("nested").join("a.txt");
        let path_b = dir.path().join("blocking-dir");
        fs::create_dir(&path_b).unwrap();

        let checkpoint = Checkpoint {
            name: "dir-cleanup".to_string(),
            file_contents: HashMap::from([
                (path_a.clone(), checkpoint_file("checkpoint-a")),
                (path_b.clone(), checkpoint_file("checkpoint-b")),
            ]),
            created_at: current_timestamp(),
            created_order: current_timestamp_nanos(),
        };

        let result = restore_paths_atomically(&checkpoint, &[path_a.clone(), path_b.clone()]);

        assert!(
            result.is_err(),
            "second restore write should fail on directory"
        );
        assert!(!path_a.exists(), "restored file should be rolled back");
        assert!(
            !missing_root.exists(),
            "new parent directories should be removed on rollback"
        );
        assert!(path_b.is_dir(), "pre-existing blocking directory remains");
    }

    #[cfg(unix)]
    #[test]
    fn durable_checkpoints_are_owner_only_and_old_history_is_shielded_without_a_walk() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        // A world-readable storage directory: checkpoints must not rely on it.
        let storage = temp.path().join("storage");
        fs::create_dir(&storage).unwrap();
        fs::set_permissions(&storage, fs::Permissions::from_mode(0o755)).unwrap();
        let checkpoints_dir = storage.join("opencode").join("checkpoints");

        // A checkpoint left by an older version: 0755 directories, 0644 files.
        let old_dir = checkpoints_dir.join("old-session").join("old-checkpoint");
        fs::create_dir_all(&old_dir).unwrap();
        let old_blob = old_dir.join("file_1_0_0.blob");
        fs::write(&old_blob, "old secret").unwrap();
        fs::set_permissions(&old_blob, fs::Permissions::from_mode(0o644)).unwrap();
        for dir in [
            checkpoints_dir.as_path(),
            old_dir.parent().unwrap(),
            old_dir.as_path(),
        ] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let source = temp.path().join("auth.json");
        fs::write(&source, "secret-token").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();

        let backup_store = BackupStore::new();
        let mut store = fresh_checkpoint_store(&storage);
        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "snap",
                vec![source.clone()],
                &backup_store,
            )
            .unwrap();
        let durable = info.storage_path.expect("durable checkpoint path");

        let mode_of =
            |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        let mut new_files = 0;
        for entry in fs::read_dir(&durable).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(
                mode_of(&path),
                0o600,
                "{} is not owner-only",
                path.display()
            );
            new_files += 1;
        }
        assert!(new_files >= 2, "expected a blob and meta.json");
        assert_eq!(mode_of(&durable), 0o700);
        assert_eq!(mode_of(durable.parent().unwrap()), 0o700);
        // The lock scope directory is also created by the store.
        assert_eq!(
            mode_of(&storage.join("checkpoints").join("test-project")),
            0o700
        );

        // The boundary shields old history without visiting every blob.
        assert_eq!(mode_of(&old_blob), 0o644);
        assert_eq!(mode_of(&old_dir), 0o755);
        assert_eq!(mode_of(&checkpoints_dir), 0o700);

        // Restore still gives the source its own recorded mode back.
        fs::write(&source, "changed").unwrap();
        store.restore(DEFAULT_SESSION_ID, "snap").unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "secret-token");
        assert_eq!(mode_of(&source), 0o644);
    }
}
