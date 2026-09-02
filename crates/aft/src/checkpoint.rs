use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backup::{hash_session, BackupStore, CapturedRegularFile};
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
    lock_path: PathBuf,
    lock_timeout: Duration,
    storage_dir: Option<PathBuf>,
    storage_harness: Option<String>,
    blob_counter: AtomicU64,
}

/// Owns a checkpoint mutation lock.
///
/// The lock scope directory is durable. Removing it after each owner releases
/// the lock races another process between its `create_dir_all` and exclusive
/// lock-file creation.
struct CheckpointLockGuard {
    guard: Option<fs_lock::LockGuard>,
}

impl Drop for CheckpointLockGuard {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
    }
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

    /// Point this store's mutation lock at a private path. Tests use this for
    /// isolation instead of mutating the process-global `AFT_CACHE_DIR` env
    /// var, which races parallel lib tests that resolve storage paths.
    #[cfg(test)]
    pub(crate) fn set_lock_path_for_test(&mut self, lock_path: PathBuf) {
        self.storage_dir = lock_path.parent().map(Path::to_path_buf);
        self.storage_harness = Some("test".to_string());
        self.lock_path = lock_path;
    }

    /// Select the harness-scoped durable namespace. Rebinding a different
    /// namespace drops only the derived in-memory cache; disk remains authoritative.
    pub fn set_storage_dir_for_harness(&mut self, dir: PathBuf, harness: crate::harness::Harness) {
        let harness = harness.storage_segment();
        if self.storage_dir.as_ref() == Some(&dir)
            && self.storage_harness.as_deref() == Some(&harness)
        {
            return;
        }
        if self.storage_dir.as_ref() == Some(&dir)
            && self.storage_harness.as_deref() == Some(UNBOUND_HARNESS_SEGMENT)
        {
            match self.acquire_mutation_lock() {
                Ok(_lock) => migrate_unbound_checkpoint_namespace(&dir, &harness),
                Err(error) => crate::slog_warn!(
                    "could not migrate unbound durable checkpoints into {}: {}",
                    harness,
                    error
                ),
            }
        }
        self.storage_dir = Some(dir);
        self.storage_harness = Some(harness);
        self.checkpoints.clear();
    }

    fn with_lock_path(lock_path: PathBuf, lock_timeout: Duration) -> Self {
        CheckpointStore {
            checkpoints: HashMap::new(),
            lock_path,
            lock_timeout,
            storage_dir: None,
            storage_harness: None,
            blob_counter: AtomicU64::new(0),
        }
    }

    fn acquire_mutation_lock(&self) -> Result<CheckpointLockGuard, AftError> {
        let scope_dir = self.lock_path.parent().map(Path::to_path_buf);
        if let Some(parent) = scope_dir.as_deref() {
            fs::create_dir_all(parent).map_err(|error| AftError::IoError {
                path: parent.display().to_string(),
                message: format!("failed to create checkpoint lock directory: {error}"),
            })?;
        }

        let acquire_result = match fs_lock::try_acquire(&self.lock_path, self.lock_timeout) {
            // A releasing peer removes the empty lock scope after its heartbeat
            // exits. It can win the tiny interval after our create_dir_all and
            // before lock creation, so recreate once and retry the acquisition.
            Err(fs_lock::AcquireError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = scope_dir.as_deref() {
                    fs::create_dir_all(parent).map_err(|error| AftError::IoError {
                        path: parent.display().to_string(),
                        message: format!("failed to recreate checkpoint lock directory: {error}"),
                    })?;
                }
                fs_lock::try_acquire(&self.lock_path, self.lock_timeout)
            }
            result => result,
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

        Ok(CheckpointLockGuard { guard: Some(guard) })
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
        self.create_impl(session, name, files, backup_store, None)
    }

    pub(crate) fn create_from_captures(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: &BackupStore,
        captures: &mut HashMap<PathBuf, CapturedRegularFile>,
    ) -> Result<CheckpointInfo, AftError> {
        self.create_impl(session, name, files, backup_store, Some(captures))
    }

    fn create_impl(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: &BackupStore,
        mut captures: Option<&mut HashMap<PathBuf, CapturedRegularFile>>,
    ) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        validate_checkpoint_name(name)?;
        self.run_process_maintenance_once_locked()?;
        self.hydrate_session_locked(session)?;
        let explicit_request = !files.is_empty();
        let file_list = if files.is_empty() {
            backup_store.tracked_files(session)
        } else {
            files
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

        // If the caller explicitly named a single file and it was unreadable,
        // that's a real error — surface it rather than silently returning an
        // empty checkpoint. For empty `files` (tracked-file fallback) with no
        // readable files at all, the empty-file checkpoint is a legitimate
        // "nothing to snapshot" outcome and we keep it.
        if explicit_request && file_contents.is_empty() && !skipped.is_empty() {
            let (path, err) = &skipped[0];
            return Err(AftError::FileNotFound {
                path: format!("{}: {}", path.display(), err),
            });
        }

        let created_at = current_timestamp();
        let created_order = current_timestamp_nanos()
            .saturating_add(self.blob_counter.fetch_add(1, Ordering::Relaxed));
        let file_count = file_contents.len();
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

        restore_paths_atomically(checkpoint, &paths)?;
        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            storage_path,
            evicted: Vec::new(),
            skipped: Vec::new(),
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
        restore_paths_atomically(checkpoint, validated_paths)?;
        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            storage_path,
            evicted: Vec::new(),
            skipped: Vec::new(),
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
        self.cleanup_locked()
    }

    fn cleanup_locked(&mut self) -> Result<(), AftError> {
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
            if !is_safe_checkpoint_name(&name) {
                continue;
            }
            let meta_path = checkpoint_dir.join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            let checkpoint = read_checkpoint_from_disk(&checkpoint_dir, session, &name)?;
            hydrated.insert(name, checkpoint);
        }
        if hydrated.is_empty() {
            self.checkpoints.remove(session);
        } else {
            self.checkpoints.insert(session.to_string(), hydrated);
            self.evict_excess_checkpoints_locked(session)?;
        }
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
        fs::create_dir_all(&checkpoint_dir).map_err(|error| AftError::IoError {
            path: checkpoint_dir.display().to_string(),
            message: format!("failed to create durable checkpoint directory: {error}"),
        })?;

        let mut files = Vec::with_capacity(checkpoint.file_contents.len());
        for (index, (path, file)) in checkpoint.file_contents.iter().enumerate() {
            let blob = format!(
                "file_{}_{}_{}.blob",
                checkpoint.created_order,
                index,
                self.blob_counter.fetch_add(1, Ordering::Relaxed)
            );
            let bytes = checkpoint_file_bytes(file);
            write_temp_fsync_rename(&checkpoint_dir, &blob, &bytes).map_err(|error| {
                AftError::IoError {
                    path: checkpoint_dir.join(&blob).display().to_string(),
                    message: format!("failed to write durable checkpoint blob: {error}"),
                }
            })?;
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
        fsync_dir(&checkpoint_dir).map_err(|error| AftError::IoError {
            path: checkpoint_dir.display().to_string(),
            message: format!("failed to sync durable checkpoint blobs: {error}"),
        })?;

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
            if let Err(error) = fs::create_dir_all(parent) {
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

fn validate_checkpoint_name(name: &str) -> Result<(), AftError> {
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

    let mut file_contents = HashMap::with_capacity(meta.files.len());
    for file in &meta.files {
        if !is_safe_blob_name(&file.blob) {
            return Err(AftError::IoError {
                path: meta_path.display().to_string(),
                message: format!("invalid durable checkpoint blob name {}", file.blob),
            });
        }
        let blob_path = checkpoint_dir.join(&file.blob);
        let blob = fs::read(&blob_path).map_err(|error| AftError::IoError {
            path: blob_path.display().to_string(),
            message: format!("failed to read durable checkpoint blob: {error}"),
        })?;
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

    Ok(Checkpoint {
        name: meta.name,
        file_contents,
        created_at: meta.created_at,
        created_order: meta.created_order,
    })
}

fn checkpoint_file_bytes(file: &CheckpointFile) -> Vec<u8> {
    match &file.kind {
        CheckpointFileKind::Regular { bytes } => bytes.to_vec(),
        CheckpointFileKind::Symlink { target, .. } => {
            target.as_os_str().to_string_lossy().as_bytes().to_vec()
        }
    }
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
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(tmp_path, final_path)
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
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

fn restore_paths_atomically(checkpoint: &Checkpoint, paths: &[PathBuf]) -> Result<(), AftError> {
    let mut pre_restore_snapshot: HashMap<PathBuf, Option<CheckpointFile>> = HashMap::new();
    for path in paths {
        let current = CheckpointFile::read_optional(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: format!("failed to snapshot pre-restore file metadata: {error}"),
        })?;
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

    Ok(())
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

    fn checkpoint_file(content: &str) -> CheckpointFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), content).unwrap();
        CheckpointFile::read(file.path()).unwrap()
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
    fn checkpoint_lock_scope_remains_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let scope_dir = dir.path().join("checkpoints").join("project-scope");
        let lock_path = scope_dir.join("checkpoint.lock");
        let path = dir.path().join("checkpoint.txt");
        fs::write(&path, "data").unwrap();
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);

        store
            .create(DEFAULT_SESSION_ID, "released", vec![path], &backup_store)
            .unwrap();

        assert!(
            scope_dir.is_dir(),
            "released lock scope must remain durable"
        );
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

    #[test]
    fn concurrent_checkpoint_stores_keep_shared_lock_scope_stable() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("locks").join("checkpoint.lock");
        let file = dir.path().join("shared.txt");
        fs::write(&file, "content").unwrap();
        let start = Arc::new(std::sync::Barrier::new(3));

        let workers = (0..2)
            .map(|worker| {
                let lock_path = lock_path.clone();
                let file = file.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    let mut store =
                        CheckpointStore::with_lock_path(lock_path, Duration::from_secs(2));
                    let backup = BackupStore::new();
                    start.wait();
                    for iteration in 0..100 {
                        store
                            .create(
                                DEFAULT_SESSION_ID,
                                &format!("worker-{worker}-{iteration}"),
                                vec![file.clone()],
                                &backup,
                            )
                            .expect("shared checkpoint lock scope must remain available");
                    }
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        for worker in workers {
            worker.join().expect("checkpoint worker");
        }
        assert!(
            lock_path.parent().unwrap().is_dir(),
            "shared lock scope must remain stable between owners"
        );
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
}
