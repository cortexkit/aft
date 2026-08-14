use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backup::{BackupStore, CapturedRegularFile};
use crate::error::AftError;
use crate::fs_lock;

const CHECKPOINT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Metadata about a checkpoint, returned by list/create/restore.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub name: String,
    pub file_count: usize,
    pub created_at: u64,
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
}

#[derive(Debug, Clone)]
struct CheckpointFile {
    /// Permission bits captured at snapshot time, used to re-apply
    /// permissions on restore. `fs::Metadata` itself cannot be
    /// reconstructed after a process restart, so we persist the mode bits
    /// and rebuild a `Permissions` from them when hydrating.
    permissions: fs::Permissions,
    kind: CheckpointFileKind,
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

/// On-disk representation of a single snapshotted file (manifest entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCheckpointFile {
    path: String,
    #[serde(rename = "kind")]
    kind: String,
    /// Unix-style permission mode bits (e.g. 0o644). The source of truth for
    /// re-applying permissions on restore (see [`permission_from_mode`]).
    #[serde(default)]
    mode: u32,
    /// Relative path of the raw-bytes blob under the checkpoint dir, for
    /// regular files only.
    #[serde(default)]
    blob: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    target_is_dir: bool,
}

/// On-disk representation of a checkpoint (manifest.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCheckpoint {
    /// Raw session id — on-disk dir names are sanitized+hashed, so the
    /// manifest carries the authoritative session key for hydration.
    session: String,
    name: String,
    created_at: u64,
    files: Vec<StoredCheckpointFile>,
}

impl CheckpointFile {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let permissions = metadata.permissions();
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let target = fs::read_link(path)?;
            let target_is_dir = fs::metadata(path)
                .map(|target_metadata| target_metadata.is_dir())
                .unwrap_or(false);
            return Ok(Self {
                permissions,
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
        Ok(Self {
            permissions: capture.metadata().permissions(),
            kind: CheckpointFileKind::Regular {
                bytes: capture.shared_bytes(),
            },
        })
    }

    fn from_fresh_capture(capture: CapturedRegularFile) -> Self {
        Self {
            permissions: capture.metadata().permissions(),
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
}

/// Workspace-wide, per-session checkpoint store.
///
/// Partitioned by session (issue #14): two OpenCode sessions sharing one bridge
/// can both create checkpoints named `snap1` without collision, and restoring
/// from one session does not leak the other's file set. Checkpoints are kept
/// in memory only — a bridge crash drops all of them, which is a deliberate
/// trade-off to keep this refactor bounded. Durable checkpoints are a possible
/// follow-up.
///
/// DURABILITY: since v0.49.4-operant this store additionally mirrors every
/// mutation to disk under `<storage_root>/<session>/<name>/` and hydrates on
/// startup, so checkpoints survive bridge restarts (the "in memory only"
/// limitation above no longer applies). `list`/`restore` fall back to the
/// hydrated in-memory map, which is kept in sync on every mutation.
#[derive(Debug)]
pub struct CheckpointStore {
    /// session -> name -> checkpoint
    checkpoints: HashMap<String, HashMap<String, Checkpoint>>,
    lock_path: PathBuf,
    lock_timeout: Duration,
    /// Root directory for durable checkpoint storage. Lock file lives at
    /// `<storage_root>/checkpoint.lock`; each checkpoint is persisted at
    /// `<storage_root>/<session>/<name>/`.
    storage_root: PathBuf,
}

impl CheckpointStore {
    pub fn new() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let project_key = crate::path_identity::project_scope_key(&project_root);
        let lock_path = crate::bash_background::storage_dir(None)
            .join("checkpoints")
            .join(project_key)
            .join("checkpoint.lock");
        Self::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT)
    }

    /// Point this store's mutation lock at a private path. Tests use this for
    /// isolation instead of mutating the process-global `AFT_CACHE_DIR` env
    /// var, which races parallel lib tests that resolve storage paths.
    #[cfg(test)]
    pub(crate) fn set_lock_path_for_test(&mut self, lock_path: PathBuf) {
        self.storage_root = lock_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        self.lock_path = lock_path;
    }

    fn with_lock_path(lock_path: PathBuf, lock_timeout: Duration) -> Self {
        let storage_root = lock_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut store = CheckpointStore {
            checkpoints: HashMap::new(),
            lock_path,
            lock_timeout,
            storage_root,
        };
        store.hydrate_from_disk();
        store
    }

    fn acquire_mutation_lock(&self) -> Result<fs_lock::LockGuard, AftError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent).map_err(|error| AftError::IoError {
                path: parent.display().to_string(),
                message: format!("failed to create checkpoint lock directory: {error}"),
            })?;
        }

        fs_lock::try_acquire(&self.lock_path, self.lock_timeout).map_err(|error| match error {
            fs_lock::AcquireError::Timeout => AftError::IoError {
                path: self.lock_path.display().to_string(),
                message: "timed out acquiring checkpoint mutation lock".to_string(),
            },
            fs_lock::AcquireError::Io(error) => AftError::IoError {
                path: self.lock_path.display().to_string(),
                message: format!("failed to acquire checkpoint mutation lock: {error}"),
            },
        })
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
        let file_count = file_contents.len();

        let checkpoint = Checkpoint {
            name: name.to_string(),
            file_contents,
            created_at,
        };

        self.checkpoints
            .entry(session.to_string())
            .or_default()
            .insert(name.to_string(), checkpoint);

        if let Err(e) = self.persist_checkpoint(session, name) {
            crate::slog_warn!(
                "checkpoint {}: persisted in memory but failed to write to disk: {}",
                name,
                e
            );
        }

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
            skipped,
        })
    }

    /// Restore a checkpoint by overwriting files with stored content.
    pub fn restore(&self, session: &str, name: &str) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
        let checkpoint = self.get(session, name)?;
        let mut paths = checkpoint.file_contents.keys().cloned().collect::<Vec<_>>();
        paths.sort();

        restore_paths_atomically(checkpoint, &paths)?;

        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            skipped: Vec::new(),
        })
    }

    /// Restore a checkpoint using a caller-validated path list.
    pub fn restore_validated(
        &self,
        session: &str,
        name: &str,
        validated_paths: &[PathBuf],
    ) -> Result<CheckpointInfo, AftError> {
        let _mutation_lock = self.acquire_mutation_lock()?;
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
            skipped: Vec::new(),
        })
    }

    /// Return the file paths stored for a checkpoint.
    pub fn file_paths(&self, session: &str, name: &str) -> Result<Vec<PathBuf>, AftError> {
        let checkpoint = self.get(session, name)?;
        Ok(checkpoint.file_contents.keys().cloned().collect())
    }

    /// Return absolute file paths stored for a checkpoint without restoring it.
    pub fn absolute_file_paths(&self, session: &str, name: &str) -> Result<Vec<PathBuf>, AftError> {
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
        let dir = self.checkpoint_dir(session, name);
        let removed = self
            .checkpoints
            .get_mut(session)
            .map(|session_checkpoints| session_checkpoints.remove(name).is_some())
            .unwrap_or(false);
        if removed {
            if let Err(e) = fs::remove_dir_all(&dir) {
                crate::slog_warn!(
                    "checkpoint {}: removed from memory but failed to delete on disk: {}",
                    name,
                    e
                );
            }
        }
        if let Some(session_checkpoints) = self.checkpoints.get(session) {
            if session_checkpoints.is_empty() {
                self.checkpoints.remove(session);
            }
        }
        removed
    }

    /// List all checkpoints for this session with metadata.
    pub fn list(&self, session: &str) -> Vec<CheckpointInfo> {
        self.checkpoints
            .get(session)
            .map(|s| {
                s.values()
                    .map(|cp| CheckpointInfo {
                        name: cp.name.clone(),
                        file_count: cp.file_contents.len(),
                        created_at: cp.created_at,
                        skipped: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Total checkpoint count across all sessions (for `/aft-status`).
    pub fn total_count(&self) -> usize {
        self.checkpoints.values().map(|s| s.len()).sum()
    }

    /// Remove checkpoints older than `ttl_hours` across all sessions.
    /// Empty session entries are pruned after cleanup.
    pub fn cleanup(&mut self, ttl_hours: u32) {
        let now = current_timestamp();
        let ttl_secs = ttl_hours as u64 * 3600;
        self.checkpoints.retain(|_, session_cps| {
            session_cps.retain(|_, cp| now.saturating_sub(cp.created_at) < ttl_secs);
            !session_cps.is_empty()
        });
        // Mirror the on-disk store: drop persisted checkpoints that aged out.
        // Compute the set of still-alive on-disk dirs first (no borrow of
        // `self` inside the traversal below).
        let mut alive_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (session, cps) in &self.checkpoints {
            for name in cps.keys() {
                alive_dirs.insert(self.checkpoint_dir(session, name));
            }
        }
        if let Ok(session_entries) = fs::read_dir(&self.storage_root) {
            for session_entry in session_entries.flatten() {
                if !session_entry.path().is_dir() {
                    continue;
                }
                if let Ok(cp_entries) = fs::read_dir(session_entry.path()) {
                    let mut session_empty = true;
                    for cp_entry in cp_entries.flatten() {
                        let cp_path = cp_entry.path();
                        if !cp_path.is_dir() {
                            continue;
                        }
                        session_empty = false;
                        if !alive_dirs.contains(&cp_path) {
                            let _ = fs::remove_dir_all(&cp_path);
                        }
                    }
                    if session_empty {
                        let _ = fs::remove_dir_all(session_entry.path());
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Durable persistence
    // -------------------------------------------------------------------

    /// Directory where a checkpoint's durable payload lives.
    ///
    /// The dir components are `sanitize(name)-<shorthash>`: sanitizing
    /// guards path traversal while the hash suffix disambiguates names that
    /// sanitize to the same string (e.g. `a/b` vs `a_b`), so two distinct
    /// checkpoints can never map to the same on-disk dir and silently
    /// overwrite each other on hydration.
    fn checkpoint_dir(&self, session: &str, name: &str) -> PathBuf {
        self.storage_root
            .join(format!(
                "{}-{}",
                sanitize_for_path(session),
                short_hash(session)
            ))
            .join(format!("{}-{}", sanitize_for_path(name), short_hash(name)))
    }

    /// Persist one checkpoint (already in the in-memory map) to disk.
    ///
    /// Layout: `<storage_root>/<session>/<name>/manifest.json` plus one
    /// `blob-<n>.bin` per regular file. Symlinks are recorded in the
    /// manifest only. The manifest is written atomically (tmp + rename).
    fn persist_checkpoint(&self, session: &str, name: &str) -> Result<(), AftError> {
        let checkpoint = self.get(session, name)?;
        let dir = self.checkpoint_dir(session, name);
        fs::create_dir_all(&dir).map_err(|error| AftError::IoError {
            path: dir.display().to_string(),
            message: format!("failed to create checkpoint dir: {error}"),
        })?;

        // Drop orphaned blobs from a previous snapshot with the same name
        // (an overwrite with fewer files would otherwise leave stale blob-N
        // files behind). We are under the mutation lock, so this is safe.
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with("blob-") && file_name.ends_with(".bin") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }

        let mut stored_files: Vec<StoredCheckpointFile> = Vec::new();
        let mut blob_index = 0usize;
        let mut paths: Vec<&PathBuf> = checkpoint.file_contents.keys().collect();
        paths.sort();
        for path in paths {
            let file = &checkpoint.file_contents[path];
            let mut stored = StoredCheckpointFile {
                path: path.display().to_string(),
                kind: String::new(),
                mode: permission_mode(&file.permissions),
                blob: None,
                target: None,
                target_is_dir: false,
            };
            match &file.kind {
                CheckpointFileKind::Regular { bytes } => {
                    let blob = format!("blob-{}.bin", blob_index);
                    blob_index += 1;
                    fs::write(dir.join(&blob), &bytes[..]).map_err(|error| AftError::IoError {
                        path: dir.join(&blob).display().to_string(),
                        message: format!("failed to write checkpoint blob: {error}"),
                    })?;
                    stored.kind = "regular".to_string();
                    stored.blob = Some(blob);
                }
                CheckpointFileKind::Symlink {
                    target,
                    target_is_dir,
                } => {
                    stored.kind = "symlink".to_string();
                    stored.target = Some(target.display().to_string());
                    stored.target_is_dir = *target_is_dir;
                }
            }
            stored_files.push(stored);
        }

        let stored = StoredCheckpoint {
            session: session.to_string(),
            name: checkpoint.name.clone(),
            created_at: checkpoint.created_at,
            files: stored_files,
        };
        let raw = serde_json::to_vec(&stored).map_err(|error| AftError::IoError {
            path: dir.display().to_string(),
            message: format!("failed to serialize checkpoint manifest: {error}"),
        })?;
        let tmp = dir.join("manifest.json.tmp");
        fs::write(&tmp, &raw).map_err(|error| AftError::IoError {
            path: tmp.display().to_string(),
            message: format!("failed to write checkpoint manifest: {error}"),
        })?;
        fs::rename(&tmp, dir.join("manifest.json")).map_err(|error| AftError::IoError {
            path: dir.join("manifest.json").display().to_string(),
            message: format!("failed to finalize checkpoint manifest: {error}"),
        })
    }

    /// Load all persisted checkpoints for this project into memory.
    ///
    /// Non-fatal: unreadable/corrupt entries are skipped with a warning so
    /// a bad manifest never blocks bridge startup.
    fn hydrate_from_disk(&mut self) {
        let Ok(session_entries) = fs::read_dir(&self.storage_root) else {
            return;
        };
        for session_entry in session_entries.flatten() {
            if !session_entry.path().is_dir() {
                continue;
            }
            let Ok(cp_entries) = fs::read_dir(session_entry.path()) else {
                continue;
            };
            for cp_entry in cp_entries.flatten() {
                let cp_path = cp_entry.path();
                if !cp_path.is_dir() {
                    continue;
                }
                let manifest_path = cp_path.join("manifest.json");
                let raw = match fs::read(&manifest_path) {
                    Ok(raw) => raw,
                    Err(_) => continue,
                };
                let stored: StoredCheckpoint = match serde_json::from_slice(&raw) {
                    Ok(s) => s,
                    Err(e) => {
                        crate::slog_warn!(
                            "checkpoint hydration: skipping corrupt manifest {}: {}",
                            manifest_path.display(),
                            e
                        );
                        continue;
                    }
                };

                let mut file_contents: HashMap<PathBuf, CheckpointFile> = HashMap::new();
                let mut ok = true;
                for f in stored.files {
                    let file = match f.kind.as_str() {
                        "regular" => {
                            let Some(blob) = &f.blob else {
                                ok = false;
                                break;
                            };
                            let bytes = match fs::read(cp_path.join(blob)) {
                                Ok(b) => Arc::<[u8]>::from(b),
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            };
                            CheckpointFile {
                                permissions: permission_from_mode(f.mode),
                                kind: CheckpointFileKind::Regular { bytes },
                            }
                        }
                        "symlink" => CheckpointFile {
                            permissions: permission_from_mode(f.mode),
                            kind: CheckpointFileKind::Symlink {
                                target: PathBuf::from(f.target.unwrap_or_default()),
                                target_is_dir: f.target_is_dir,
                            },
                        },
                        _ => {
                            ok = false;
                            break;
                        }
                    };
                    file_contents.insert(PathBuf::from(f.path), file);
                }
                if !ok {
                    crate::slog_warn!(
                        "checkpoint hydration: skipping incomplete checkpoint {} (missing blobs)",
                        cp_path.display()
                    );
                    continue;
                }
                // Use the manifest's authoritative session id, not the
                // (sanitized+hashed) on-disk dir name.
                self.checkpoints
                    .entry(stored.session.clone())
                    .or_default()
                    .insert(
                        stored.name.clone(),
                        Checkpoint {
                            name: stored.name,
                            file_contents,
                            created_at: stored.created_at,
                        },
                    );
            }
        }
    }

    fn get(&self, session: &str, name: &str) -> Result<&Checkpoint, AftError> {
        self.checkpoints
            .get(session)
            .and_then(|s| s.get(name))
            .ok_or_else(|| AftError::CheckpointNotFound {
                name: name.to_string(),
            })
    }
}

/// Sanitize a session/name into a safe single path component.
///
/// Session ids and checkpoint names are caller-controlled strings; they must
/// never be able to escape the storage root via `../` or path separators.
fn sanitize_for_path(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    let mut chars = component.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Separators and NUL would allow escaping the storage root.
            '/' | '\\' | '\0' => out.push('_'),
            // Leading dots could produce "." / ".." path components.
            '.' if out.is_empty() => {
                out.push('_');
                // Consume a second dot so ".." never survives.
                if chars.peek() == Some(&'.') {
                    let _ = chars.next();
                    out.push('_');
                }
            }
            c if c.is_ascii_control() => out.push('_'),
            c => out.push(c),
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Deterministic 64-bit FNV-1a hash, hex-encoded (8 chars).
///
/// Used to disambiguate on-disk dir names after sanitization. Implemented
/// inline (no `DefaultHasher`, whose algorithm is not stable across Rust
/// releases) so hydrated dir names always match.
fn short_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{:08x}", hash)
}

#[cfg(unix)]
fn permission_mode(permissions: &fs::Permissions) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    permissions.mode() & 0o7777
}

#[cfg(windows)]
fn permission_mode(permissions: &fs::Permissions) -> u32 {
    use std::os::windows::fs::PermissionsExt;
    permissions.mode() & 0o7777
}

#[cfg(not(any(unix, windows)))]
fn permission_mode(permissions: &fs::Permissions) -> u32 {
    if permissions.readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn permission_from_mode(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode & 0o7777)
}

#[cfg(windows)]
fn permission_from_mode(mode: u32) -> fs::Permissions {
    use std::os::windows::fs::PermissionsExt;
    fs::Permissions::from_mode(mode & 0o7777)
}

#[cfg(not(any(unix, windows)))]
fn permission_from_mode(mode: u32) -> fs::Permissions {
    fs::Permissions::from_readonly(mode & 0o444 == 0)
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
            fs::set_permissions(path, snapshot.permissions.clone()).map_err(|error| {
                AftError::IoError {
                    path: path.display().to_string(),
                    message: format!("failed to restore checkpoint file permissions: {error}"),
                }
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

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

    fn checkpoint_store() -> (CheckpointStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("checkpoint.lock");
        (
            CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT),
            dir,
        )
    }

    fn checkpoint_file(content: &str) -> CheckpointFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), content).unwrap();
        CheckpointFile::read(file.path()).unwrap()
    }

    #[test]
    fn checkpoints_survive_store_recreation() {
        // Durability: a checkpoint persisted by one store instance (simulating
        // one bridge process) must be listable + restorable by a fresh store
        // over the same storage root (simulating a new bridge process).
        let (path, _dir) = temp_file("cp_durable.txt", "durable-original");
        let storage = tempfile::tempdir().unwrap();
        let lock_path = storage.path().join("checkpoint.lock");
        let backup_store = BackupStore::new();

        {
            let mut store =
                CheckpointStore::with_lock_path(lock_path.clone(), CHECKPOINT_LOCK_TIMEOUT);
            store
                .create(
                    DEFAULT_SESSION_ID,
                    "durable",
                    vec![path.clone()],
                    &backup_store,
                )
                .unwrap();
            // On-disk layout: <root>/<session>-<hash>/<name>-<hash>/{manifest.json, blob-0.bin}
            let cp_dir = store.checkpoint_dir(DEFAULT_SESSION_ID, "durable");
            assert!(
                cp_dir.join("manifest.json").exists(),
                "manifest must be written"
            );
            assert!(cp_dir.join("blob-0.bin").exists(), "blob must be written");
        } // drop store — simulates bridge shutdown

        // Fresh store over the same storage root — hydrates from disk.
        let mut store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        let list = store.list(DEFAULT_SESSION_ID);
        assert_eq!(list.len(), 1, "hydrated checkpoint should be listable");
        assert_eq!(list[0].name, "durable");
        assert_eq!(list[0].file_count, 1);

        // Modify the file, then restore from the hydrated checkpoint.
        fs::write(&path, "durable-modified").unwrap();
        store.restore(DEFAULT_SESSION_ID, "durable").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "durable-original");
    }

    #[test]
    fn sanitize_blocks_traversal_and_collides_disambiguated() {
        // Every separator/NUL is replaced with '_' — no component can ever
        // contain a path separator, so traversal is impossible regardless of
        // embedded ".." (which survives only as harmless text inside a name).
        assert_eq!(sanitize_for_path("../../etc/passwd"), "___.._etc_passwd");
        assert_eq!(sanitize_for_path(".."), "__");
        assert_eq!(sanitize_for_path("/abs/path"), "_abs_path");
        assert_eq!(sanitize_for_path("\\win\\path"), "_win_path");
        assert_eq!(sanitize_for_path("a/b"), "a_b");
        assert_eq!(sanitize_for_path("a\\b"), "a_b");
        assert_eq!(sanitize_for_path(""), "_");
        // Sanitized output must never contain a separator.
        for probe in [
            "../../etc/passwd",
            "..",
            "/abs/path",
            "\\win\\path",
            "a/b",
            "a\\b",
        ] {
            let out = sanitize_for_path(probe);
            assert!(
                !out.contains('/') && !out.contains('\\'),
                "unsafe output {out}"
            );
        }

        // Distinct names that sanitize identically get distinct dirs via hash.
        let (p, _d) = temp_file("cp_collision.txt", "x");
        let storage = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::with_lock_path(
            storage.path().join("checkpoint.lock"),
            CHECKPOINT_LOCK_TIMEOUT,
        );
        let backup = BackupStore::new();
        store
            .create(DEFAULT_SESSION_ID, "a/b", vec![p.clone()], &backup)
            .unwrap();
        store
            .create(DEFAULT_SESSION_ID, "a_b", vec![p.clone()], &backup)
            .unwrap();
        assert_ne!(
            store.checkpoint_dir(DEFAULT_SESSION_ID, "a/b"),
            store.checkpoint_dir(DEFAULT_SESSION_ID, "a_b")
        );
        assert_eq!(store.list(DEFAULT_SESSION_ID).len(), 2);
    }

    #[test]
    fn delete_and_cleanup_mirror_to_disk() {
        let (p, _d) = temp_file("cp_mirror.txt", "x");
        let storage = tempfile::tempdir().unwrap();
        let lock_path = storage.path().join("checkpoint.lock");
        let backup = BackupStore::new();

        {
            let mut store =
                CheckpointStore::with_lock_path(lock_path.clone(), CHECKPOINT_LOCK_TIMEOUT);
            store
                .create(DEFAULT_SESSION_ID, "keep", vec![p.clone()], &backup)
                .unwrap();
            store
                .create(DEFAULT_SESSION_ID, "drop", vec![p.clone()], &backup)
                .unwrap();
            assert!(store.checkpoint_dir(DEFAULT_SESSION_ID, "drop").exists());

            // delete mirrors to disk
            assert!(store.delete(DEFAULT_SESSION_ID, "drop"));
            assert!(!store.checkpoint_dir(DEFAULT_SESSION_ID, "drop").exists());
            assert!(store.checkpoint_dir(DEFAULT_SESSION_ID, "keep").exists());
        }

        // Fresh store over the same root: only "keep" hydrates.
        let store = CheckpointStore::with_lock_path(lock_path, CHECKPOINT_LOCK_TIMEOUT);
        let names: Vec<String> = store
            .list(DEFAULT_SESSION_ID)
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["keep".to_string()]);
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

        let default_list = store.list(DEFAULT_SESSION_ID);
        assert_eq!(default_list.len(), 2);
        let names: Vec<&str> = default_list.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));

        let other_list = store.list("other_session");
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
                },
            );

        assert_eq!(store.total_count(), 2);
        store.cleanup(24); // 24 hours
        assert_eq!(store.total_count(), 1);
        assert_eq!(store.list(DEFAULT_SESSION_ID)[0].name, "recent");
        assert!(store.list("other").is_empty());
    }

    #[test]
    fn restore_nonexistent_returns_error() {
        let (store, _store_dir) = checkpoint_store();
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
