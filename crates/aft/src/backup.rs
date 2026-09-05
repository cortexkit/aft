use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use crate::db::TrackedConnection;
use rusqlite::Connection;

use crate::db::backups::BackupRow;
use crate::error::AftError;
use sha2::{Digest, Sha256};

pub const DEFAULT_MAX_UNDO_DEPTH: usize = 20;
/// Default upper bound for one automatic undo snapshot (64 MiB).
pub const DEFAULT_MAX_BACKUP_FILE_SIZE: u64 = 64 * 1024 * 1024;

static BACKUP_SKIPPED_TOO_LARGE_TOTAL: AtomicU64 = AtomicU64::new(0);
static BACKUP_SKIPPED_TEMP_PATH_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Process-wide automatic-backup skip counters for status and health surfaces.
pub fn backup_skipped_totals() -> (u64, u64) {
    (
        BACKUP_SKIPPED_TOO_LARGE_TOTAL.load(Ordering::Relaxed),
        BACKUP_SKIPPED_TEMP_PATH_TOTAL.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
const MAX_UNDO_DEPTH: usize = DEFAULT_MAX_UNDO_DEPTH;
const V2_FORMAT_VERSION: &str = "v2";
const DB_RESTORE_META_VERSION: u32 = 1;
const MAX_RESTORE_OPERATION_LOCK_RETRIES: usize = 32;

#[cfg(test)]
type RestoreBeforeLockHook = Box<dyn FnMut(usize) -> bool + Send>;

#[cfg(test)]
static RESTORE_BEFORE_LOCK_HOOKS: LazyLock<Mutex<HashMap<String, RestoreBeforeLockHook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static BACKUP_MAINTENANCE_KEYS: LazyLock<Mutex<HashSet<(PathBuf, Option<String>)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[cfg(test)]
fn set_restore_before_lock_hook_for_tests(
    session: &str,
    hook: impl FnMut(usize) -> bool + Send + 'static,
) {
    RESTORE_BEFORE_LOCK_HOOKS
        .lock()
        .unwrap()
        .insert(session.to_string(), Box::new(hook));
}

#[cfg(test)]
fn run_restore_before_lock_hook_for_tests(session: &str, attempt: usize) {
    let mut hooks = RESTORE_BEFORE_LOCK_HOOKS.lock().unwrap();
    let Some(mut hook) = hooks.remove(session) else {
        return;
    };
    drop(hooks);
    let keep_hook = hook(attempt);
    if keep_hook {
        RESTORE_BEFORE_LOCK_HOOKS
            .lock()
            .unwrap()
            .insert(session.to_string(), hook);
    }
}

#[cfg(not(test))]
fn run_restore_before_lock_hook_for_tests(_session: &str, _attempt: usize) {}

/// Current on-disk backup metadata schema version.
///
/// Bump this when the `meta.json` shape changes. Readers check the field and
/// refuse or migrate older versions instead of misinterpreting them.
const SCHEMA_VERSION: u32 = 4;

/// A single backup entry for a file.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub backup_id: String,
    /// UTF-8 view of the captured regular-file bytes, kept for API/tests that
    /// inspect text backups. Restore uses `content_bytes` so binary files round-trip.
    pub content: String,
    pub content_bytes: Arc<[u8]>,
    pub timestamp: u64,
    pub order: u128,
    pub description: String,
    pub op_id: Option<String>,
    pub kind: BackupEntryKind,
    pub mode: Option<u32>,
    pub link_target: Option<PathBuf>,
    pub created_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupEntryKind {
    Content,
    Symlink,
    Tombstone,
}

/// One regular file captured for both rollback and durable undo.
///
/// Seeded consumers must call [`Self::refresh_if_stale`] immediately before
/// storing the snapshot. A size or modification-time change means the buffer
/// may no longer describe the pre-mutation file on disk, so the capture is
/// replaced with a fresh read. Symlinks deliberately use the existing path-based
/// snapshot code so their link metadata and target semantics remain unchanged.
#[derive(Debug, Clone)]
pub(crate) struct CapturedRegularFile {
    metadata: std::fs::Metadata,
    bytes: Arc<[u8]>,
}

impl CapturedRegularFile {
    pub(crate) fn read(path: &Path) -> std::io::Result<Option<Self>> {
        let before = std::fs::symlink_metadata(path)?;
        if !before.is_file() {
            return Ok(None);
        }

        let mut bytes: Arc<[u8]> = read_captured_content(path)?.into();
        let mut metadata = std::fs::symlink_metadata(path)?;
        if !same_capture_stat(&before, &metadata) {
            if !metadata.is_file() {
                return Ok(None);
            }
            bytes = read_captured_content(path)?.into();
            metadata = std::fs::symlink_metadata(path)?;
        }

        Ok(Some(Self { metadata, bytes }))
    }

    pub(crate) fn read_text(path: &Path) -> std::io::Result<Option<(Self, String)>> {
        let Some(capture) = Self::read(path)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(capture.bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .to_owned();
        Ok(Some((capture, text)))
    }

    pub(crate) fn refresh_if_stale(&mut self, path: &Path) -> std::io::Result<bool> {
        let current = std::fs::symlink_metadata(path)?;
        if current.is_file() && same_capture_stat(&self.metadata, &current) {
            return Ok(false);
        }

        let Some(fresh) = Self::read(path)? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "captured regular file is no longer a regular file",
            ));
        };
        *self = fresh;
        Ok(true)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    pub(crate) fn metadata(&self) -> &std::fs::Metadata {
        &self.metadata
    }
}

fn same_capture_stat(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn read_captured_content(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(test)]
    {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        *CAPTURE_READ_COUNTS.lock().unwrap().entry(key).or_default() += 1;
    }
    std::fs::read(path)
}

#[cfg(test)]
static CAPTURE_READ_COUNTS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
pub(crate) fn reset_capture_read_count(path: &Path) {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    CAPTURE_READ_COUNTS.lock().unwrap().remove(&key);
}

#[cfg(test)]
pub(crate) fn capture_read_count(path: &Path) -> usize {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    CAPTURE_READ_COUNTS
        .lock()
        .unwrap()
        .get(&key)
        .copied()
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct BackupEntryHead {
    order: u128,
    op_id: Option<String>,
}

impl BackupEntryHead {
    fn from_entry(entry: &BackupEntry) -> Self {
        Self {
            order: entry.order,
            op_id: entry.op_id.clone(),
        }
    }

    fn from_row(row: &BackupRow) -> Self {
        Self {
            order: row.order,
            op_id: row.op_id.clone(),
        }
    }
}

impl BackupEntry {
    fn to_backup_row(
        &self,
        harness: &str,
        session_id: &str,
        project_key: &str,
        file_path: &str,
        path_hash: &str,
        backup_path: Option<&str>,
    ) -> BackupRow {
        BackupRow {
            backup_id: self.backup_id.clone(),
            harness: harness.to_string(),
            session_id: session_id.to_string(),
            project_key: project_key.to_string(),
            op_id: self.op_id.clone(),
            order: self.order,
            file_path: file_path.to_string(),
            path_hash: path_hash.to_string(),
            backup_path: backup_path.map(str::to_string),
            kind: match self.kind {
                BackupEntryKind::Content => "content".to_string(),
                BackupEntryKind::Symlink => "symlink".to_string(),
                BackupEntryKind::Tombstone => "tombstone".to_string(),
            },
            description: self.description.clone(),
            created_at: i64::try_from(self.timestamp).unwrap_or(i64::MAX),
            is_tombstone: matches!(self.kind, BackupEntryKind::Tombstone),
            restore_meta: Some(restore_metadata_json(self)),
        }
    }
}

impl TryFrom<BackupRow> for BackupEntry {
    type Error = std::io::Error;

    fn try_from(row: BackupRow) -> Result<Self, Self::Error> {
        let kind = if row.is_tombstone || row.kind == "tombstone" {
            BackupEntryKind::Tombstone
        } else if row.kind == "symlink" {
            BackupEntryKind::Symlink
        } else {
            BackupEntryKind::Content
        };
        let backup_path = row.backup_path.clone();
        let persisted_metadata = row
            .restore_meta
            .as_deref()
            .and_then(restore_metadata_from_json);
        let restore_metadata = persisted_metadata.or_else(|| {
            backup_path
                .as_deref()
                .and_then(|path| read_entry_disk_metadata(Path::new(path), &row.backup_id))
        });
        let content_bytes = match kind {
            BackupEntryKind::Content | BackupEntryKind::Symlink => {
                let backup_path = backup_path.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("backup DB row {} has no backup_path", row.backup_id),
                    )
                })?;
                std::fs::read(backup_path)?
            }
            BackupEntryKind::Tombstone => Vec::new(),
        };
        let link_target = if kind == BackupEntryKind::Symlink {
            restore_metadata
                .as_ref()
                .and_then(|metadata| metadata.link_target.clone())
                .or_else(|| {
                    Some(PathBuf::from(
                        String::from_utf8_lossy(&content_bytes).into_owned(),
                    ))
                })
        } else {
            None
        };
        let content = match kind {
            BackupEntryKind::Content => String::from_utf8_lossy(&content_bytes).into_owned(),
            BackupEntryKind::Symlink => link_target
                .as_ref()
                .map(|target| target.display().to_string())
                .unwrap_or_default(),
            BackupEntryKind::Tombstone => String::new(),
        };

        Ok(BackupEntry {
            backup_id: row.backup_id,
            content,
            content_bytes: content_bytes.into(),
            timestamp: u64::try_from(row.created_at).unwrap_or_default(),
            order: row.order,
            description: row.description,
            op_id: row.op_id,
            kind,
            mode: restore_metadata.as_ref().and_then(|metadata| metadata.mode),
            link_target,
            created_dirs: restore_metadata
                .map(|metadata| metadata.created_dirs)
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RestoredOperation {
    pub op_id: String,
    pub restored: Vec<RestoredFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RestoredFile {
    pub path: PathBuf,
    pub backup_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupSkippedReason {
    TooLarge,
    TempPath,
    Disabled,
}

impl BackupSkippedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::TempPath => "temp_path",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
struct SkippedBackup {
    path: PathBuf,
    op_id: Option<String>,
    reason: BackupSkippedReason,
    order: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotDecision {
    Capture,
    Skip(BackupSkippedReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupPolicy {
    pub enabled: bool,
    pub max_depth: usize,
    pub max_file_size: Option<u64>,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: DEFAULT_MAX_UNDO_DEPTH,
            max_file_size: Some(DEFAULT_MAX_BACKUP_FILE_SIZE),
        }
    }
}

/// Per-(session, file) undo store with optional disk persistence.
///
/// Introduced alongside project-shared bridges (issue #14): one bridge can now
/// serve many OpenCode sessions in the same project, so undo history must be
/// partitioned by session to keep session A's edits invisible to session B.
///
/// The 20-entry cap is enforced **per (session, file)** deliberately — a global
/// per-file LRU would re-couple sessions and let one busy session evict
/// another's history.
///
/// Disk layout (metadata `format_version` v2):
///   `<storage_dir>/backups/<session_hash>/session.json` — session metadata
///   `<storage_dir>/backups/<session_hash>/<path_hash>/meta.json` — file path + count + session
///   `<storage_dir>/backups/<session_hash>/<path_hash>/bak_<order>_<id>.bak` — append-only content
///
/// Legacy layouts from before sessionization (flat `<path_hash>/` directly under
/// `backups/`) are migrated by process-wide maintenance on the first backup
/// mutation or restore, never during configure.
#[derive(Debug)]
pub struct BackupStore {
    /// session -> path -> entry stack
    entries: HashMap<String, HashMap<PathBuf, Vec<BackupEntry>>>,
    /// session -> path -> disk metadata
    disk_index: HashMap<String, HashMap<PathBuf, DiskMeta>>,
    /// session -> metadata
    session_meta: HashMap<String, SessionMeta>,
    counter: AtomicU64,
    storage_dir: Option<PathBuf>,
    storage_harness: Option<String>,
    maintenance_ttl_hours: u32,
    db_pool: RwLock<Option<Arc<Mutex<TrackedConnection>>>>,
    db_harness: RwLock<Option<String>>,
    db_project_key: RwLock<Option<String>>,
    /// Stacks whose SQLite mirror has a known-good baseline in this process.
    /// Unknown stacks take the full repair path once before append deltas begin.
    db_mirrored_stacks: RwLock<HashMap<String, HashSet<PathBuf>>>,
    policy: BackupPolicy,
    /// In-process mutation records whose undo snapshot was intentionally skipped.
    skipped_backups: HashMap<String, Vec<SkippedBackup>>,
    #[cfg(test)]
    enforce_temp_path_policy: bool,
    #[cfg(test)]
    disk_io_count: AtomicU64,
    #[cfg(test)]
    fail_next_disk_write: bool,
}

#[derive(Debug, Clone)]
struct DiskMeta {
    dir: PathBuf,
    count: usize,
}

enum DbMirrorPlan<'a> {
    Full,
    Append {
        evicted_orders: &'a [u128],
        new_entry: Option<&'a BackupEntry>,
    },
}

struct DbMirrorContext<'a> {
    harness: &'a str,
    session: &'a str,
    project_key: &'a str,
    file_path: &'a str,
    path_hash: &'a str,
}

#[derive(Debug, Clone, Default)]
struct SessionMeta {
    /// Unix timestamp of last read/write activity in this session namespace.
    /// Maintained in-memory now, reserved for future inactivity-TTL cleanup.
    last_accessed: u64,
}

impl BackupStore {
    pub fn new() -> Self {
        BackupStore {
            entries: HashMap::new(),
            disk_index: HashMap::new(),
            session_meta: HashMap::new(),
            counter: AtomicU64::new(0),
            storage_dir: None,
            storage_harness: None,
            maintenance_ttl_hours: 0,
            db_pool: RwLock::new(None),
            db_harness: RwLock::new(None),
            db_project_key: RwLock::new(None),
            db_mirrored_stacks: RwLock::new(HashMap::new()),
            policy: BackupPolicy::default(),
            skipped_backups: HashMap::new(),
            #[cfg(test)]
            enforce_temp_path_policy: false,
            #[cfg(test)]
            disk_io_count: AtomicU64::new(0),
            #[cfg(test)]
            fail_next_disk_write: false,
        }
    }

    pub fn set_policy(&mut self, policy: BackupPolicy) {
        let old_policy = self.policy;
        self.policy = policy;

        let failed_disk_prunes = if policy.max_depth < old_policy.max_depth {
            self.prune_disk_stacks_to_depth(policy.max_depth)
        } else {
            HashSet::new()
        };

        for (session, files) in &mut self.entries {
            for (key, stack) in files {
                if failed_disk_prunes.contains(&(session.clone(), key.clone())) {
                    continue;
                }
                trim_stack_to_depth(stack, self.policy.max_depth);
            }
        }
        self.entries.retain(|_, files| {
            files.retain(|_, stack| !stack.is_empty());
            !files.is_empty()
        });
    }

    pub fn policy(&self) -> BackupPolicy {
        self.policy
    }

    #[cfg(test)]
    fn fail_next_disk_write_for_tests(&mut self) {
        self.fail_next_disk_write = true;
    }

    #[cfg(test)]
    fn enforce_temp_path_policy_for_tests(&mut self) {
        self.enforce_temp_path_policy = true;
    }

    pub fn set_db_pool(&self, conn: Arc<Mutex<TrackedConnection>>) {
        if let Ok(mut slot) = self.db_pool.write() {
            *slot = Some(conn);
        }
        self.clear_db_mirror_sync();
    }

    pub fn clear_db_pool(&self) {
        if let Ok(mut slot) = self.db_pool.write() {
            *slot = None;
        }
        self.clear_db_mirror_sync();
    }

    pub fn set_db_harness(&self, harness: crate::harness::Harness) {
        if let Ok(mut slot) = self.db_harness.write() {
            *slot = Some(harness.storage_segment());
        }
        self.clear_db_mirror_sync();
    }

    pub fn set_db_project_key(&self, project_key: String) {
        if let Ok(mut slot) = self.db_project_key.write() {
            *slot = Some(project_key);
        }
        self.clear_db_mirror_sync();
    }

    /// Select the storage namespace used for lazy backup persistence.
    ///
    /// Configuration never scans backup sessions. Each `(session, path)` stack
    /// is hydrated under its disk lock when that stack is first read or changed.
    pub fn set_storage_dir(&mut self, dir: PathBuf, ttl_hours: u32) {
        self.set_storage_dir_inner(dir, None, ttl_hours);
    }

    pub fn set_storage_dir_for_harness(
        &mut self,
        dir: PathBuf,
        harness: crate::harness::Harness,
        ttl_hours: u32,
    ) {
        self.set_storage_dir_inner(dir, Some(harness.storage_segment()), ttl_hours);
    }

    fn set_storage_dir_inner(&mut self, dir: PathBuf, harness: Option<String>, ttl_hours: u32) {
        if self.storage_dir.as_ref() == Some(&dir) && self.storage_harness == harness {
            return;
        }

        self.storage_dir = Some(dir);
        self.storage_harness = harness;
        self.maintenance_ttl_hours = ttl_hours;
        self.entries.clear();
        self.disk_index.clear();
        self.session_meta.clear();
        self.skipped_backups.clear();
        self.clear_db_mirror_sync();
    }

    /// Run namespace repair, stale-session GC, and legacy migration at most once
    /// per process for the selected storage namespace.
    ///
    /// Configure deliberately does not invoke this. The first backup mutation or
    /// restore pays this one-time maintenance cost, while ordinary binds remain
    /// independent of harness-wide history.
    pub fn run_process_maintenance_once(&mut self) {
        let Some(storage_dir) = self.storage_dir.clone() else {
            return;
        };
        let key = (storage_dir, self.storage_harness.clone());
        if !BACKUP_MAINTENANCE_KEYS.lock().unwrap().insert(key) {
            return;
        }

        self.record_disk_io_for_tests();
        self.repair_root_backups_if_needed();
        self.gc_stale_sessions(self.maintenance_ttl_hours);
        self.migrate_legacy_layout_if_needed();
    }

    #[cfg(test)]
    pub(crate) fn disk_io_count_for_tests(&self) -> u64 {
        self.disk_io_count.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn record_disk_io_for_tests(&self) {
        self.disk_io_count.fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(not(test))]
    fn record_disk_io_for_tests(&self) {}

    /// Snapshot the current contents of `path` under the given session namespace.
    pub fn snapshot(
        &mut self,
        session: &str,
        path: &Path,
        description: &str,
    ) -> Result<Option<String>, AftError> {
        self.snapshot_with_op(session, path, description, None)
    }

    /// Snapshot the current contents of `path` under the given session namespace,
    /// optionally tagging it with an operation id shared by all files touched by
    /// one mutating tool call.
    pub fn snapshot_with_op(
        &mut self,
        session: &str,
        path: &Path,
        description: &str,
        op_id: Option<&str>,
    ) -> Result<Option<String>, AftError> {
        if !self.prepare_snapshot(session, path, op_id, false)? {
            return Ok(None);
        }
        self.run_process_maintenance_once();
        let key = canonicalize_key(path);
        let _disk_lock = self.acquire_stack_disk_lock(session, &key)?;
        // Hydrate any prior on-disk history before appending, so a snapshot
        // taken on a fresh store (post-restart) extends the existing stack and
        // advances the id counter instead of overwriting history with a single
        // entry and reusing backup-0.
        self.ensure_stack_hydrated_locked(session, &key)?;
        let (id, order) = self.next_id_and_order();
        let entry = backup_entry_from_path(path, id.clone(), order, description, op_id)?;

        self.persist_new_entry_locked(session, &key, entry)?;
        self.touch_session(session);

        Ok(Some(id))
    }

    /// Store an already captured regular file without reading its contents again.
    /// The caller must refresh the capture immediately before invoking this method.
    pub(crate) fn snapshot_with_op_from_capture(
        &mut self,
        session: &str,
        path: &Path,
        description: &str,
        op_id: Option<&str>,
        capture: &CapturedRegularFile,
    ) -> Result<Option<String>, AftError> {
        if !self.prepare_snapshot(session, path, op_id, false)? {
            return Ok(None);
        }
        self.run_process_maintenance_once();
        let key = canonicalize_key(path);
        let _disk_lock = self.acquire_stack_disk_lock(session, &key)?;
        self.ensure_stack_hydrated_locked(session, &key)?;
        let (id, order) = self.next_id_and_order();
        let entry = backup_entry_from_capture(capture, id.clone(), order, description, op_id);

        self.persist_new_entry_locked(session, &key, entry)?;
        self.touch_session(session);

        Ok(Some(id))
    }

    /// Record that `path` was created by the operation and should be removed
    /// if that operation is undone. No file content is captured.
    pub fn snapshot_op_tombstone(
        &mut self,
        session: &str,
        op_id: &str,
        path: &Path,
        description: &str,
    ) -> Result<Option<String>, AftError> {
        if !self.prepare_snapshot(session, path, Some(op_id), true)? {
            return Ok(None);
        }
        self.run_process_maintenance_once();
        let key = canonicalize_key(path);
        let _disk_lock = self.acquire_stack_disk_lock(session, &key)?;
        self.ensure_stack_hydrated_locked(session, &key)?;
        let created_dirs = path.parent().map(missing_parent_dirs).unwrap_or_default();
        let (id, order) = self.next_id_and_order();
        let entry = BackupEntry {
            backup_id: id.clone(),
            content: String::new(),
            content_bytes: Arc::from([]),
            timestamp: current_timestamp(),
            order,
            description: description.to_string(),
            op_id: Some(op_id.to_string()),
            kind: BackupEntryKind::Tombstone,
            mode: None,
            link_target: None,
            created_dirs,
        };

        self.persist_new_entry_locked(session, &key, entry)?;
        self.touch_session(session);

        Ok(Some(id))
    }

    /// Restore every top-of-stack backup entry belonging to the most recent
    /// operation in this session.
    pub fn restore_last_operation(&mut self, session: &str) -> Result<RestoredOperation, AftError> {
        self.run_process_maintenance_once();
        let mut candidate_keys = self.restore_operation_candidate_keys(session)?;
        if candidate_keys.is_empty() {
            self.load_latest_operation_from_db_or_log(session);
            candidate_keys = self.restore_operation_candidate_keys(session)?;
        }

        for attempt in 0..MAX_RESTORE_OPERATION_LOCK_RETRIES {
            if candidate_keys.is_empty() {
                return Err(AftError::NoUndoHistory {
                    path: "operation".to_string(),
                });
            }

            run_restore_before_lock_hook_for_tests(session, attempt);

            let disk_locks = self.acquire_stack_disk_locks(session, &candidate_keys)?;
            let locked_keys: HashSet<PathBuf> = candidate_keys.iter().cloned().collect();
            let current_keys = self.restore_operation_candidate_keys(session)?;
            let current_key_set: HashSet<PathBuf> = current_keys.iter().cloned().collect();
            if !current_key_set.is_subset(&locked_keys) {
                drop(disk_locks);
                candidate_keys.extend(current_key_set);
                candidate_keys.sort();
                candidate_keys.dedup();
                continue;
            }

            for key in &current_keys {
                self.load_from_disk_if_needed_locked(session, key)?;
            }

            if !self.has_in_memory_entries(session) {
                self.load_latest_operation_from_db_or_log(session);
            }

            let Some(op_id) = self.latest_operation_id_from_memory(session) else {
                return Err(AftError::NoUndoHistory {
                    path: "operation".to_string(),
                });
            };

            let keys_to_restore = self.operation_keys_for_top_op(session, &op_id);
            if keys_to_restore.is_empty() {
                return Err(AftError::NoUndoHistory {
                    path: "operation".to_string(),
                });
            }
            if !keys_to_restore.iter().all(|key| locked_keys.contains(key)) {
                drop(disk_locks);
                candidate_keys.extend(keys_to_restore);
                candidate_keys.sort();
                candidate_keys.dedup();
                continue;
            }

            let mut content_targets = Vec::new();
            let mut tombstone_targets = Vec::new();
            for key in &keys_to_restore {
                let entry = self
                    .entries
                    .get(session)
                    .and_then(|files| files.get(key))
                    .and_then(|stack| stack.last())
                    .cloned()
                    .ok_or_else(|| AftError::NoUndoHistory {
                        path: key.display().to_string(),
                    })?;
                match entry.kind {
                    BackupEntryKind::Content | BackupEntryKind::Symlink => {
                        let existing_state = capture_path_state(key)?;
                        let warning = self.check_external_modification(session, key, key);
                        content_targets.push((key.clone(), entry, warning, existing_state));
                    }
                    BackupEntryKind::Tombstone => {
                        let existing_state = capture_path_state(key)?;
                        tombstone_targets.push((key.clone(), entry, existing_state));
                    }
                }
            }

            let mut created_dirs = Vec::new();
            for (key, _, _, _) in &content_targets {
                if let Some(parent) = key.parent() {
                    if !parent.as_os_str().is_empty() {
                        let missing_dirs = missing_parent_dirs(parent);
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            let mut dirs_to_remove = created_dirs;
                            dirs_to_remove.extend(missing_dirs);
                            let rollback_ok = rollback_created_dirs(&dirs_to_remove);
                            return Err(AftError::IoError {
                                path: parent.display().to_string(),
                                message: format!(
                                    "{}; restore_last_operation aborted; partial_rollback: {}; rollback_succeeded: {}",
                                    e,
                                    !rollback_ok,
                                    rollback_ok
                                ),
                            });
                        }
                        created_dirs.extend(missing_dirs);
                    }
                }
            }

            let mut written = Vec::new();
            for (key, entry, _, existing_state) in &content_targets {
                if let Err(e) = restore_entry_to_path(key, entry) {
                    let files_rollback_ok =
                        rollback_transactional_restore(&written, Some((key, existing_state)));
                    let dirs_rollback_ok = rollback_created_dirs(&created_dirs);
                    let rollback_ok = files_rollback_ok && dirs_rollback_ok;
                    return Err(AftError::IoError {
                        path: key.display().to_string(),
                        message: format!(
                            "{}; restore_last_operation aborted; partial_rollback: {}; rollback_succeeded: {}",
                            e,
                            !rollback_ok,
                            rollback_ok
                        ),
                    });
                }
                written.push((key.clone(), existing_state.clone()));
            }

            let mut deleted_tombstones = Vec::new();
            for (key, _, existing_state) in &tombstone_targets {
                match remove_tombstone_path(key) {
                    Ok(()) => deleted_tombstones.push((key.clone(), existing_state.clone())),
                    Err(e) => {
                        let files_rollback_ok = rollback_transactional_restore(&written, None);
                        let tombstone_rollback_ok =
                            rollback_deleted_tombstones(&deleted_tombstones);
                        let dirs_rollback_ok = rollback_created_dirs(&created_dirs);
                        let rollback_ok =
                            files_rollback_ok && tombstone_rollback_ok && dirs_rollback_ok;
                        return Err(AftError::IoError {
                            path: key.display().to_string(),
                            message: format!(
                                "{}; restore_last_operation aborted; partial_rollback: {}; rollback_succeeded: {}",
                                e,
                                !rollback_ok,
                                rollback_ok
                            ),
                        });
                    }
                }
            }
            let tombstone_created_dirs = tombstone_targets
                .iter()
                .flat_map(|(_, entry, _)| entry.created_dirs.iter().cloned())
                .collect::<Vec<_>>();
            remove_created_dirs_best_effort(&tombstone_created_dirs);

            let mut restored = Vec::new();
            let mut warnings = self
                .skipped_backups
                .get(session)
                .into_iter()
                .flatten()
                .filter(|skip| skip.op_id.as_deref() == Some(op_id.as_str()))
                .map(|skip| {
                    format!(
                        "{}: undo unavailable because backup was skipped ({})",
                        skip.path.display(),
                        skip.reason.as_str()
                    )
                })
                .collect::<Vec<_>>();
            for (key, entry, warning, _) in content_targets {
                self.commit_restored_backup_locked(session, &key)?;
                if let Some(warning) = warning {
                    warnings.push(format!("{}: {}", key.display(), warning));
                }
                restored.push(RestoredFile {
                    path: key,
                    backup_id: entry.backup_id,
                });
            }
            for (key, entry, _) in tombstone_targets {
                self.commit_restored_backup_locked(session, &key)?;
                restored.push(RestoredFile {
                    path: key,
                    backup_id: entry.backup_id,
                });
            }
            if let Some(skips) = self.skipped_backups.get_mut(session) {
                skips.retain(|skip| skip.op_id.as_deref() != Some(op_id.as_str()));
                if skips.is_empty() {
                    self.skipped_backups.remove(session);
                }
            }
            self.touch_session(session);
            drop(disk_locks);

            return Ok(RestoredOperation {
                op_id,
                restored,
                warnings,
            });
        }

        Err(AftError::IoError {
            path: "operation".to_string(),
            message: "backup stack changing under concurrent activity; retry".to_string(),
        })
    }

    /// Pop the most recent backup for `(session, path)` and restore the file.
    /// Returns `(entry, optional_warning)`.
    pub fn restore_latest(
        &mut self,
        session: &str,
        path: &Path,
    ) -> Result<(BackupEntry, Option<String>), AftError> {
        self.run_process_maintenance_once();
        let key = canonicalize_key(path);
        let _disk_lock = self.acquire_stack_disk_lock(session, &key)?;

        match self.read_stack_from_disk_unlocked(session, &key) {
            Ok(Some(entries)) if !entries.is_empty() => {
                self.update_counter_from_entries(&entries);
                self.entries
                    .entry(session.to_string())
                    .or_default()
                    .insert(key.to_path_buf(), entries);
            }
            Ok(_) => {
                if self.session_dir(session).is_some() {
                    self.restore_in_memory_stack(session, &key, None);
                }
            }
            Err(error) => {
                return Err(AftError::IoError {
                    path: key.display().to_string(),
                    message: error,
                });
            }
        }

        if self
            .entries
            .get(session)
            .and_then(|s| s.get(&key))
            .is_none_or(|s| s.is_empty())
        {
            match self.load_from_db_if_present(session, &key) {
                Some(Ok(true)) => {}
                Some(Ok(false)) => {
                    crate::slog_info!(
                        "backup DB miss for session {} path {}; disk meta is authoritative",
                        session,
                        key.display()
                    );
                }
                Some(Err(error)) => {
                    crate::slog_warn!(
                        "backup DB lookup failed for session {} path {}: {}",
                        session,
                        key.display(),
                        error
                    );
                }
                None => {
                    crate::slog_info!(
                        "backup DB unavailable for session {} path {}",
                        session,
                        key.display()
                    );
                }
            }
        }

        // Try memory first
        let in_memory = self
            .entries
            .get(session)
            .and_then(|s| s.get(&key))
            .map_or(false, |s| !s.is_empty());
        if in_memory {
            let warning = self.check_external_modification(session, &key, path);
            let result = self
                .do_restore_locked(session, &key, path)
                .map(|(entry, _)| (entry, warning));
            if result.is_ok() {
                self.touch_session(session);
            }
            return result;
        }

        Err(AftError::NoUndoHistory {
            path: path.display().to_string(),
        })
    }

    /// Return the backup history for `(session, path)` (oldest first).
    pub fn history(&self, session: &str, path: &Path) -> Vec<BackupEntry> {
        let key = canonicalize_key(path);
        let _disk_lock = match self.acquire_stack_disk_lock(session, &key) {
            Ok(lock) => lock,
            Err(error) => {
                crate::slog_warn!(
                    "backup disk read lock failed for {}: {}",
                    key.display(),
                    error
                );
                return Vec::new();
            }
        };

        match self.read_stack_from_disk_unlocked(session, &key) {
            Ok(Some(stack)) if !stack.is_empty() => return stack,
            Ok(_) => {}
            Err(error) => {
                crate::slog_warn!("backup disk read failed for {}: {}", key.display(), error);
                return Vec::new();
            }
        }

        if let Some(stack) = self.entries.get(session).and_then(|s| s.get(&key)).cloned() {
            if !stack.is_empty() {
                return stack;
            }
        }

        match self.read_stack_from_db(session, &key) {
            Some(Ok(stack)) if !stack.is_empty() => stack,
            Some(Ok(_)) => Vec::new(),
            Some(Err(error)) => {
                crate::slog_warn!(
                    "backup history DB lookup failed for session {} path {}: {}",
                    session,
                    key.display(),
                    error
                );
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Return the number of on-disk backup entries for `(session, file)`.
    pub fn disk_history_count(&self, session: &str, path: &Path) -> usize {
        let key = canonicalize_key(path);
        self.disk_index
            .get(session)
            .and_then(|s| s.get(&key))
            .map(|m| m.count)
            .unwrap_or(0)
    }

    /// Return all files that have at least one backup entry in this session
    /// (memory + disk). Other sessions' files are not visible.
    pub fn tracked_files(&self, session: &str) -> Vec<PathBuf> {
        let mut files: std::collections::HashSet<PathBuf> = self
            .entries
            .get(session)
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(disk) = self.disk_index.get(session) {
            for key in disk.keys() {
                files.insert(key.clone());
            }
        }
        files.into_iter().collect()
    }

    /// Preview the file path that `restore_latest` would write for `(session, path)`.
    ///
    /// This is intentionally read-only: it inspects DB/disk/in-memory backup metadata
    /// without popping the undo stack or writing restored file contents.
    pub fn preview_latest_path(&self, session: &str, path: &Path) -> Result<PathBuf, AftError> {
        let key = canonicalize_key(path);
        if self.latest_head_for_key(session, &key).is_some() {
            Ok(key)
        } else {
            Err(AftError::NoUndoHistory {
                path: path.display().to_string(),
            })
        }
    }

    /// Preview the paths that `restore_last_operation` would touch for `session`.
    ///
    /// This mirrors the operation selection logic used by restore, but only reads
    /// backup metadata. It includes tombstone targets because undoing a create
    /// operation deletes those paths and therefore still requires write permission.
    pub fn preview_last_operation_paths(&self, session: &str) -> Result<Vec<PathBuf>, AftError> {
        let mut heads_by_path: HashMap<PathBuf, BackupEntryHead> = self
            .entries
            .get(session)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|(key, stack)| {
                        stack
                            .last()
                            .map(|entry| (key.clone(), BackupEntryHead::from_entry(entry)))
                    })
                    .collect()
            })
            .unwrap_or_default();

        match self.read_latest_operation_heads_from_db(session) {
            Some(Ok(db_heads)) if !db_heads.is_empty() => {
                for (key, head) in db_heads {
                    heads_by_path.insert(key, head);
                }
                self.merge_disk_stack_heads(session, &mut heads_by_path);
            }
            Some(Ok(_)) => {
                crate::slog_info!(
                    "backup latest operation preview DB miss for session {}; falling back to disk",
                    session
                );
                self.merge_disk_stack_heads(session, &mut heads_by_path);
            }
            Some(Err(error)) => {
                crate::slog_warn!(
                    "backup latest operation preview DB lookup failed for session {}; falling back to disk: {}",
                    session,
                    error
                );
                self.merge_disk_stack_heads(session, &mut heads_by_path);
            }
            None => {
                crate::slog_info!(
                    "backup latest operation preview DB unavailable for session {}; falling back to disk",
                    session
                );
                self.merge_disk_stack_heads(session, &mut heads_by_path);
            }
        }

        let mut latest: Option<(u128, String)> = None;
        for head in heads_by_path.values() {
            if let Some(op_id) = &head.op_id {
                if latest
                    .as_ref()
                    .map_or(true, |(latest_order, _)| head.order > *latest_order)
                {
                    latest = Some((head.order, op_id.clone()));
                }
            }
        }

        let Some((_, op_id)) = latest else {
            return Err(AftError::NoUndoHistory {
                path: "operation".to_string(),
            });
        };

        let mut paths: Vec<PathBuf> = heads_by_path
            .into_iter()
            .filter_map(|(key, head)| {
                (head.op_id.as_deref() == Some(op_id.as_str())).then_some(key)
            })
            .collect();
        paths.sort();

        if paths.is_empty() {
            Err(AftError::NoUndoHistory {
                path: "operation".to_string(),
            })
        } else {
            Ok(paths)
        }
    }

    /// Return all session namespaces that currently have any backup state
    /// (memory or disk). Exposed for `/aft-status` aggregate reporting.
    pub fn sessions_with_backups(&self) -> Vec<String> {
        let mut sessions: std::collections::HashSet<String> =
            self.entries.keys().cloned().collect();
        for s in self.disk_index.keys() {
            sessions.insert(s.clone());
        }
        sessions.into_iter().collect()
    }

    /// Total on-disk bytes across all sessions (best-effort, reads metadata only).
    /// Used by `/aft-status` to surface storage footprint.
    pub fn total_disk_bytes(&self) -> u64 {
        let mut total = 0u64;
        for session_dirs in self.disk_index.values() {
            for meta in session_dirs.values() {
                if let Ok(read_dir) = std::fs::read_dir(&meta.dir) {
                    for entry in read_dir.flatten() {
                        if let Ok(m) = entry.metadata() {
                            if m.is_file() {
                                total += m.len();
                            }
                        }
                    }
                }
            }
        }
        total
    }

    fn next_id_and_order(&self) -> (String, u128) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let order = ((current_timestamp_nanos() as u128) << 32) | u128::from(n);
        (format!("backup-{}", n), order)
    }

    fn db_pool_and_harness(&self) -> Option<(Arc<Mutex<TrackedConnection>>, String)> {
        let pool = self.db_pool.read().ok().and_then(|slot| slot.clone())?;
        let harness = self.db_harness.read().ok().and_then(|slot| slot.clone())?;
        Some((pool, harness))
    }

    fn clear_db_mirror_sync(&self) {
        if let Ok(mut synced) = self.db_mirrored_stacks.write() {
            synced.clear();
        }
    }

    fn db_mirror_is_synced(&self, session: &str, key: &Path) -> bool {
        self.db_mirrored_stacks
            .read()
            .is_ok_and(|synced| synced.get(session).is_some_and(|keys| keys.contains(key)))
    }

    fn set_db_mirror_synced(&self, session: &str, key: &Path, is_synced: bool) {
        if let Ok(mut synced) = self.db_mirrored_stacks.write() {
            if is_synced {
                synced
                    .entry(session.to_string())
                    .or_default()
                    .insert(key.to_path_buf());
            } else if let Some(keys) = synced.get_mut(session) {
                keys.remove(key);
                if keys.is_empty() {
                    synced.remove(session);
                }
            }
        }
    }

    fn latest_head_for_key(&self, session: &str, key: &Path) -> Option<BackupEntryHead> {
        self.entries
            .get(session)
            .and_then(|files| files.get(key))
            .and_then(|stack| stack.last())
            .map(BackupEntryHead::from_entry)
            .or_else(|| {
                self.read_stack_heads_from_disk(session, key)
                    .and_then(|stack| stack.last().cloned())
            })
            .or_else(|| match self.read_stack_heads_from_db(session, key) {
                Some(Ok(stack)) if !stack.is_empty() => stack.last().cloned(),
                Some(Err(error)) => {
                    crate::slog_warn!(
                        "backup preview DB lookup failed for session {} path {}: {}",
                        session,
                        key.display(),
                        error
                    );
                    None
                }
                _ => None,
            })
    }

    fn merge_disk_stack_heads(
        &self,
        session: &str,
        heads_by_path: &mut HashMap<PathBuf, BackupEntryHead>,
    ) {
        let disk_keys: Vec<PathBuf> = self
            .disk_index
            .get(session)
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default();
        for key in disk_keys {
            if let Some(head) = self
                .read_stack_heads_from_disk(session, &key)
                .and_then(|stack| stack.last().cloned())
            {
                heads_by_path.insert(key, head);
            }
        }
    }

    fn read_stack_heads_from_db(
        &self,
        session: &str,
        key: &Path,
    ) -> Option<Result<Vec<BackupEntryHead>, String>> {
        let (pool, harness) = self.db_pool_and_harness()?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let path_hash = Self::path_hash(key);
        Some(
            crate::db::backups::list_backups(&conn, &harness, session, &path_hash)
                .map_err(|error| error.to_string())
                .map(|rows| {
                    rows.iter()
                        .map(BackupEntryHead::from_row)
                        .collect::<Vec<_>>()
                }),
        )
    }

    fn read_latest_operation_heads_from_db(
        &self,
        session: &str,
    ) -> Option<Result<HashMap<PathBuf, BackupEntryHead>, String>> {
        let (pool, harness) = self.db_pool_and_harness()?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let latest = match crate::db::backups::get_latest_operation_backup(&conn, &harness, session)
        {
            Ok(Some(row)) => row,
            Ok(None) => return Some(Ok(HashMap::new())),
            Err(error) => return Some(Err(error.to_string())),
        };
        let Some(op_id) = latest.op_id else {
            return Some(Ok(HashMap::new()));
        };
        let rows = match crate::db::backups::list_backups_by_op(&conn, &harness, session, &op_id) {
            Ok(rows) => rows,
            Err(error) => return Some(Err(error.to_string())),
        };
        if rows.is_empty() {
            return Some(Ok(HashMap::new()));
        }
        let path_hashes: std::collections::HashSet<String> =
            rows.into_iter().map(|row| row.path_hash).collect();
        drop(conn);

        let mut heads = HashMap::new();
        for path_hash in path_hashes {
            let conn = match pool.lock() {
                Ok(conn) => conn,
                Err(_) => return Some(Err("db mutex poisoned".to_string())),
            };
            let rows = match crate::db::backups::list_backups(&conn, &harness, session, &path_hash)
            {
                Ok(rows) => rows,
                Err(error) => return Some(Err(error.to_string())),
            };
            drop(conn);

            let Some(file_path) = rows.first().map(|row| row.file_path.clone()) else {
                continue;
            };
            let Some(head) = rows.last().map(BackupEntryHead::from_row) else {
                continue;
            };
            heads.insert(PathBuf::from(file_path), head);
        }

        Some(Ok(heads))
    }

    fn read_stack_from_db(
        &self,
        session: &str,
        key: &Path,
    ) -> Option<Result<Vec<BackupEntry>, String>> {
        let (pool, harness) = self.db_pool_and_harness()?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let path_hash = Self::path_hash(key);
        Some(
            crate::db::backups::list_backups(&conn, &harness, session, &path_hash)
                .map_err(|error| error.to_string())
                .and_then(|rows| {
                    rows.into_iter()
                        .map(|row| self.backup_entry_from_db_row(row))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| error.to_string())
                }),
        )
    }

    fn load_from_db_if_present(
        &mut self,
        session: &str,
        key: &Path,
    ) -> Option<Result<bool, String>> {
        match self.read_stack_from_db(session, key) {
            Some(Ok(stack)) if !stack.is_empty() => {
                self.update_counter_from_entries(&stack);
                self.entries
                    .entry(session.to_string())
                    .or_default()
                    .insert(key.to_path_buf(), stack);
                Some(Ok(true))
            }
            Some(Ok(_)) => Some(Ok(false)),
            Some(Err(error)) => Some(Err(error)),
            None => None,
        }
    }

    fn load_latest_operation_from_db(&mut self, session: &str) -> Option<Result<bool, String>> {
        let (pool, harness) = self.db_pool_and_harness()?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let latest = match crate::db::backups::get_latest_operation_backup(&conn, &harness, session)
        {
            Ok(Some(row)) => row,
            Ok(None) => return Some(Ok(false)),
            Err(error) => return Some(Err(error.to_string())),
        };
        let Some(op_id) = latest.op_id else {
            return Some(Ok(false));
        };
        let rows = match crate::db::backups::list_backups_by_op(&conn, &harness, session, &op_id) {
            Ok(rows) => rows,
            Err(error) => return Some(Err(error.to_string())),
        };
        if rows.is_empty() {
            return Some(Ok(false));
        }
        let path_hashes: std::collections::HashSet<String> =
            rows.into_iter().map(|row| row.path_hash).collect();
        drop(conn);

        let mut loaded_any = false;
        for path_hash in path_hashes {
            let conn = match pool.lock() {
                Ok(conn) => conn,
                Err(_) => return Some(Err("db mutex poisoned".to_string())),
            };
            let loaded =
                match crate::db::backups::list_backups(&conn, &harness, session, &path_hash) {
                    Ok(rows) => {
                        let file_path = rows.first().map(|row| row.file_path.clone());
                        rows.into_iter()
                            .map(|row| self.backup_entry_from_db_row(row))
                            .collect::<Result<Vec<_>, _>>()
                            .map(|stack| (file_path, stack))
                            .map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error.to_string()),
                };
            drop(conn);
            let (file_path, stack) = match loaded {
                Ok((file_path, stack)) if !stack.is_empty() => (file_path, stack),
                Ok(_) => continue,
                Err(error) => return Some(Err(error)),
            };
            let Some(file_path) = file_path else {
                return Some(Err(format!(
                    "backup DB rows for path hash {path_hash} have no file path"
                )));
            };
            let key = PathBuf::from(file_path);
            self.update_counter_from_entries(&stack);
            self.entries
                .entry(session.to_string())
                .or_default()
                .insert(key, stack);
            loaded_any = true;
        }

        Some(Ok(loaded_any))
    }

    fn update_counter_from_entries(&self, entries: &[BackupEntry]) {
        if let Some(next_counter) = entries
            .iter()
            .filter_map(|entry| backup_sequence(&entry.backup_id))
            .max()
            .and_then(|max| max.checked_add(1))
        {
            let _ = self
                .counter
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    (current < next_counter).then_some(next_counter)
                });
        }
    }

    fn persist_new_entry_locked(
        &mut self,
        session: &str,
        key: &Path,
        entry: BackupEntry,
    ) -> Result<(), AftError> {
        let max_depth = self.policy.max_depth;
        // Detach the stack so persistence can borrow it while updating the rest of
        // the store. Keep evicted entries by ownership until the disk commit, which
        // lets a failed write rebuild the original order without cloning file data.
        let mut stack = self
            .entries
            .get_mut(session)
            .and_then(|files| files.remove(key))
            .unwrap_or_default();
        let mut evicted = drain_stack_to_depth(&mut stack, max_depth.saturating_sub(1));
        if max_depth > 0 {
            stack.push(entry);
        }

        // The append path knows its exact database delta without inspecting or
        // rebuilding retained history: remove evicted orders and add the one new
        // entry. Disk persistence still receives the complete stack unchanged.
        let evicted_orders = evicted.iter().map(|entry| entry.order).collect::<Vec<_>>();
        let new_entry = (max_depth > 0).then(|| stack.last().expect("new backup entry"));
        if let Err(error) = self.write_appended_snapshot_to_disk_locked(
            session,
            key,
            &stack,
            &evicted_orders,
            new_entry,
        ) {
            if max_depth > 0 {
                stack.pop();
            }
            evicted.append(&mut stack);
            self.restore_in_memory_stack(session, key, Some(evicted));
            return Err(error);
        }

        self.entries
            .entry(session.to_string())
            .or_default()
            .insert(key.to_path_buf(), stack);
        Ok(())
    }

    fn restore_in_memory_stack(
        &mut self,
        session: &str,
        key: &Path,
        stack: Option<Vec<BackupEntry>>,
    ) {
        match stack {
            Some(stack) if !stack.is_empty() => {
                self.entries
                    .entry(session.to_string())
                    .or_default()
                    .insert(key.to_path_buf(), stack);
            }
            _ => {
                if let Some(files) = self.entries.get_mut(session) {
                    files.remove(key);
                    if files.is_empty() {
                        self.entries.remove(session);
                    }
                }
            }
        }
    }

    fn has_in_memory_entries(&self, session: &str) -> bool {
        self.entries
            .get(session)
            .is_some_and(|files| files.values().any(|stack| !stack.is_empty()))
    }

    fn latest_operation_id_from_memory(&self, session: &str) -> Option<String> {
        let mut latest: Option<(u128, String)> = None;
        if let Some(files) = self.entries.get(session) {
            for stack in files.values() {
                if let Some(entry) = stack.last() {
                    if let Some(op_id) = &entry.op_id {
                        if latest
                            .as_ref()
                            .is_none_or(|(latest_order, _)| entry.order > *latest_order)
                        {
                            latest = Some((entry.order, op_id.clone()));
                        }
                    }
                }
            }
        }
        latest.map(|(_, op_id)| op_id)
    }

    fn operation_keys_for_top_op(&self, session: &str, op_id: &str) -> Vec<PathBuf> {
        let mut keys: Vec<PathBuf> = self
            .entries
            .get(session)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|(key, stack)| {
                        stack.last().and_then(|entry| {
                            (entry.op_id.as_deref() == Some(op_id)).then(|| key.clone())
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        keys.sort();
        keys
    }

    fn load_latest_operation_from_db_or_log(&mut self, session: &str) {
        match self.load_latest_operation_from_db(session) {
            Some(Ok(true)) => {}
            Some(Ok(false)) => {
                crate::slog_info!(
                    "backup latest operation DB miss for session {}; disk meta is authoritative",
                    session
                );
            }
            Some(Err(error)) => {
                crate::slog_warn!(
                    "backup latest operation DB lookup failed for session {}: {}",
                    session,
                    error
                );
            }
            None => {
                crate::slog_info!(
                    "backup latest operation DB unavailable for session {}",
                    session
                );
            }
        }
    }

    fn resolve_db_backup_row_path(&self, mut row: BackupRow) -> BackupRow {
        if let Some(backup_path) = row.backup_path.clone() {
            let path = PathBuf::from(&backup_path);
            if path.is_relative() {
                if let Some(session_dir) = self.session_dir(&row.session_id) {
                    row.backup_path = Some(
                        session_dir
                            .join(&row.path_hash)
                            .join(path)
                            .display()
                            .to_string(),
                    );
                }
            }
        }
        row
    }

    fn backup_entry_from_db_row(&self, row: BackupRow) -> Result<BackupEntry, std::io::Error> {
        BackupEntry::try_from(self.resolve_db_backup_row_path(row))
    }

    pub fn discard_operation_entries(&mut self, session: &str, op_id: &str) {
        if let Some(skips) = self.skipped_backups.get_mut(session) {
            skips.retain(|skip| skip.op_id.as_deref() != Some(op_id));
            if skips.is_empty() {
                self.skipped_backups.remove(session);
            }
        }
        let keys: Vec<PathBuf> = self
            .entries
            .get(session)
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default();

        for key in keys {
            let mut remove_key = false;
            let mut remaining_stack = None;
            if let Some(session_entries) = self.entries.get_mut(session) {
                if let Some(stack) = session_entries.get_mut(&key) {
                    while stack
                        .last()
                        .is_some_and(|entry| entry.op_id.as_deref() == Some(op_id))
                    {
                        stack.pop();
                    }
                    if stack.is_empty() {
                        remove_key = true;
                    } else {
                        remaining_stack = Some(stack.clone());
                    }
                }
                if remove_key {
                    session_entries.remove(&key);
                }
            }

            if remove_key {
                if let Err(error) = self.remove_disk_backups(session, &key) {
                    crate::slog_warn!(
                        "failed to remove backup stack for {} during operation discard: {}",
                        key.display(),
                        error
                    );
                }
            } else if let Some(stack) = remaining_stack {
                if let Err(error) = self.write_snapshot_to_disk(session, &key, &stack) {
                    crate::slog_warn!(
                        "failed to persist backup stack for {} during operation discard: {}",
                        key.display(),
                        error
                    );
                }
            }
        }

        if self
            .entries
            .get(session)
            .is_some_and(|session_entries| session_entries.is_empty())
        {
            self.entries.remove(session);
        }
    }

    pub(crate) fn discard_latest_operation_entry_for_path(
        &mut self,
        session: &str,
        op_id: &str,
        path: &Path,
    ) {
        let key = canonicalize_key(path);
        if let Some(skips) = self.skipped_backups.get_mut(session) {
            skips.retain(|skip| skip.op_id.as_deref() != Some(op_id) || skip.path != key);
            if skips.is_empty() {
                self.skipped_backups.remove(session);
            }
        }
        let mut remove_key = false;
        let mut remaining_stack = None;

        if let Some(session_entries) = self.entries.get_mut(session) {
            if let Some(stack) = session_entries.get_mut(&key) {
                if stack
                    .last()
                    .is_some_and(|entry| entry.op_id.as_deref() == Some(op_id))
                {
                    stack.pop();
                    if stack.is_empty() {
                        remove_key = true;
                    } else {
                        remaining_stack = Some(stack.clone());
                    }
                }
            }
            if remove_key {
                session_entries.remove(&key);
            }
        }

        if remove_key {
            if let Err(error) = self.remove_disk_backups(session, &key) {
                crate::slog_warn!(
                    "failed to remove backup stack for {} during single-entry discard: {}",
                    key.display(),
                    error
                );
            }
        } else if let Some(stack) = remaining_stack {
            if let Err(error) = self.write_snapshot_to_disk(session, &key, &stack) {
                crate::slog_warn!(
                    "failed to persist backup stack for {} during single-entry discard: {}",
                    key.display(),
                    error
                );
            }
        }

        if self
            .entries
            .get(session)
            .is_some_and(|session_entries| session_entries.is_empty())
        {
            self.entries.remove(session);
        }
    }

    fn touch_session(&mut self, session: &str) {
        let now = current_timestamp();
        self.session_meta
            .entry(session.to_string())
            .or_default()
            .last_accessed = now;
        self.write_session_marker(session, now);
    }

    // ---- Internal helpers ----

    fn do_restore_locked(
        &mut self,
        session: &str,
        key: &Path,
        path: &Path,
    ) -> Result<(BackupEntry, Option<String>), AftError> {
        let session_entries =
            self.entries
                .get_mut(session)
                .ok_or_else(|| AftError::NoUndoHistory {
                    path: path.display().to_string(),
                })?;
        let stack = session_entries
            .get_mut(key)
            .ok_or_else(|| AftError::NoUndoHistory {
                path: path.display().to_string(),
            })?;

        let entry = stack
            .last()
            .cloned()
            .ok_or_else(|| AftError::NoUndoHistory {
                path: path.display().to_string(),
            })?;

        match entry.kind {
            BackupEntryKind::Content | BackupEntryKind::Symlink => {
                restore_entry_to_path(path, &entry).map_err(|e| AftError::IoError {
                    path: path.display().to_string(),
                    message: e.to_string(),
                })?;
            }
            BackupEntryKind::Tombstone => {
                remove_tombstone_path(path).map_err(|e| AftError::IoError {
                    path: path.display().to_string(),
                    message: e.to_string(),
                })?;
                remove_created_dirs_best_effort(&entry.created_dirs);
            }
        }

        stack.pop();
        if stack.is_empty() {
            session_entries.remove(key);
            // Also prune the session map when its last file is gone.
            if session_entries.is_empty() {
                self.entries.remove(session);
            }
            self.remove_disk_backups_locked(session, key)?;
        } else {
            let stack_clone = self
                .entries
                .get(session)
                .and_then(|s| s.get(key))
                .cloned()
                .unwrap_or_default();
            self.write_snapshot_to_disk_locked(session, key, &stack_clone)?;
        }

        Ok((entry, None))
    }

    fn commit_restored_backup_locked(&mut self, session: &str, key: &Path) -> Result<(), AftError> {
        let mut remove_key = false;
        let mut remove_session = false;
        let mut remaining_stack = None;

        if let Some(session_entries) = self.entries.get_mut(session) {
            if let Some(stack) = session_entries.get_mut(key) {
                stack.pop();
                if stack.is_empty() {
                    remove_key = true;
                } else {
                    remaining_stack = Some(stack.clone());
                }
            }

            if remove_key {
                session_entries.remove(key);
                remove_session = session_entries.is_empty();
            }
        }

        if remove_session {
            self.entries.remove(session);
        }

        if remove_key {
            self.remove_disk_backups_locked(session, key)?;
        } else if let Some(stack) = remaining_stack {
            self.write_snapshot_to_disk_locked(session, key, &stack)?;
        }

        Ok(())
    }

    fn check_external_modification(
        &self,
        session: &str,
        key: &Path,
        path: &Path,
    ) -> Option<String> {
        let stack = self.entries.get(session).and_then(|s| s.get(key))?;
        let latest = stack.last()?;
        let modified = match latest.kind {
            BackupEntryKind::Content => std::fs::read(path)
                .map(|current| current.as_slice() != latest.content_bytes.as_ref())
                .unwrap_or(true),
            BackupEntryKind::Symlink => std::fs::read_link(path)
                .map(|target| latest.link_target.as_ref() != Some(&target))
                .unwrap_or(true),
            BackupEntryKind::Tombstone => false,
        };
        modified.then(|| "file was modified externally since last backup".to_string())
    }

    // ---- Disk persistence ----

    fn backups_dir(&self) -> Option<PathBuf> {
        self.storage_dir
            .as_ref()
            .map(|dir| match &self.storage_harness {
                Some(harness) => dir.join(harness).join("backups"),
                None => dir.join("backups"),
            })
    }

    fn session_dir(&self, session: &str) -> Option<PathBuf> {
        self.backups_dir()
            .map(|d| d.join(Self::session_hash(session)))
    }

    fn session_hash(session: &str) -> String {
        hash_session(session)
    }

    fn path_hash(key: &Path) -> String {
        // v0.16.0 intentionally switched from DefaultHasher to SHA-256 for
        // stable on-disk names. Existing DefaultHasher backup directories are
        // not migrated: backups are short-lived/session-scoped, so one-time
        // loss of pre-upgrade undo history is acceptable.
        stable_hash_16(key.to_string_lossy().as_bytes())
    }

    fn write_session_marker(&self, session: &str, last_accessed: u64) {
        let Some(session_dir) = self.session_dir(session) else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            crate::slog_warn!("failed to create session dir: {}", e);
            return;
        }
        let marker = session_dir.join("session.json");
        let json = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": session,
            "last_accessed": last_accessed,
        });
        if let Ok(s) = serde_json::to_string_pretty(&json) {
            let tmp = session_dir.join("session.json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, marker);
            }
        }
    }

    fn repair_root_backups_if_needed(&self) {
        let (Some(storage_dir), Some(harness)) = (&self.storage_dir, &self.storage_harness) else {
            return;
        };
        let root_backups = storage_dir.join("backups");
        if !dir_has_entries(&root_backups) {
            return;
        }
        let harness_backups = storage_dir.join(harness).join("backups");
        if dir_has_entries(&harness_backups) {
            return;
        }
        if let Some(parent) = harness_backups.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                crate::slog_warn!(
                    "failed to create harness backup dir {}: {}",
                    parent.display(),
                    error
                );
                return;
            }
        }
        if harness_backups.exists() {
            let _ = std::fs::remove_dir(&harness_backups);
        }
        match std::fs::rename(&root_backups, &harness_backups) {
            Ok(()) => {
                crate::slog_info!(
                    "moved legacy root backups into harness namespace: {}",
                    harness_backups.display()
                );
            }
            Err(error) => {
                crate::slog_warn!(
                    "failed to move legacy root backups into {}: {}; trying child merge",
                    harness_backups.display(),
                    error
                );
                if std::fs::create_dir_all(&harness_backups).is_err() {
                    return;
                }
                if let Ok(entries) = std::fs::read_dir(&root_backups) {
                    for entry in entries.flatten() {
                        let source = entry.path();
                        let target = harness_backups.join(entry.file_name());
                        if !target.exists() {
                            let _ = std::fs::rename(source, target);
                        }
                    }
                }
                let _ = std::fs::remove_dir(&root_backups);
            }
        }
    }

    fn gc_stale_sessions(&mut self, ttl_hours: u32) {
        let backups_dir = match self.backups_dir() {
            Some(d) if d.exists() => d,
            _ => return,
        };
        let ttl_secs = u64::from(if ttl_hours == 0 { 72 } else { ttl_hours }) * 60 * 60;
        let cutoff = current_timestamp().saturating_sub(ttl_secs);
        let entries = match std::fs::read_dir(&backups_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let session_dir = entry.path();
            if !session_dir.is_dir() || session_dir.join("meta.json").exists() {
                continue;
            }
            let Some(last_accessed) = Self::read_session_last_accessed(&session_dir) else {
                continue;
            };
            if last_accessed >= cutoff {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&session_dir) {
                crate::slog_warn!(
                    "failed to remove stale backup session {}: {}",
                    session_dir.display(),
                    e
                );
            } else {
                crate::slog_warn!(
                    "removed stale backup session {} (last_accessed={})",
                    session_dir.display(),
                    last_accessed
                );
            }
        }
    }

    /// One-time migration: move pre-session flat layout into the default
    /// session namespace. Called from `set_storage_dir` so existing backups
    /// survive the upgrade.
    ///
    /// Detection: any directory directly under `backups/` that contains a
    /// `meta.json` (as opposed to a `session.json` marker or subdirectories)
    /// is treated as a legacy entry.
    fn migrate_legacy_layout_if_needed(&mut self) {
        let backups_dir = match self.backups_dir() {
            Some(d) if d.exists() => d,
            _ => return,
        };
        let default_session_dir =
            backups_dir.join(Self::session_hash(crate::protocol::DEFAULT_SESSION_ID));

        let entries = match std::fs::read_dir(&backups_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut migrated = 0usize;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            // Skip non-directories and already-sessionized layouts.
            if !entry_path.is_dir() {
                continue;
            }
            if entry_path == default_session_dir {
                continue;
            }
            let meta_path = entry_path.join("meta.json");
            if !meta_path.exists() {
                continue; // Already a session-hash dir (contains per-path subdirs), skip
            }
            // This is a legacy flat-layout path-hash directory. Move it under
            // the default session namespace.
            if let Err(e) = std::fs::create_dir_all(&default_session_dir) {
                crate::slog_warn!("failed to create default session dir: {}", e);
                return;
            }
            let leaf = match entry_path.file_name() {
                Some(n) => n,
                None => continue,
            };
            let target = default_session_dir.join(leaf);
            if target.exists() {
                // Already migrated on a prior run that was interrupted —
                // leave both and let the regular load pick up the target.
                continue;
            }
            match std::fs::rename(&entry_path, &target) {
                Ok(()) => {
                    // Bump meta.json to include session_id + schema_version.
                    Self::upgrade_meta_file(
                        &target.join("meta.json"),
                        crate::protocol::DEFAULT_SESSION_ID,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    crate::slog_warn!(
                        "failed to migrate legacy backup {}: {}",
                        entry_path.display(),
                        e
                    );
                }
            }
        }
        if migrated > 0 {
            crate::slog_info!(
                "migrated {} legacy backup entries into default session namespace",
                migrated
            );
            // Write a session.json marker so future scans don't re-migrate.
            let marker = default_session_dir.join("session.json");
            let json = serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": crate::protocol::DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            });
            if let Ok(s) = serde_json::to_string_pretty(&json) {
                let _ = std::fs::write(&marker, s);
            }
        }
    }

    fn upgrade_meta_file(meta_path: &Path, session_id: &str) {
        let content = match std::fs::read_to_string(meta_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut parsed: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(obj) = parsed.as_object_mut() {
            let count = obj.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            obj.insert(
                "schema_version".to_string(),
                serde_json::json!(SCHEMA_VERSION),
            );
            obj.insert("session_id".to_string(), serde_json::json!(session_id));
            obj.entry("entries").or_insert_with(|| {
                serde_json::Value::Array(
                    (0..count)
                        .map(|i| {
                            serde_json::json!({
                                "backup_id": format!("disk-{}", i),
                                "timestamp": 0,
                                "description": "restored from disk",
                                "op_id": null,
                            })
                        })
                        .collect(),
                )
            });
        }
        if let Ok(s) = serde_json::to_string_pretty(&parsed) {
            let tmp = meta_path.with_extension("json.tmp");
            if std::fs::write(&tmp, &s).is_ok() {
                let _ = std::fs::rename(&tmp, meta_path);
            }
        }
    }

    fn read_session_last_accessed(session_dir: &Path) -> Option<u64> {
        let marker = session_dir.join("session.json");
        let content = std::fs::read_to_string(&marker).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
        parsed.get("last_accessed").and_then(|v| v.as_u64())
    }

    fn prepare_snapshot(
        &mut self,
        session: &str,
        path: &Path,
        op_id: Option<&str>,
        allow_missing: bool,
    ) -> Result<bool, AftError> {
        match self.should_snapshot_path(path, allow_missing)? {
            SnapshotDecision::Capture => Ok(true),
            SnapshotDecision::Skip(reason) => {
                self.record_skipped_backup(session, path, op_id, reason);
                Ok(false)
            }
        }
    }

    fn should_snapshot_path(
        &self,
        path: &Path,
        allow_missing: bool,
    ) -> Result<SnapshotDecision, AftError> {
        if !self.policy.enabled || self.policy.max_file_size == Some(0) {
            return Ok(SnapshotDecision::Skip(BackupSkippedReason::Disabled));
        }
        if self.temp_path_policy_applies() && crate::bash_permissions::is_system_temp_path(path) {
            return Ok(SnapshotDecision::Skip(BackupSkippedReason::TempPath));
        }
        let Some(max_file_size) = self.policy.max_file_size else {
            return Ok(SnapshotDecision::Capture);
        };
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len() > max_file_size => {
                Ok(SnapshotDecision::Skip(BackupSkippedReason::TooLarge))
            }
            Ok(_) => Ok(SnapshotDecision::Capture),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                Ok(SnapshotDecision::Capture)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(AftError::FileNotFound {
                    path: path.display().to_string(),
                })
            }
            Err(error) => Err(AftError::IoError {
                path: path.display().to_string(),
                message: error.to_string(),
            }),
        }
    }

    fn temp_path_policy_applies(&self) -> bool {
        #[cfg(test)]
        if !self.enforce_temp_path_policy {
            return false;
        }
        #[cfg(debug_assertions)]
        {
            // Integration fixtures live under the OS temp directory, so debug test
            // processes may opt back into legacy snapshots while production-path
            // tests explicitly leave the temp-path policy enabled.
            return std::env::var_os("AFT_TEST_ALLOW_TEMP_BACKUPS").as_deref()
                != Some(std::ffi::OsStr::new("1"));
        }
        #[cfg(not(debug_assertions))]
        true
    }

    fn record_skipped_backup(
        &mut self,
        session: &str,
        path: &Path,
        op_id: Option<&str>,
        reason: BackupSkippedReason,
    ) {
        match reason {
            BackupSkippedReason::TooLarge => {
                BACKUP_SKIPPED_TOO_LARGE_TOTAL.fetch_add(1, Ordering::Relaxed);
            }
            BackupSkippedReason::TempPath => {
                BACKUP_SKIPPED_TEMP_PATH_TOTAL.fetch_add(1, Ordering::Relaxed);
            }
            BackupSkippedReason::Disabled => {}
        }
        let (_, order) = self.next_id_and_order();
        self.skipped_backups
            .entry(session.to_string())
            .or_default()
            .push(SkippedBackup {
                path: canonicalize_key(path),
                op_id: op_id.map(str::to_string),
                reason,
                order,
            });
    }

    pub fn skipped_reason_for_operation(
        &self,
        session: &str,
        op_id: &str,
        path: Option<&Path>,
    ) -> Option<BackupSkippedReason> {
        let key = path.map(canonicalize_key);
        self.skipped_backups
            .get(session)?
            .iter()
            .rev()
            .find(|skip| {
                skip.op_id.as_deref() == Some(op_id)
                    && key.as_ref().is_none_or(|key| &skip.path == key)
            })
            .map(|skip| skip.reason)
    }

    pub fn latest_skipped_order(&self, session: &str) -> Option<u128> {
        self.skipped_backups
            .get(session)?
            .iter()
            .map(|skip| skip.order)
            .max()
    }

    pub fn skipped_reason_after(
        &self,
        session: &str,
        order: Option<u128>,
    ) -> Option<BackupSkippedReason> {
        self.skipped_backups
            .get(session)?
            .iter()
            .filter(|skip| order.is_none_or(|order| skip.order > order))
            .max_by_key(|skip| skip.order)
            .map(|skip| skip.reason)
    }

    pub fn latest_skipped_reason_for_undo(
        &self,
        session: &str,
        path: Option<&Path>,
    ) -> Option<BackupSkippedReason> {
        self.latest_skipped_candidate_for_undo(session, path)
            .map(|(_, reason, _)| reason)
    }

    fn latest_skipped_candidate_for_undo(
        &self,
        session: &str,
        path: Option<&Path>,
    ) -> Option<(usize, BackupSkippedReason, Option<String>)> {
        let key = path.map(canonicalize_key);
        let skips = self.skipped_backups.get(session)?;
        let (index, skip) = skips
            .iter()
            .enumerate()
            .filter(|(_, skip)| key.as_ref().is_none_or(|key| &skip.path == key))
            .max_by_key(|(_, skip)| skip.order)?;

        let latest_backup = if let Some(key) = key.as_ref() {
            self.entries
                .get(session)
                .and_then(|files| files.get(key))
                .and_then(|stack| stack.last())
                .map(|entry| (entry.order, entry.op_id.as_deref()))
        } else {
            self.entries.get(session).and_then(|files| {
                files
                    .values()
                    .filter_map(|stack| stack.last())
                    .max_by_key(|entry| entry.order)
                    .map(|entry| (entry.order, entry.op_id.as_deref()))
            })
        };
        if latest_backup.is_some_and(|(order, op_id)| {
            order > skip.order || (op_id.is_some() && op_id == skip.op_id.as_deref())
        }) {
            return None;
        }
        Some((index, skip.reason, skip.op_id.clone()))
    }

    pub fn take_latest_skipped_reason_for_undo(
        &mut self,
        session: &str,
        path: Option<&Path>,
    ) -> Option<BackupSkippedReason> {
        let (index, reason, op_id) = self.latest_skipped_candidate_for_undo(session, path)?;
        let skips = self.skipped_backups.get_mut(session)?;
        if path.is_some() {
            skips.remove(index);
        } else if let Some(op_id) = op_id {
            skips.retain(|skip| skip.op_id.as_deref() != Some(op_id.as_str()));
        } else {
            skips.remove(index);
        }
        if skips.is_empty() {
            self.skipped_backups.remove(session);
        }
        Some(reason)
    }

    fn ensure_session_marker(&self, session_dir: &Path, session: &str) -> Result<(), AftError> {
        let marker = session_dir.join("session.json");
        if marker.exists() {
            return Ok(());
        }
        let json = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": session,
            "last_accessed": current_timestamp(),
        });
        let content = serde_json::to_string_pretty(&json).map_err(|error| AftError::IoError {
            path: marker.display().to_string(),
            message: error.to_string(),
        })?;
        write_temp_fsync_rename(session_dir, "session.json", content.as_bytes()).map_err(
            |error| AftError::IoError {
                path: marker.display().to_string(),
                message: error.to_string(),
            },
        )?;
        let _ = fsync_dir(session_dir);
        Ok(())
    }

    fn acquire_stack_disk_lock(
        &self,
        session: &str,
        key: &Path,
    ) -> Result<Option<crate::fs_lock::LockGuard>, AftError> {
        let Some(session_dir) = self.session_dir(session) else {
            return Ok(None);
        };
        self.record_disk_io_for_tests();
        let lock_dir = session_dir.join(".locks");
        std::fs::create_dir_all(&lock_dir).map_err(|error| AftError::IoError {
            path: lock_dir.display().to_string(),
            message: error.to_string(),
        })?;
        let lock_path = lock_dir.join(format!("{}.lock", Self::path_hash(key)));
        crate::fs_lock::acquire(&lock_path)
            .map(Some)
            .map_err(|error| AftError::IoError {
                path: lock_path.display().to_string(),
                message: error.to_string(),
            })
    }

    fn acquire_stack_disk_locks(
        &self,
        session: &str,
        keys: &[PathBuf],
    ) -> Result<Vec<crate::fs_lock::LockGuard>, AftError> {
        let mut keys = keys.to_vec();
        keys.sort();
        keys.dedup();
        let mut guards = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(guard) = self.acquire_stack_disk_lock(session, &key)? {
                guards.push(guard);
            }
        }
        Ok(guards)
    }

    #[cfg(test)]
    fn load_from_disk_if_needed(&mut self, session: &str, key: &Path) -> Result<bool, AftError> {
        let _disk_lock = self.acquire_stack_disk_lock(session, key)?;
        self.load_from_disk_if_needed_locked(session, key)
    }

    fn load_from_disk_if_needed_locked(
        &mut self,
        session: &str,
        key: &Path,
    ) -> Result<bool, AftError> {
        let entries = match self.read_stack_from_disk_unlocked(session, key) {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                if self.session_dir(session).is_some() {
                    self.restore_in_memory_stack(session, key, None);
                }
                if let Some(files) = self.disk_index.get_mut(session) {
                    files.remove(key);
                    if files.is_empty() {
                        self.disk_index.remove(session);
                    }
                }
                return Ok(false);
            }
            Err(error) => {
                return Err(AftError::IoError {
                    path: key.display().to_string(),
                    message: error,
                });
            }
        };

        self.update_counter_from_entries(&entries);
        if let Ok(Some((disk_meta, _))) = self.read_disk_meta_value(session, key) {
            self.disk_index
                .entry(session.to_string())
                .or_default()
                .insert(key.to_path_buf(), disk_meta);
        }
        self.entries
            .entry(session.to_string())
            .or_default()
            .insert(key.to_path_buf(), entries);
        Ok(true)
    }

    /// Re-read the on-disk stack while the per-stack disk lock is held.
    ///
    /// The on-disk stack is authoritative across processes. A long-running
    /// process may have a non-empty but stale in-memory stack, so every mutating
    /// append validates disk state before it writes new metadata or prunes old
    /// content files.
    fn ensure_stack_hydrated_locked(&mut self, session: &str, key: &Path) -> Result<(), AftError> {
        self.load_from_disk_if_needed_locked(session, key)?;
        Ok(())
    }

    fn refresh_disk_index_for_session(&mut self, session: &str) -> Result<Vec<PathBuf>, AftError> {
        let Some(session_dir) = self.session_dir(session) else {
            self.disk_index.remove(session);
            return Ok(Vec::new());
        };
        if !session_dir.exists() {
            self.disk_index.remove(session);
            return Ok(Vec::new());
        }

        let path_dirs = std::fs::read_dir(&session_dir).map_err(|error| AftError::IoError {
            path: session_dir.display().to_string(),
            message: error.to_string(),
        })?;
        let mut per_session = HashMap::new();
        for path_entry in path_dirs {
            let path_entry = path_entry.map_err(|error| AftError::IoError {
                path: session_dir.display().to_string(),
                message: error.to_string(),
            })?;
            let path_dir = path_entry.path();
            if !path_dir.is_dir() {
                continue;
            }
            let meta_path = path_dir.join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            let content =
                std::fs::read_to_string(&meta_path).map_err(|error| AftError::IoError {
                    path: meta_path.display().to_string(),
                    message: error.to_string(),
                })?;
            let meta = serde_json::from_str::<serde_json::Value>(&content).map_err(|error| {
                AftError::IoError {
                    path: meta_path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
            let path_str = meta
                .get("path")
                .and_then(|value| value.as_str())
                .ok_or_else(|| AftError::IoError {
                    path: meta_path.display().to_string(),
                    message: "backup meta missing path".to_string(),
                })?;
            let key = PathBuf::from(path_str);
            if !is_loadable_backup_path(&key, &path_dir) {
                continue;
            }
            let count = meta_entry_count(&meta).ok_or_else(|| AftError::IoError {
                path: meta_path.display().to_string(),
                message: "backup meta missing entry count".to_string(),
            })?;
            if count > 0 {
                per_session.insert(
                    key,
                    DiskMeta {
                        dir: path_dir,
                        count,
                    },
                );
            }
        }

        let keys = per_session.keys().cloned().collect::<Vec<_>>();
        if per_session.is_empty() {
            self.disk_index.remove(session);
        } else {
            self.disk_index.insert(session.to_string(), per_session);
        }
        Ok(keys)
    }

    fn restore_operation_candidate_keys(
        &mut self,
        session: &str,
    ) -> Result<Vec<PathBuf>, AftError> {
        let mut keys: HashSet<PathBuf> = self
            .refresh_disk_index_for_session(session)?
            .into_iter()
            .collect();
        if let Some(files) = self.entries.get(session) {
            keys.extend(files.keys().cloned());
        }
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        Ok(keys)
    }

    fn read_stack_heads_from_disk(
        &self,
        session: &str,
        key: &Path,
    ) -> Option<Vec<BackupEntryHead>> {
        let _disk_lock = match self.acquire_stack_disk_lock(session, key) {
            Ok(lock) => lock,
            Err(error) => {
                crate::slog_warn!(
                    "backup disk head read lock failed for {}: {}",
                    key.display(),
                    error
                );
                return None;
            }
        };
        match self.read_stack_heads_from_disk_unlocked(session, key) {
            Ok(heads) => heads,
            Err(error) => {
                crate::slog_warn!(
                    "backup disk head read failed for {}: {}",
                    key.display(),
                    error
                );
                None
            }
        }
    }

    fn read_stack_heads_from_disk_unlocked(
        &self,
        session: &str,
        key: &Path,
    ) -> Result<Option<Vec<BackupEntryHead>>, String> {
        let Some((disk_meta, meta)) = self.read_disk_meta_value(session, key)? else {
            return Ok(None);
        };
        if disk_meta.count == 0 {
            return Ok(None);
        }

        let heads = if is_v2_meta(&meta) {
            let entries = meta_entries(&meta)?;
            for entry in entries {
                self.validate_v2_content_reference(&disk_meta.dir, entry)?;
            }
            entries
                .iter()
                .enumerate()
                .map(|(i, entry)| backup_head_from_meta(Some(entry), i))
                .collect::<Vec<_>>()
        } else {
            let entries = meta.get("entries").and_then(|value| value.as_array());
            (0..disk_meta.count)
                .map(|i| backup_head_from_meta(entries.and_then(|entries| entries.get(i)), i))
                .collect::<Vec<_>>()
        };

        Ok((!heads.is_empty()).then_some(heads))
    }

    fn read_stack_from_disk_unlocked(
        &self,
        session: &str,
        key: &Path,
    ) -> Result<Option<Vec<BackupEntry>>, String> {
        let Some((disk_meta, meta)) = self.read_disk_meta_value(session, key)? else {
            return Ok(None);
        };
        if disk_meta.count == 0 {
            return Ok(None);
        }

        let entries = if is_v2_meta(&meta) {
            meta_entries(&meta)?
                .iter()
                .enumerate()
                .map(|(i, entry_meta)| self.entry_from_v2_meta(&disk_meta.dir, entry_meta, i))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let entries = meta.get("entries").and_then(|value| value.as_array());
            let mut loaded = Vec::new();
            for i in 0..disk_meta.count {
                let entry_meta = entries.and_then(|entries| entries.get(i));
                if let Some(entry) = legacy_entry_from_meta(&disk_meta.dir, entry_meta, i) {
                    loaded.push(entry);
                }
            }
            loaded
        };

        Ok((!entries.is_empty()).then_some(entries))
    }

    fn read_disk_meta_value(
        &self,
        session: &str,
        key: &Path,
    ) -> Result<Option<(DiskMeta, serde_json::Value)>, String> {
        let Some(session_dir) = self.session_dir(session) else {
            return Ok(None);
        };
        let dir = session_dir.join(Self::path_hash(key));
        let meta_path = dir.join("meta.json");
        if !meta_path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&meta_path)
            .map_err(|error| format!("failed to read {}: {}", meta_path.display(), error))?;
        let meta = serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|error| format!("failed to parse {}: {}", meta_path.display(), error))?;
        let path_str = meta
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("backup meta {} missing path", meta_path.display()))?;
        let stored_key = PathBuf::from(path_str);
        if stored_key != key || !is_loadable_backup_path(&stored_key, &dir) {
            return Ok(None);
        }
        let count = meta_entry_count(&meta)
            .ok_or_else(|| format!("backup meta {} missing entry count", meta_path.display()))?;
        Ok(Some((DiskMeta { dir, count }, meta)))
    }

    fn validate_v2_content_reference(
        &self,
        dir: &Path,
        entry_meta: &serde_json::Value,
    ) -> Result<(), String> {
        let kind = entry_kind_from_meta(Some(entry_meta));
        if matches!(kind, BackupEntryKind::Tombstone) {
            return Ok(());
        }
        let content_path = content_path_from_meta(entry_meta)?;
        let path = dir.join(content_path);
        if !path.is_file() {
            return Err(format!(
                "v2 backup meta references missing content file {}",
                path.display()
            ));
        }
        Ok(())
    }

    fn entry_from_v2_meta(
        &self,
        dir: &Path,
        entry_meta: &serde_json::Value,
        index: usize,
    ) -> Result<BackupEntry, String> {
        let kind = entry_kind_from_meta(Some(entry_meta));
        let content_bytes = match kind {
            BackupEntryKind::Content | BackupEntryKind::Symlink => {
                let content_path = content_path_from_meta(entry_meta)?;
                let path = dir.join(content_path);
                std::fs::read(&path).map_err(|error| {
                    format!(
                        "failed to read v2 backup content {}: {}",
                        path.display(),
                        error
                    )
                })?
            }
            BackupEntryKind::Tombstone => Vec::new(),
        };
        Ok(entry_from_meta(
            Some(entry_meta),
            index,
            kind,
            content_bytes,
        ))
    }

    fn write_snapshot_to_disk(
        &mut self,
        session: &str,
        key: &Path,
        stack: &[BackupEntry],
    ) -> Result<(), AftError> {
        let _disk_lock = self.acquire_stack_disk_lock(session, key)?;
        self.write_snapshot_to_disk_locked(session, key, stack)
    }

    fn write_snapshot_to_disk_locked(
        &mut self,
        session: &str,
        key: &Path,
        stack: &[BackupEntry],
    ) -> Result<(), AftError> {
        self.write_snapshot_to_disk_locked_with_db_plan(session, key, stack, DbMirrorPlan::Full)
    }

    fn write_appended_snapshot_to_disk_locked(
        &mut self,
        session: &str,
        key: &Path,
        stack: &[BackupEntry],
        evicted_orders: &[u128],
        new_entry: Option<&BackupEntry>,
    ) -> Result<(), AftError> {
        self.write_snapshot_to_disk_locked_with_db_plan(
            session,
            key,
            stack,
            DbMirrorPlan::Append {
                evicted_orders,
                new_entry,
            },
        )
    }

    fn write_snapshot_to_disk_locked_with_db_plan(
        &mut self,
        session: &str,
        key: &Path,
        stack: &[BackupEntry],
        db_plan: DbMirrorPlan<'_>,
    ) -> Result<(), AftError> {
        #[cfg(test)]
        if self.fail_next_disk_write {
            self.fail_next_disk_write = false;
            return Err(AftError::IoError {
                path: key.display().to_string(),
                message: "injected backup disk write failure".to_string(),
            });
        }

        let Some(session_dir) = self.session_dir(session) else {
            return Ok(());
        };

        std::fs::create_dir_all(&session_dir).map_err(|error| AftError::IoError {
            path: session_dir.display().to_string(),
            message: error.to_string(),
        })?;
        self.ensure_session_marker(&session_dir, session)?;

        let hash = Self::path_hash(key);
        let dir = session_dir.join(&hash);
        std::fs::create_dir_all(&dir).map_err(|error| AftError::IoError {
            path: dir.display().to_string(),
            message: error.to_string(),
        })?;

        let max_depth = self.policy.max_depth;
        let retained_start = stack.len().saturating_sub(max_depth);
        let retained = &stack[retained_start..];
        let mut referenced_content = HashSet::new();
        let mut wrote_content = false;

        for entry in retained {
            if let Some(content_path) = content_filename_for_entry(entry) {
                referenced_content.insert(content_path.clone());
                let final_path = dir.join(&content_path);
                if final_path.exists() {
                    continue;
                }
                let bytes = content_bytes_for_disk(entry);
                write_temp_fsync_rename(&dir, &content_path, &bytes).map_err(|error| {
                    AftError::IoError {
                        path: final_path.display().to_string(),
                        message: error.to_string(),
                    }
                })?;
                wrote_content = true;
            }
        }
        if wrote_content {
            fsync_dir(&dir).map_err(|error| AftError::IoError {
                path: dir.display().to_string(),
                message: error.to_string(),
            })?;
        }

        let entries: Vec<serde_json::Value> = retained.iter().map(entry_meta_json).collect();
        let meta = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "format_version": V2_FORMAT_VERSION,
            "session_id": session,
            "path": key.display().to_string(),
            "count": retained.len(),
            "entries": entries,
        });
        let meta_content =
            serde_json::to_string_pretty(&meta).map_err(|error| AftError::IoError {
                path: dir.join("meta.json").display().to_string(),
                message: error.to_string(),
            })?;
        write_temp_fsync_rename(&dir, "meta.json", meta_content.as_bytes()).map_err(|error| {
            AftError::IoError {
                path: dir.join("meta.json").display().to_string(),
                message: error.to_string(),
            }
        })?;
        fsync_dir(&dir).map_err(|error| AftError::IoError {
            path: dir.display().to_string(),
            message: error.to_string(),
        })?;

        prune_unreferenced_backup_files(&dir, &referenced_content).map_err(|error| {
            AftError::IoError {
                path: dir.display().to_string(),
                message: error.to_string(),
            }
        })?;
        let _ = fsync_dir(&dir);

        // Keep the in-memory disk_index in sync so tracked_files() and
        // disk_history_count() immediately reflect what we just wrote.
        self.disk_index
            .entry(session.to_string())
            .or_default()
            .insert(
                key.to_path_buf(),
                DiskMeta {
                    dir: dir.clone(),
                    count: retained.len(),
                },
            );
        self.mirror_stack_to_db(session, key, retained, db_plan);
        Ok(())
    }

    fn mirror_stack_to_db(
        &self,
        session: &str,
        key: &Path,
        stack: &[BackupEntry],
        plan: DbMirrorPlan<'_>,
    ) {
        let pool = self.db_pool.read().ok().and_then(|slot| slot.clone());
        let Some(pool) = pool else {
            return;
        };
        let harness = self.db_harness.read().ok().and_then(|slot| slot.clone());
        let Some(harness) = harness else {
            crate::slog_warn!(
                "dual-write backup to DB skipped for {}: harness not configured",
                key.display()
            );
            return;
        };
        let project_key = self
            .db_project_key
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(project_key) = project_key else {
            crate::slog_warn!(
                "dual-write backup to DB skipped for {}: project key not configured",
                key.display()
            );
            return;
        };

        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => {
                self.set_db_mirror_synced(session, key, false);
                crate::slog_warn!(
                    "dual-write backup to DB failed for {}: db mutex poisoned",
                    key.display()
                );
                return;
            }
        };
        let path_hash = Self::path_hash(key);
        let file_path = key.display().to_string();

        let context = DbMirrorContext {
            harness: &harness,
            session,
            project_key: &project_key,
            file_path: &file_path,
            path_hash: &path_hash,
        };
        let (write_result, operation) = match plan {
            DbMirrorPlan::Append {
                evicted_orders,
                new_entry,
            } if self.db_mirror_is_synced(session, key) => (
                apply_backup_append_delta_in_db(&conn, &context, evicted_orders, new_entry),
                "append delta",
            ),
            // A newly configured or previously failed mirror has no trusted DB
            // baseline. Repair it once from the authoritative disk stack before
            // later appends switch to constant-work deltas.
            DbMirrorPlan::Append { .. } | DbMirrorPlan::Full => (
                replace_backup_stack_in_db(&conn, &context, stack),
                "full stack",
            ),
        };
        self.set_db_mirror_synced(session, key, write_result.is_ok());
        if let Err(error) = write_result {
            crate::slog_warn!(
                "dual-write backup {} to DB failed for {} (rolled back, prior stack kept): {}",
                operation,
                key.display(),
                error
            );
        }
    }

    fn prune_disk_stacks_to_depth(&mut self, max_depth: usize) -> HashSet<(String, PathBuf)> {
        // Only prune stacks already discovered by lazy per-path hydration.
        // Loading every session here would turn a configure-time policy change
        // back into an O(all backup history) scan.
        let disk_keys = self
            .disk_index
            .iter()
            .flat_map(|(session, files)| {
                files
                    .keys()
                    .cloned()
                    .map(|key| (session.clone(), key))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut failed = HashSet::new();

        for (session, key) in disk_keys {
            let disk_lock = match self.acquire_stack_disk_lock(&session, &key) {
                Ok(lock) => lock,
                Err(error) => {
                    crate::slog_warn!(
                        "failed to lock backup stack for {} while applying max_depth: {}",
                        key.display(),
                        error
                    );
                    failed.insert((session, key));
                    continue;
                }
            };

            let mut stack = match self.read_stack_from_disk_unlocked(&session, &key) {
                Ok(Some(stack)) => stack,
                Ok(None) => Vec::new(),
                Err(error) => {
                    crate::slog_warn!(
                        "failed to read backup stack for {} while applying max_depth: {}",
                        key.display(),
                        error
                    );
                    failed.insert((session, key));
                    drop(disk_lock);
                    continue;
                }
            };
            trim_stack_to_depth(&mut stack, max_depth);
            if let Err(error) = self.write_snapshot_to_disk_locked(&session, &key, &stack) {
                crate::slog_warn!(
                    "failed to prune backup stack for {} while applying max_depth: {}",
                    key.display(),
                    error
                );
                failed.insert((session, key));
                drop(disk_lock);
                continue;
            }
            if stack.is_empty() {
                if let Some(files) = self.entries.get_mut(&session) {
                    files.remove(&key);
                    if files.is_empty() {
                        self.entries.remove(&session);
                    }
                }
            } else {
                self.entries
                    .entry(session.clone())
                    .or_default()
                    .insert(key.clone(), stack);
            }
            drop(disk_lock);
        }

        failed
    }

    fn remove_disk_backups(&mut self, session: &str, key: &Path) -> Result<(), AftError> {
        let _disk_lock = self.acquire_stack_disk_lock(session, key)?;
        self.remove_disk_backups_locked(session, key)
    }

    fn remove_disk_backups_locked(&mut self, session: &str, key: &Path) -> Result<(), AftError> {
        self.remove_db_backups(session, key);
        let removed = self.disk_index.get_mut(session).and_then(|s| s.remove(key));
        if let Some(meta) = removed {
            if let Err(error) = std::fs::remove_dir_all(&meta.dir) {
                return Err(AftError::IoError {
                    path: meta.dir.display().to_string(),
                    message: error.to_string(),
                });
            }
        } else if let Some(session_dir) = self.session_dir(session) {
            let hash = Self::path_hash(key);
            let dir = session_dir.join(&hash);
            if dir.exists() {
                if let Err(error) = std::fs::remove_dir_all(&dir) {
                    return Err(AftError::IoError {
                        path: dir.display().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }

        // If this session has no more disk entries, drop the map slot (session
        // dir itself is kept so the marker survives future sessions).
        let empty = self
            .disk_index
            .get(session)
            .map(|s| s.is_empty())
            .unwrap_or(false);
        if empty {
            self.disk_index.remove(session);
        }
        Ok(())
    }

    fn remove_db_backups(&self, session: &str, key: &Path) {
        let Some((pool, harness)) = self.db_pool_and_harness() else {
            return;
        };
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => {
                self.set_db_mirror_synced(session, key, false);
                crate::slog_warn!(
                    "delete backup DB rows failed for {}: db mutex poisoned",
                    key.display()
                );
                return;
            }
        };
        let path_hash = Self::path_hash(key);
        match crate::db::backups::delete_backups_for_path(&conn, &harness, session, &path_hash) {
            // Do not retain synchronization state for an absent stack. A future
            // first append can cheaply establish its one-row baseline again.
            Ok(_) => self.set_db_mirror_synced(session, key, false),
            Err(error) => {
                self.set_db_mirror_synced(session, key, false);
                crate::slog_warn!(
                    "delete backup DB rows failed for {}: {}",
                    key.display(),
                    error
                );
            }
        }
    }
}

fn backup_row_for_db(entry: &BackupEntry, context: &DbMirrorContext<'_>) -> BackupRow {
    let backup_path = content_filename_for_entry(entry);
    entry.to_backup_row(
        context.harness,
        context.session,
        context.project_key,
        context.file_path,
        context.path_hash,
        backup_path.as_deref(),
    )
}

fn replace_backup_stack_in_db(
    conn: &Connection,
    context: &DbMirrorContext<'_>,
    stack: &[BackupEntry],
) -> rusqlite::Result<()> {
    // Arbitrary stack transforms require full replacement. Keep the path delete
    // and every insert in one transaction: if an insert fails or SQLite is busy,
    // rollback leaves the previously consistent mirror untouched rather than a
    // partial stack that restore/history could mistake for authoritative data.
    let tx = conn.unchecked_transaction()?;
    crate::db::backups::delete_backups_for_path(
        &tx,
        context.harness,
        context.session,
        context.path_hash,
    )?;
    for entry in stack {
        crate::db::backups::insert_backup(&tx, &backup_row_for_db(entry, context))?;
    }
    tx.commit()
}

fn apply_backup_append_delta_in_db(
    conn: &Connection,
    context: &DbMirrorContext<'_>,
    evicted_orders: &[u128],
    new_entry: Option<&BackupEntry>,
) -> rusqlite::Result<()> {
    // A normal append changes only the entries named by the caller. Applying
    // those deletes and the single insert atomically preserves the same rollback
    // guarantee as full replacement without rewriting retained history.
    let tx = conn.unchecked_transaction()?;
    for &order in evicted_orders {
        crate::db::backups::delete_backup_for_order(
            &tx,
            context.harness,
            context.session,
            context.path_hash,
            order,
        )?;
    }
    if let Some(entry) = new_entry {
        crate::db::backups::insert_backup(&tx, &backup_row_for_db(entry, context))?;
    }
    tx.commit()
}

pub fn hash_session(session: &str) -> String {
    stable_hash_16(session.as_bytes())
}

pub fn new_op_id() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = current_timestamp().to_le_bytes()[..4]
            .try_into()
            .unwrap_or([0; 4]);
    }
    let rand = u32::from_le_bytes(bytes);
    format!("op-{}-{:08x}", current_timestamp() * 1000, rand)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackupEntryDiskMetadata {
    mode: Option<u32>,
    link_target: Option<PathBuf>,
    created_dirs: Vec<PathBuf>,
}

fn restore_metadata_json(entry: &BackupEntry) -> String {
    serde_json::json!({
        "version": DB_RESTORE_META_VERSION,
        "mode": entry.mode,
        "link_target": entry.link_target.as_ref().map(|target| target.display().to_string()),
        "created_dirs": entry
            .created_dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn restore_metadata_from_json(value: &str) -> Option<BackupEntryDiskMetadata> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    if value.get("version")?.as_u64()? != u64::from(DB_RESTORE_META_VERSION) {
        return None;
    }

    let mode = value.get("mode")?;
    if !mode.is_null()
        && mode
            .as_u64()
            .and_then(|mode| u32::try_from(mode).ok())
            .is_none()
    {
        return None;
    }
    let link_target = value.get("link_target")?;
    if !link_target.is_null() && !link_target.is_string() {
        return None;
    }
    if !value
        .get("created_dirs")?
        .as_array()?
        .iter()
        .all(serde_json::Value::is_string)
    {
        return None;
    }

    Some(restore_metadata_fields(&value))
}

fn restore_metadata_fields(value: &serde_json::Value) -> BackupEntryDiskMetadata {
    BackupEntryDiskMetadata {
        mode: value
            .get("mode")
            .and_then(|value| value.as_u64())
            .and_then(|mode| u32::try_from(mode).ok()),
        link_target: value
            .get("link_target")
            .and_then(|value| value.as_str())
            .map(PathBuf::from),
        created_dirs: value
            .get("created_dirs")
            .and_then(|value| value.as_array())
            .map(|dirs| {
                dirs.iter()
                    .filter_map(|dir| dir.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[derive(Debug, Clone)]
enum RestorePathState {
    Missing,
    Regular {
        content_bytes: Vec<u8>,
        mode: Option<u32>,
    },
    Symlink {
        target: PathBuf,
    },
    Directory,
}

fn backup_entry_from_path(
    path: &Path,
    backup_id: String,
    order: u128,
    description: &str,
    op_id: Option<&str>,
) -> Result<BackupEntry, AftError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AftError::FileNotFound {
            path: path.display().to_string(),
        },
        _ => AftError::IoError {
            path: path.display().to_string(),
            message: error.to_string(),
        },
    })?;
    let mode = file_mode(&metadata);

    let (kind, content, content_bytes, link_target) = if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        (
            BackupEntryKind::Symlink,
            target.display().to_string(),
            Arc::from([]),
            Some(target),
        )
    } else if metadata.is_file() {
        let bytes: Arc<[u8]> = read_captured_content(path)
            .map_err(|error| AftError::IoError {
                path: path.display().to_string(),
                message: error.to_string(),
            })?
            .into();
        (
            BackupEntryKind::Content,
            String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
            None,
        )
    } else {
        return Err(AftError::InvalidRequest {
            message: format!(
                "backup: '{}' is not a regular file or symlink",
                path.display()
            ),
        });
    };

    Ok(BackupEntry {
        backup_id,
        content,
        content_bytes,
        timestamp: current_timestamp(),
        order,
        description: description.to_string(),
        op_id: op_id.map(str::to_string),
        kind,
        mode,
        link_target,
        created_dirs: Vec::new(),
    })
}

fn backup_entry_from_capture(
    capture: &CapturedRegularFile,
    backup_id: String,
    order: u128,
    description: &str,
    op_id: Option<&str>,
) -> BackupEntry {
    BackupEntry {
        backup_id,
        content: String::from_utf8_lossy(capture.bytes()).into_owned(),
        content_bytes: capture.shared_bytes(),
        timestamp: current_timestamp(),
        order,
        description: description.to_string(),
        op_id: op_id.map(str::to_string),
        kind: BackupEntryKind::Content,
        mode: file_mode(capture.metadata()),
        link_target: None,
        created_dirs: Vec::new(),
    }
}

fn canonicalize_key(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            canonicalize_parent_join_leaf(&absolute)
        }
        Ok(_) => std::fs::canonicalize(&absolute)
            .map(|path| normalize_absolute_key(&path))
            .unwrap_or_else(|_| canonicalize_existing_ancestor(&absolute)),
        Err(_) => canonicalize_existing_ancestor(&absolute),
    }
}

fn canonicalize_parent_join_leaf(path: &Path) -> PathBuf {
    let Some(parent) = path.parent() else {
        return normalize_absolute_key(path);
    };
    let mut key = canonicalize_existing_ancestor(parent);
    if let Some(file_name) = path.file_name() {
        key.push(file_name);
    }
    key
}

fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut current = path;

    loop {
        if let Ok(mut base) = std::fs::canonicalize(current) {
            for component in suffix.iter().rev() {
                base.push(Path::new(component));
            }
            return normalize_absolute_key(&base);
        }
        let Some(parent) = current.parent() else {
            return normalize_absolute_key(path);
        };
        if let Some(file_name) = current.file_name() {
            suffix.push(file_name.to_os_string());
        }
        current = parent;
    }
}

fn normalize_absolute_key(path: &Path) -> PathBuf {
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

fn file_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn set_file_mode(path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = mode {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn capture_path_state(path: &Path) -> Result<RestorePathState, AftError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RestorePathState::Missing);
        }
        Err(error) => {
            return Err(AftError::IoError {
                path: path.display().to_string(),
                message: error.to_string(),
            });
        }
    };

    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(RestorePathState::Symlink { target })
    } else if metadata.is_file() {
        let content_bytes = std::fs::read(path).map_err(|error| AftError::IoError {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(RestorePathState::Regular {
            content_bytes,
            mode: file_mode(&metadata),
        })
    } else {
        Ok(RestorePathState::Directory)
    }
}

fn restore_entry_to_path(path: &Path, entry: &BackupEntry) -> std::io::Result<()> {
    match entry.kind {
        BackupEntryKind::Content => restore_regular_file(path, &entry.content_bytes, entry.mode),
        BackupEntryKind::Symlink => {
            let target = entry.link_target.as_ref().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "symlink backup entry missing target",
                )
            })?;
            restore_symlink(path, target)
        }
        BackupEntryKind::Tombstone => remove_tombstone_path(path),
    }
}

fn restore_path_state(path: &Path, state: &RestorePathState) -> bool {
    match state {
        RestorePathState::Missing => remove_file_or_symlink_if_present(path).is_ok(),
        RestorePathState::Regular {
            content_bytes,
            mode,
        } => restore_regular_file(path, content_bytes, *mode).is_ok(),
        RestorePathState::Symlink { target } => restore_symlink(path, target).is_ok(),
        RestorePathState::Directory => true,
    }
}

fn restore_regular_file(
    path: &Path,
    content_bytes: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, content_bytes)?;
    set_file_mode(path, mode)
}

fn restore_symlink(path: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    remove_file_or_symlink_if_present(path)?;
    create_symlink(target, path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn remove_tombstone_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(path)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "tombstone target is a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_file_or_symlink_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(path)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "path is a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_entry_disk_metadata(
    backup_path: &Path,
    backup_id: &str,
) -> Option<BackupEntryDiskMetadata> {
    let meta_path = if backup_path.file_name().and_then(|name| name.to_str()) == Some("meta.json") {
        backup_path.to_path_buf()
    } else {
        backup_path.parent()?.join("meta.json")
    };
    let content = std::fs::read_to_string(meta_path).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
    let entries = meta.get("entries")?.as_array()?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("backup_id").and_then(|value| value.as_str()) == Some(backup_id))?;
    Some(restore_metadata_fields(entry))
}

fn rollback_transactional_restore(
    written: &[(PathBuf, RestorePathState)],
    attempted: Option<(&PathBuf, &RestorePathState)>,
) -> bool {
    let mut ok = true;

    if let Some((path, state)) = attempted {
        ok &= restore_path_state(path, state);
    }

    for (path, state) in written.iter().rev() {
        ok &= restore_path_state(path, state);
    }

    ok
}

fn rollback_deleted_tombstones(deleted: &[(PathBuf, RestorePathState)]) -> bool {
    let mut ok = true;
    for (path, state) in deleted.iter().rev() {
        ok &= restore_path_state(path, state);
    }
    ok
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

fn remove_created_dirs_best_effort(dirs: &[PathBuf]) {
    let mut dirs = dirs.to_vec();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    dirs.dedup();

    for dir in dirs {
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn dir_has_entries(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn current_timestamp_nanos() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    nanos.min(u128::from(u64::MAX)) as u64
}

fn legacy_entry_order(timestamp_secs: u64, backup_id: &str) -> u128 {
    let nanos = timestamp_secs.saturating_mul(1_000_000_000);
    ((nanos as u128) << 32) | u128::from(backup_sequence(backup_id).unwrap_or(0))
}

fn parse_order_value(value: &serde_json::Value) -> Option<u128> {
    value
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .or_else(|| value.as_u64().map(u128::from))
}

fn is_v2_meta(meta: &serde_json::Value) -> bool {
    meta.get("format_version").and_then(|value| value.as_str()) == Some(V2_FORMAT_VERSION)
}

fn meta_entries(meta: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    meta.get("entries")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "backup meta missing entries array".to_string())
}

fn meta_entry_count(meta: &serde_json::Value) -> Option<usize> {
    if is_v2_meta(meta) {
        return meta
            .get("entries")
            .and_then(|value| value.as_array())
            .map(Vec::len);
    }
    meta.get("count")
        .and_then(|value| value.as_u64())
        .and_then(|count| usize::try_from(count).ok())
        .or_else(|| {
            meta.get("entries")
                .and_then(|value| value.as_array())
                .map(Vec::len)
        })
}

fn entry_kind_from_meta(entry_meta: Option<&serde_json::Value>) -> BackupEntryKind {
    match entry_meta
        .and_then(|meta| meta.get("kind"))
        .and_then(|value| value.as_str())
    {
        Some("tombstone") => BackupEntryKind::Tombstone,
        Some("symlink") => BackupEntryKind::Symlink,
        _ => BackupEntryKind::Content,
    }
}

fn backup_head_from_meta(entry_meta: Option<&serde_json::Value>, index: usize) -> BackupEntryHead {
    let backup_id = entry_backup_id(entry_meta, index);
    let timestamp = entry_meta
        .and_then(|meta| meta.get("timestamp"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let order = entry_meta
        .and_then(|meta| meta.get("order"))
        .and_then(parse_order_value)
        .unwrap_or_else(|| legacy_entry_order(timestamp, &backup_id));
    BackupEntryHead {
        order,
        op_id: entry_meta
            .and_then(|meta| meta.get("op_id"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

fn entry_backup_id(entry_meta: Option<&serde_json::Value>, index: usize) -> String {
    entry_meta
        .and_then(|meta| meta.get("backup_id"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("disk-{}", index))
}

fn entry_from_meta(
    entry_meta: Option<&serde_json::Value>,
    index: usize,
    kind: BackupEntryKind,
    content_bytes: Vec<u8>,
) -> BackupEntry {
    let backup_id = entry_backup_id(entry_meta, index);
    let timestamp = entry_meta
        .and_then(|meta| meta.get("timestamp"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let order = entry_meta
        .and_then(|meta| meta.get("order"))
        .and_then(parse_order_value)
        .unwrap_or_else(|| legacy_entry_order(timestamp, &backup_id));
    let link_target = if kind == BackupEntryKind::Symlink {
        entry_meta
            .and_then(|meta| meta.get("link_target"))
            .and_then(|value| value.as_str())
            .map(PathBuf::from)
            .or_else(|| {
                Some(PathBuf::from(
                    String::from_utf8_lossy(&content_bytes).into_owned(),
                ))
            })
    } else {
        None
    };
    let content = match kind {
        BackupEntryKind::Content => String::from_utf8_lossy(&content_bytes).into_owned(),
        BackupEntryKind::Symlink => link_target
            .as_ref()
            .map(|target| target.display().to_string())
            .unwrap_or_default(),
        BackupEntryKind::Tombstone => String::new(),
    };
    BackupEntry {
        backup_id,
        content,
        content_bytes: content_bytes.into(),
        timestamp,
        order,
        description: entry_meta
            .and_then(|meta| meta.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or("restored from disk")
            .to_string(),
        op_id: entry_meta
            .and_then(|meta| meta.get("op_id"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
        kind,
        mode: entry_meta
            .and_then(|meta| meta.get("mode"))
            .and_then(|value| value.as_u64())
            .and_then(|mode| u32::try_from(mode).ok()),
        link_target,
        created_dirs: entry_meta
            .and_then(|meta| meta.get("created_dirs"))
            .and_then(|value| value.as_array())
            .map(|dirs| {
                dirs.iter()
                    .filter_map(|dir| dir.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn legacy_entry_from_meta(
    dir: &Path,
    entry_meta: Option<&serde_json::Value>,
    index: usize,
) -> Option<BackupEntry> {
    let kind = entry_kind_from_meta(entry_meta);
    let content_bytes = match kind {
        BackupEntryKind::Content | BackupEntryKind::Symlink => {
            std::fs::read(dir.join(format!("{}.bak", index))).ok()?
        }
        BackupEntryKind::Tombstone => Vec::new(),
    };
    Some(entry_from_meta(entry_meta, index, kind, content_bytes))
}

fn content_path_from_meta(entry_meta: &serde_json::Value) -> Result<&str, String> {
    let value = entry_meta
        .get("content_path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "v2 backup entry missing content_path".to_string())?;
    let path = Path::new(value);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(value),
        _ => Err(format!("invalid backup content_path '{value}'")),
    }
}

fn sanitize_backup_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn content_filename_for_entry(entry: &BackupEntry) -> Option<String> {
    match entry.kind {
        BackupEntryKind::Content | BackupEntryKind::Symlink => Some(format!(
            "bak_{}_{}.bak",
            entry.order,
            sanitize_backup_id(&entry.backup_id)
        )),
        BackupEntryKind::Tombstone => None,
    }
}

fn content_bytes_for_disk(entry: &BackupEntry) -> Cow<'_, [u8]> {
    match entry.kind {
        BackupEntryKind::Content => Cow::Borrowed(&entry.content_bytes),
        BackupEntryKind::Symlink => Cow::Owned(
            entry
                .link_target
                .as_ref()
                .map(|target| target.as_os_str().to_string_lossy().as_bytes().to_vec())
                .unwrap_or_default(),
        ),
        BackupEntryKind::Tombstone => Cow::Borrowed(&[]),
    }
}

fn entry_meta_json(entry: &BackupEntry) -> serde_json::Value {
    serde_json::json!({
        "backup_id": entry.backup_id,
        "timestamp": entry.timestamp,
        "order": entry.order.to_string(),
        "description": entry.description,
        "op_id": entry.op_id,
        "kind": match entry.kind {
            BackupEntryKind::Content => "content",
            BackupEntryKind::Symlink => "symlink",
            BackupEntryKind::Tombstone => "tombstone",
        },
        "content_path": content_filename_for_entry(entry),
        "mode": entry.mode,
        "link_target": entry.link_target.as_ref().map(|target| target.display().to_string()),
        "created_dirs": entry
            .created_dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>(),
    })
}

fn drain_stack_to_depth(stack: &mut Vec<BackupEntry>, max_depth: usize) -> Vec<BackupEntry> {
    let overflow = stack.len().saturating_sub(max_depth);
    stack.drain(..overflow).collect()
}

fn trim_stack_to_depth(stack: &mut Vec<BackupEntry>, max_depth: usize) {
    let overflow = stack.len().saturating_sub(max_depth);
    drop(stack.drain(..overflow));
}

fn write_temp_fsync_rename(dir: &Path, final_name: &str, content: &[u8]) -> std::io::Result<()> {
    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        final_name,
        std::process::id(),
        current_timestamp_nanos()
    );
    let tmp_path = dir.join(tmp_name);
    let final_path = dir.join(final_name);
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    replace_file(&tmp_path, &final_path)
}

fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    // On Windows, std::fs::rename uses MoveFileExW replace-existing semantics,
    // so a single rename keeps meta.json atomic instead of deleting it first.
    std::fs::rename(from, to)
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> std::io::Result<()> {
    // Windows cannot open a directory as a regular File handle without
    // FILE_FLAG_BACKUP_SEMANTICS — `File::open` on a directory returns
    // "Access is denied" (os error 5). Directory fsync is also not the
    // durability mechanism there: `std::fs::rename` maps to MoveFileExW with
    // MOVEFILE_WRITE_THROUGH, which flushes the rename's metadata change to
    // disk, and each content/meta file is already `sync_all()`-ed before the
    // rename. So a separate directory sync is unnecessary on non-Unix.
    Ok(())
}

fn prune_unreferenced_backup_files(
    dir: &Path,
    referenced: &HashSet<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_backup_content = (name.starts_with("bak_") && name.ends_with(".bak"))
            || legacy_numeric_backup_name(name);
        let is_temp = name.ends_with(".tmp") || name.contains(".tmp.");
        if is_temp || (is_backup_content && !referenced.contains(name)) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

fn legacy_numeric_backup_name(name: &str) -> bool {
    name.strip_suffix(".bak")
        .is_some_and(|stem| !stem.is_empty() && stem.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_loadable_backup_path(key: &Path, path_dir: &Path) -> bool {
    if !key.is_absolute()
        || key
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let Some(dir_name) = path_dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    BackupStore::path_hash(key) == dir_name
}

fn stable_hash_16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8]
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

fn backup_sequence(backup_id: &str) -> Option<u64> {
    backup_id
        .strip_prefix("backup-")
        .or_else(|| backup_id.strip_prefix("disk-"))
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use crate::protocol::DEFAULT_SESSION_ID;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, LazyLock, Mutex};

    const DB_MIRROR_HARNESS: &str = "opencode";
    const DB_MIRROR_SESSION: &str = "db-mirror-session";
    const DB_MIRROR_PROJECT: &str = "db-mirror-project";
    const DB_MIRROR_FILE: &str = "/project/src/file.rs";
    const DB_MIRROR_PATH_HASH: &str = "db-mirror-path";

    static MIRROR_SQL_TRACE: LazyLock<Mutex<Vec<String>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));

    fn capture_mirror_sql(sql: &str) {
        MIRROR_SQL_TRACE.lock().unwrap().push(sql.to_string());
    }

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("aft_backup_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn db_mirror_context() -> DbMirrorContext<'static> {
        DbMirrorContext {
            harness: DB_MIRROR_HARNESS,
            session: DB_MIRROR_SESSION,
            project_key: DB_MIRROR_PROJECT,
            file_path: DB_MIRROR_FILE,
            path_hash: DB_MIRROR_PATH_HASH,
        }
    }

    fn db_mirror_entry(index: u128, kind: BackupEntryKind) -> BackupEntry {
        BackupEntry {
            backup_id: format!("backup-{index}"),
            content: format!("content-{index}"),
            content_bytes: format!("content-{index}").into_bytes().into(),
            timestamp: u64::try_from(index + 1).unwrap(),
            order: index + 1,
            description: format!("generated-{index}"),
            op_id: index.is_multiple_of(3).then(|| format!("op-{}", index % 5)),
            kind,
            mode: None,
            link_target: None,
            created_dirs: Vec::new(),
        }
    }

    fn db_mirror_rows(conn: &Connection) -> Vec<BackupRow> {
        crate::db::backups::list_backups(
            conn,
            DB_MIRROR_HARNESS,
            DB_MIRROR_SESSION,
            DB_MIRROR_PATH_HASH,
        )
        .unwrap()
    }

    #[test]
    fn backup_db_restore_metadata_round_trips_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let context = db_mirror_context();
        let mut entry = db_mirror_entry(0, BackupEntryKind::Symlink);
        entry.mode = Some(0o100755);
        entry.link_target = Some(PathBuf::from("../target"));
        entry.created_dirs = vec![PathBuf::from("/project/new"), PathBuf::from("/project")];

        crate::db::backups::insert_backup(&conn, &backup_row_for_db(&entry, &context)).unwrap();
        let row = db_mirror_rows(&conn).pop().unwrap();
        let metadata = row
            .restore_meta
            .as_deref()
            .and_then(restore_metadata_from_json)
            .unwrap();

        assert_eq!(
            metadata,
            BackupEntryDiskMetadata {
                mode: entry.mode,
                link_target: entry.link_target,
                created_dirs: entry.created_dirs,
            }
        );
    }

    #[test]
    fn backup_db_append_delta_matches_full_rebuild_for_generated_sequences() {
        let context = db_mirror_context();
        let mut saw_eviction = false;
        let mut saw_tombstone = false;

        for max_depth in [1, 2, 5, 8] {
            let delta_dir = tempfile::tempdir().unwrap();
            let full_dir = tempfile::tempdir().unwrap();
            let delta_conn = crate::db::open(&delta_dir.path().join("aft.db")).unwrap();
            let full_conn = crate::db::open(&full_dir.path().join("aft.db")).unwrap();
            let mut stack = Vec::new();
            let mut generated = 0x9e37_79b9_u64 ^ u64::try_from(max_depth).unwrap();

            for step in 0..64_u128 {
                generated = generated
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let kind = if generated.is_multiple_of(4) {
                    saw_tombstone = true;
                    BackupEntryKind::Tombstone
                } else {
                    BackupEntryKind::Content
                };
                let entry = db_mirror_entry(step, kind);
                let evicted = drain_stack_to_depth(&mut stack, max_depth - 1);
                saw_eviction |= !evicted.is_empty();
                let evicted_orders = evicted.iter().map(|entry| entry.order).collect::<Vec<_>>();
                stack.push(entry);

                apply_backup_append_delta_in_db(
                    &delta_conn,
                    &context,
                    &evicted_orders,
                    stack.last(),
                )
                .unwrap();
                replace_backup_stack_in_db(&full_conn, &context, &stack).unwrap();

                assert_eq!(
                    db_mirror_rows(&delta_conn),
                    db_mirror_rows(&full_conn),
                    "delta mirror drifted at depth {max_depth}, step {step}"
                );
            }
        }

        assert!(saw_eviction, "generated sequence must exercise eviction");
        assert!(saw_tombstone, "generated sequence must exercise tombstones");
    }

    #[test]
    fn backup_db_append_delta_executes_constant_dml_statement_count() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = project.path().join("statement-count.txt");
        let mut store = BackupStore::new();
        store.set_storage_dir(storage.path().to_path_buf(), 72);
        store.set_db_harness(Harness::Opencode);
        store.set_db_project_key(DB_MIRROR_PROJECT.to_string());
        store.set_policy(BackupPolicy {
            enabled: true,
            max_depth: 4,
            max_file_size: None,
        });
        let shared = Arc::new(Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        store.set_db_pool(shared.clone());
        for version in 0..4 {
            fs::write(&file, format!("version-{version}")).unwrap();
            store
                .snapshot(DB_MIRROR_SESSION, &file, "fill retained stack")
                .unwrap();
        }

        MIRROR_SQL_TRACE.lock().unwrap().clear();
        shared.lock().unwrap().trace(Some(capture_mirror_sql));
        fs::write(&file, "version-4").unwrap();
        store
            .snapshot(DB_MIRROR_SESSION, &file, "measured append")
            .unwrap();
        shared.lock().unwrap().trace(None);

        let traced = MIRROR_SQL_TRACE.lock().unwrap().clone();
        let dml = traced
            .iter()
            .filter(|sql| {
                let sql = sql.trim_start();
                sql.starts_with("DELETE FROM backups") || sql.starts_with("INSERT INTO backups")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            dml.len(),
            2,
            "one eviction plus one append must execute exactly two DML statements: {traced:?}"
        );
        assert_eq!(
            dml.iter()
                .filter(|sql| sql.trim_start().starts_with("DELETE FROM backups"))
                .count(),
            1
        );
        assert_eq!(
            dml.iter()
                .filter(|sql| sql.trim_start().starts_with("INSERT INTO backups"))
                .count(),
            1
        );
    }

    #[test]
    fn backup_db_append_delta_rolls_back_eviction_when_insert_fails() {
        let context = db_mirror_context();
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let prior_stack = vec![
            db_mirror_entry(0, BackupEntryKind::Content),
            db_mirror_entry(1, BackupEntryKind::Content),
        ];
        replace_backup_stack_in_db(&conn, &context, &prior_stack).unwrap();
        let prior_rows = db_mirror_rows(&conn);
        conn.execute_batch(
            "CREATE TRIGGER fail_delta_insert
             BEFORE INSERT ON backups
             WHEN NEW.backup_id = 'backup-2'
             BEGIN
               SELECT RAISE(ABORT, 'forced delta insert failure');
             END;",
        )
        .unwrap();

        let new_entry = db_mirror_entry(2, BackupEntryKind::Tombstone);
        let error = apply_backup_append_delta_in_db(
            &conn,
            &context,
            &[prior_stack[0].order],
            Some(&new_entry),
        )
        .unwrap_err();

        assert!(error.to_string().contains("forced delta insert failure"));
        assert_eq!(
            db_mirror_rows(&conn),
            prior_rows,
            "failed append must roll back the preceding eviction"
        );
    }

    #[test]
    fn backup_db_unknown_mirror_repairs_disk_history_before_append_deltas() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = project.path().join("repair-before-delta.txt");
        let mut disk_only = BackupStore::new();
        disk_only.set_storage_dir(storage.path().to_path_buf(), 72);
        fs::write(&file, "v1").unwrap();
        disk_only
            .snapshot(DB_MIRROR_SESSION, &file, "disk first")
            .unwrap();
        fs::write(&file, "v2").unwrap();
        disk_only
            .snapshot(DB_MIRROR_SESSION, &file, "disk second")
            .unwrap();

        let mut mirrored = BackupStore::new();
        mirrored.set_storage_dir(storage.path().to_path_buf(), 72);
        mirrored.set_db_harness(Harness::Opencode);
        mirrored.set_db_project_key(DB_MIRROR_PROJECT.to_string());
        let conn = Arc::new(Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        mirrored.set_db_pool(conn.clone());
        fs::write(&file, "v3").unwrap();
        mirrored
            .snapshot(DB_MIRROR_SESSION, &file, "first mirrored append")
            .unwrap();

        let key = canonicalize_key(&file);
        let rows = crate::db::backups::list_backups(
            &conn.lock().unwrap(),
            DB_MIRROR_HARNESS,
            DB_MIRROR_SESSION,
            &BackupStore::path_hash(&key),
        )
        .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.description.as_str())
                .collect::<Vec<_>>(),
            vec!["disk first", "disk second", "first mirrored append"]
        );
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let path = temp_file("round_trip.txt", "original");
        let mut store = BackupStore::new();

        let id = store
            .snapshot(DEFAULT_SESSION_ID, &path, "before edit")
            .unwrap()
            .unwrap();
        assert!(id.starts_with("backup-"));

        fs::write(&path, "modified").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "modified");

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.content, "original");
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn multiple_snapshots_preserve_order() {
        let path = temp_file("order.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "first").unwrap();
        fs::write(&path, "v2").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "second").unwrap();
        fs::write(&path, "v3").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "third").unwrap();

        let history = store.history(DEFAULT_SESSION_ID, &path);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "v1");
        assert_eq!(history[1].content, "v2");
        assert_eq!(history[2].content, "v3");
    }

    #[test]
    fn restore_pops_from_stack() {
        let path = temp_file("pop.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "first").unwrap();
        fs::write(&path, "v2").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path, "second").unwrap();

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.description, "second");
        assert_eq!(entry.content, "v2");

        let history = store.history(DEFAULT_SESSION_ID, &path);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn empty_history_returns_empty_vec() {
        let store = BackupStore::new();
        let path = Path::new("/tmp/aft_backup_tests/nonexistent_history.txt");
        assert!(store.history(DEFAULT_SESSION_ID, path).is_empty());
    }

    #[test]
    fn snapshot_nonexistent_file_returns_error() {
        let mut store = BackupStore::new();
        let path = Path::new("/tmp/aft_backup_tests/absolutely_does_not_exist.txt");
        assert!(store.snapshot(DEFAULT_SESSION_ID, path, "test").is_err());
    }

    #[test]
    fn tracked_files_lists_snapshotted_paths() {
        let path1 = temp_file("tracked1.txt", "a");
        let path2 = temp_file("tracked2.txt", "b");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path1, "snap1").unwrap();
        store.snapshot(DEFAULT_SESSION_ID, &path2, "snap2").unwrap();
        assert_eq!(store.tracked_files(DEFAULT_SESSION_ID).len(), 2);
    }

    #[test]
    fn sessions_are_isolated() {
        let path = temp_file("isolated.txt", "original");
        let mut store = BackupStore::new();

        store.snapshot("session_a", &path, "a's snapshot").unwrap();

        // Session B sees no history for this file.
        assert!(store.history("session_b", &path).is_empty());
        assert_eq!(store.tracked_files("session_b").len(), 0);

        // Session B's restore_latest fails with NoUndoHistory.
        let err = store.restore_latest("session_b", &path);
        assert!(matches!(err, Err(AftError::NoUndoHistory { .. })));

        // Session A still sees its own snapshot.
        assert_eq!(store.history("session_a", &path).len(), 1);
        assert_eq!(store.tracked_files("session_a").len(), 1);
    }

    #[test]
    fn per_session_per_file_cap_is_independent() {
        // Two sessions fill up their own stacks independently; hitting the cap
        // in session A does not evict anything from session B.
        let path = temp_file("cap_indep.txt", "v0");
        let mut store = BackupStore::new();

        for i in 0..(MAX_UNDO_DEPTH + 5) {
            fs::write(&path, format!("a{}", i)).unwrap();
            store.snapshot("session_a", &path, "a").unwrap();
        }
        fs::write(&path, "b_initial").unwrap();
        store.snapshot("session_b", &path, "b").unwrap();

        // Session A should be capped at MAX_UNDO_DEPTH.
        assert_eq!(store.history("session_a", &path).len(), MAX_UNDO_DEPTH);
        // Session B should still have its single entry.
        assert_eq!(store.history("session_b", &path).len(), 1);
    }

    #[test]
    fn sessions_with_backups_lists_all_namespaces() {
        let path_a = temp_file("sessions_list_a.txt", "a");
        let path_b = temp_file("sessions_list_b.txt", "b");
        let mut store = BackupStore::new();

        store.snapshot("alice", &path_a, "from alice").unwrap();
        store.snapshot("bob", &path_b, "from bob").unwrap();

        let sessions = store.sessions_with_backups();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s == "alice"));
        assert!(sessions.iter().any(|s| s == "bob"));
    }

    #[test]
    fn disk_persistence_survives_reload() {
        let dir = std::env::temp_dir().join("aft_backup_disk_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = temp_file("disk_persist.txt", "original");

        // Create store with storage, snapshot under default session, drop.
        {
            let mut store = BackupStore::new();
            store.set_storage_dir(dir.clone(), 72);
            store
                .snapshot(DEFAULT_SESSION_ID, &file_path, "before edit")
                .unwrap();
        }

        // Modify the file externally.
        fs::write(&file_path, "externally modified").unwrap();

        // Create new store, load from disk, restore.
        let mut store2 = BackupStore::new();
        store2.set_storage_dir(dir.clone(), 72);

        let (entry, warning) = store2
            .restore_latest(DEFAULT_SESSION_ID, &file_path)
            .unwrap();
        assert_eq!(entry.content, "original");
        assert!(warning.is_some()); // modified externally
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "original");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_after_restart_preserves_history_and_unique_ids() {
        // Regression (bug #8): after a restart the BackupStore is fresh
        // (entries cleared, counter reset to 0). A new snapshot must EXTEND the
        // persisted undo stack — not overwrite it with a single entry — and must
        // not reuse backup-0. Two undo levels must remain available across the
        // restart boundary.
        let dir = std::env::temp_dir().join("aft_backup_restart_history_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file_path = temp_file("restart_history.txt", "v0");

        // Run 1: edit v0 -> v1 (snapshot captures "v0"), then write v1.
        let first_id = {
            let mut store = BackupStore::new();
            store.set_storage_dir(dir.clone(), 72);
            let id = store
                .snapshot(DEFAULT_SESSION_ID, &file_path, "edit 1")
                .unwrap()
                .unwrap();
            fs::write(&file_path, "v1").unwrap();
            id
        };

        // Restart: fresh store, same storage dir. Edit v1 -> v2 (snapshot
        // captures "v1"), then write v2.
        let second_id = {
            let mut store = BackupStore::new();
            store.set_storage_dir(dir.clone(), 72);
            let id = store
                .snapshot(DEFAULT_SESSION_ID, &file_path, "edit 2")
                .unwrap()
                .unwrap();
            fs::write(&file_path, "v2").unwrap();
            id
        };

        // The post-restart snapshot must NOT reuse the first id (counter
        // advanced past persisted entries).
        assert_ne!(
            first_id, second_id,
            "post-restart snapshot reused backup id {first_id}"
        );

        // Both undo levels survive: a fresh store sees 2 entries on disk, and
        // two sequential restores walk v1 then v0.
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);
        assert_eq!(
            store.history(DEFAULT_SESSION_ID, &file_path).len(),
            2,
            "prior history was overwritten by the post-restart snapshot"
        );

        let (entry1, _) = store
            .restore_latest(DEFAULT_SESSION_ID, &file_path)
            .unwrap();
        assert_eq!(entry1.content, "v1", "first undo should restore v1");
        let (entry0, _) = store
            .restore_latest(DEFAULT_SESSION_ID, &file_path)
            .unwrap();
        assert_eq!(entry0.content, "v0", "second undo should restore v0");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_store_defers_backup_io_until_first_snapshot_and_preserves_undo() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = project.path().join("lazy-history.txt");
        fs::write(&path, "v0").unwrap();

        {
            let mut store = BackupStore::new();
            store.set_storage_dir(storage.path().to_path_buf(), 72);
            assert_eq!(store.disk_io_count_for_tests(), 0);
            store.snapshot("session-a", &path, "captures v0").unwrap();
            fs::write(&path, "v1").unwrap();
        }

        let mut fresh = BackupStore::new();
        fresh.set_storage_dir(storage.path().to_path_buf(), 72);
        assert_eq!(
            fresh.disk_io_count_for_tests(),
            0,
            "binding a fresh store must not inspect backup directories"
        );

        fresh.snapshot("session-a", &path, "captures v1").unwrap();
        assert!(fresh.disk_io_count_for_tests() > 0);
        fs::write(&path, "v2").unwrap();

        fresh.restore_latest("session-a", &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
        fresh.restore_latest("session-a", &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v0");
    }

    #[test]
    fn same_namespace_bind_is_idempotent_and_session_history_survives() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = project.path().join("session-isolation.txt");
        fs::write(&path, "v0").unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir_for_harness(storage.path().to_path_buf(), Harness::Opencode, 72);
        assert_eq!(store.disk_io_count_for_tests(), 0);
        store.snapshot("session-a", &path, "session A").unwrap();
        fs::write(&path, "v1").unwrap();

        let io_before_rebind = store.disk_io_count_for_tests();
        store.set_storage_dir_for_harness(storage.path().to_path_buf(), Harness::Opencode, 72);
        assert_eq!(store.disk_io_count_for_tests(), io_before_rebind);
        assert_eq!(store.disk_history_count("session-a", &path), 1);

        store.snapshot("session-b", &path, "session B").unwrap();
        fs::write(&path, "v2").unwrap();
        store.restore_latest("session-a", &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v0");
        assert_eq!(store.disk_history_count("session-b", &path), 1);
    }

    #[test]
    fn legacy_flat_layout_migrates_to_default_session() {
        // Simulate a pre-session on-disk layout (schema v1) and verify
        // process-wide backup maintenance moves it into the default namespace.
        let dir = std::env::temp_dir().join("aft_backup_migration_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();

        // Fake legacy entry for some path hash.
        let legacy_hash = "deadbeefcafebabe";
        let legacy_dir = backups.join(legacy_hash);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("0.bak"), "original content").unwrap();
        let legacy_meta = serde_json::json!({
            "path": "/tmp/migrated_file.txt",
            "count": 1,
        });
        fs::write(
            legacy_dir.join("meta.json"),
            serde_json::to_string_pretty(&legacy_meta).unwrap(),
        )
        .unwrap();

        // Run migration.
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);
        assert_eq!(store.disk_io_count_for_tests(), 0);
        store.run_process_maintenance_once();

        // After migration, the legacy dir should be gone from the top level,
        // and the entry should now live under the default-session hash dir.
        let default_session_dir = backups.join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        assert!(default_session_dir.exists());
        assert!(default_session_dir.join(legacy_hash).exists());
        assert!(!backups.join(legacy_hash).exists());

        // The upgraded meta.json should now include session_id + schema_version.
        let meta_content =
            fs::read_to_string(default_session_dir.join(legacy_hash).join("meta.json")).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap();
        assert_eq!(meta["session_id"], DEFAULT_SESSION_ID);
        assert_eq!(meta["schema_version"], SCHEMA_VERSION);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_maintenance_removes_stale_backup_sessions() {
        let dir = std::env::temp_dir().join("aft_backup_gc_test");
        let _ = fs::remove_dir_all(&dir);
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();

        let stale_session_dir = backups.join("stale-session");
        fs::create_dir_all(&stale_session_dir).unwrap();
        let stale_marker = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": "stale",
            "last_accessed": 1,
        });
        fs::write(
            stale_session_dir.join("session.json"),
            serde_json::to_string_pretty(&stale_marker).unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 1);
        assert_eq!(store.disk_io_count_for_tests(), 0);
        store.run_process_maintenance_once();

        assert!(!stale_session_dir.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn markerless_session_dir_is_skipped_not_mapped_to_default() {
        let dir = std::env::temp_dir().join("aft_backup_markerless_skip_test");
        let _ = fs::remove_dir_all(&dir);
        let file_path = temp_file("markerless.txt", "original");
        let key = canonicalize_key(&file_path);
        let path_dir = dir
            .join("backups")
            .join("corrupt-session")
            .join("path-entry");
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(path_dir.join("0.bak"), "original").unwrap();
        fs::write(
            path_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": "lost-session",
                "path": key.display().to_string(),
                "count": 1,
                "entries": [{
                    "backup_id": "disk-0",
                    "timestamp": 0,
                    "description": "corrupt marker test",
                    "op_id": null,
                    "kind": "content",
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);

        assert_eq!(store.disk_history_count(DEFAULT_SESSION_ID, &file_path), 0);
        assert!(store.sessions_with_backups().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_storage_dir_reconfiguration_drops_previous_disk_index() {
        let dir_a = std::env::temp_dir().join("aft_backup_storage_a_test");
        let dir_b = std::env::temp_dir().join("aft_backup_storage_b_test");
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        let file_path = temp_file("storage_reconfigure.txt", "original");

        let mut store = BackupStore::new();
        store.set_storage_dir(dir_a.clone(), 72);
        store
            .snapshot(DEFAULT_SESSION_ID, &file_path, "stored in a")
            .unwrap();
        assert_eq!(store.disk_history_count(DEFAULT_SESSION_ID, &file_path), 1);

        store.set_storage_dir(dir_b.clone(), 72);

        assert_eq!(store.disk_history_count(DEFAULT_SESSION_ID, &file_path), 0);
        assert!(store.tracked_files(DEFAULT_SESSION_ID).is_empty());
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn restore_last_operation_restores_all_top_entries_for_same_op() {
        let path_a = temp_file("op_restore_a.txt", "a1");
        let path_b = temp_file("op_restore_b.txt", "b1");
        let mut store = BackupStore::new();
        let op_id = "op-test-00000001";

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "a", Some(op_id))
            .unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "b", Some(op_id))
            .unwrap();
        fs::write(&path_a, "a2").unwrap();
        fs::write(&path_b, "b2").unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(restored.restored.len(), 2);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a1");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b1");
    }

    #[test]
    fn restore_last_operation_deletes_tombstone_destination() {
        let dir = std::env::temp_dir().join("aft_backup_tombstone_delete_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.txt");
        let destination = dir.join("destination.txt");
        let canonical_dir = fs::canonicalize(&dir).unwrap();
        fs::write(&source, "original").unwrap();

        let mut store = BackupStore::new();
        let op_id = "op-tombstone-delete";
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &source, "move source", Some(op_id))
            .unwrap();
        fs::rename(&source, &destination).unwrap();
        store
            .snapshot_op_tombstone(DEFAULT_SESSION_ID, op_id, &destination, "created dest")
            .unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(restored.restored.len(), 2);
        let restored_paths = restored
            .restored
            .iter()
            .map(|file| file.path.clone())
            .collect::<HashSet<_>>();
        assert_eq!(
            restored_paths,
            HashSet::from([
                canonical_dir.join("source.txt"),
                canonical_dir.join("destination.txt")
            ])
        );
        assert!(restored
            .restored
            .iter()
            .all(|file| !file.backup_id.is_empty()));
        assert_eq!(fs::read_to_string(&source).unwrap(), "original");
        assert!(!destination.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_last_operation_rolls_back_source_when_tombstone_delete_fails() {
        let dir = std::env::temp_dir().join("aft_backup_tombstone_atomic_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.txt");
        let destination = dir.join("destination.txt");
        fs::write(&source, "original").unwrap();

        let mut store = BackupStore::new();
        let op_id = "op-tombstone-atomic";
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &source, "move source", Some(op_id))
            .unwrap();
        fs::rename(&source, &destination).unwrap();
        store
            .snapshot_op_tombstone(DEFAULT_SESSION_ID, op_id, &destination, "created dest")
            .unwrap();

        fs::remove_file(&destination).unwrap();
        fs::create_dir(&destination).unwrap();
        let result = store.restore_last_operation(DEFAULT_SESSION_ID);

        assert!(result.is_err(), "directory tombstone target should fail");
        assert!(
            !source.exists(),
            "source restore must roll back when destination deletion fails"
        );
        assert!(
            destination.is_dir(),
            "failed tombstone target should remain"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Uses Unix-specific PermissionsExt::set_mode to make a target file
    // read-only and force the staging-phase write of the two-phase-commit
    // restore to fail. The atomicity logic it exercises is platform-independent
    // — Windows has different mechanisms for forcing write failures, covered
    // separately.
    #[cfg(unix)]
    #[test]
    fn restore_last_operation_is_atomic_when_a_write_fails() {
        let dir = std::env::temp_dir().join("aft_backup_tests_atomic_restore");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.txt");
        let path_b = dir.join("b.txt");
        let path_c = dir.join("c.txt");
        fs::write(&path_a, "a-original").unwrap();
        fs::write(&path_b, "b-original").unwrap();
        fs::write(&path_c, "c-original").unwrap();

        let mut store = BackupStore::new();
        let op_id = "op-atomic-restore-01";
        let id_a = store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "a", Some(op_id))
            .unwrap()
            .unwrap();
        let id_b = store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "b", Some(op_id))
            .unwrap()
            .unwrap();
        let id_c = store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_c, "c", Some(op_id))
            .unwrap()
            .unwrap();
        fs::write(&path_a, "a-modified").unwrap();
        fs::write(&path_b, "b-modified").unwrap();
        fs::write(&path_c, "c-modified").unwrap();

        let original_permissions = fs::metadata(&path_b).unwrap().permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_mode(0o444);
        fs::set_permissions(&path_b, readonly_permissions).unwrap();

        let result = store.restore_last_operation(DEFAULT_SESSION_ID);
        fs::set_permissions(&path_b, original_permissions).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-modified");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-modified");
        assert_eq!(fs::read_to_string(&path_c).unwrap(), "c-modified");

        let history_a = store.history(DEFAULT_SESSION_ID, &path_a);
        let history_b = store.history(DEFAULT_SESSION_ID, &path_b);
        let history_c = store.history(DEFAULT_SESSION_ID, &path_c);
        assert_eq!(history_a.len(), 1);
        assert_eq!(history_b.len(), 1);
        assert_eq!(history_c.len(), 1);
        assert_eq!(history_a[0].backup_id, id_a);
        assert_eq!(history_b[0].backup_id, id_b);
        assert_eq!(history_c[0].backup_id, id_c);
        assert_eq!(history_a[0].op_id.as_deref(), Some(op_id));
        assert_eq!(history_b[0].op_id.as_deref(), Some(op_id));
        assert_eq!(history_c[0].op_id.as_deref(), Some(op_id));

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(restored.restored.len(), 3);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-original");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-original");
        assert_eq!(fs::read_to_string(&path_c).unwrap(), "c-original");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_last_operation_restores_only_most_recent_op() {
        let path_a = temp_file("op_recent_a.txt", "a1");
        let path_b = temp_file("op_recent_b.txt", "b1");
        let mut store = BackupStore::new();

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "older", Some("op-older"))
            .unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "newer", Some("op-newer"))
            .unwrap();
        fs::write(&path_a, "a2").unwrap();
        fs::write(&path_b, "b2").unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, "op-newer");
        assert_eq!(restored.restored.len(), 1);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a2");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b1");
    }

    #[test]
    fn restore_recreates_missing_parent_directories() {
        // Simulate aft_delete files: [dir/] with recursive: true:
        // the parent directories are gone by the time we restore.
        let dir = std::env::temp_dir().join("aft_backup_tests_recreate_parents");
        let _ = fs::remove_dir_all(&dir);
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let path = nested.join("inner.txt");
        fs::write(&path, "original").unwrap();

        let mut store = BackupStore::new();
        let op_id = "op-recreate-parents-01";
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "original", Some(op_id))
            .unwrap();

        // Real-world delete sequence: tree is wiped before undo runs.
        fs::remove_dir_all(&dir).unwrap();
        assert!(!path.exists());
        assert!(!nested.exists());
        assert!(!dir.exists());

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(restored.restored.len(), 1);
        assert!(
            path.exists(),
            "file should be restored even though both nested/ and dir/ were missing"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_last_operation_ignores_legacy_entries_without_op_id() {
        let path = temp_file("op_legacy_none.txt", "v1");
        let mut store = BackupStore::new();

        store.snapshot(DEFAULT_SESSION_ID, &path, "legacy").unwrap();
        fs::write(&path, "v2").unwrap();

        let err = store.restore_last_operation(DEFAULT_SESSION_ID);
        assert!(matches!(err, Err(AftError::NoUndoHistory { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn schema_v2_meta_loads_with_none_op_id_and_persists_as_v3() {
        let dir = std::env::temp_dir().join("aft_backup_v2_to_v3_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file_path = temp_file("v2_to_v3.txt", "original");
        let key = canonicalize_key(&file_path);
        let session_dir = dir
            .join("backups")
            .join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        let path_dir = session_dir.join(BackupStore::path_hash(&key));
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(path_dir.join("0.bak"), "original").unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "session_id": DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            path_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "session_id": DEFAULT_SESSION_ID,
                "path": key.display().to_string(),
                "count": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);
        assert!(store
            .load_from_disk_if_needed(DEFAULT_SESSION_ID, &key)
            .unwrap());
        let history = store.history(DEFAULT_SESSION_ID, &file_path);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].op_id, None);

        fs::write(&file_path, "second").unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &file_path, "second", Some("op-v3"))
            .unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path_dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(written["schema_version"], SCHEMA_VERSION);
        assert_eq!(written["entries"][0]["op_id"], serde_json::Value::Null);
        assert_eq!(written["entries"][1]["op_id"], "op-v3");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_restore_latest_still_works_with_op_ids() {
        let path = temp_file("op_per_file.txt", "v1");
        let mut store = BackupStore::new();

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "op", Some("op-file"))
            .unwrap();
        fs::write(&path, "v2").unwrap();

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.op_id.as_deref(), Some("op-file"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
    }

    #[test]
    fn per_file_restore_latest_deletes_tombstone() {
        let dir = std::env::temp_dir().join("aft_backup_per_file_tombstone_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("created.txt");
        fs::write(&path, "created").unwrap();

        let mut store = BackupStore::new();
        let id = store
            .snapshot_op_tombstone(DEFAULT_SESSION_ID, "op-create", &path, "created")
            .unwrap()
            .unwrap();

        let (entry, _) = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap();
        assert_eq!(entry.backup_id, id);
        assert!(!path.exists(), "tombstone undo should delete the file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lazy_stack_read_skips_tampered_meta_path_hash_mismatch() {
        let dir = std::env::temp_dir().join("aft_backup_tampered_meta_skip_test");
        let _ = fs::remove_dir_all(&dir);
        let backups = dir.join("backups");
        let session_dir = backups.join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        let path_dir = session_dir.join("not-the-path-hash");
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(path_dir.join("0.bak"), "outside").unwrap();
        fs::write(
            path_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": DEFAULT_SESSION_ID,
                "path": "/tmp/aft-malicious-overwrite-target.txt",
                "count": 1,
                "entries": [{
                    "backup_id": "backup-0",
                    "timestamp": current_timestamp(),
                    "order": "1",
                    "description": "tampered",
                    "op_id": "op-tampered",
                    "kind": "content",
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.clone(), 72);

        assert!(store
            .history(
                DEFAULT_SESSION_ID,
                Path::new("/tmp/aft-malicious-overwrite-target.txt")
            )
            .is_empty());
        assert!(store.sessions_with_backups().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_last_operation_uses_only_top_entries_and_persisted_order() {
        let path_a = temp_file("op_order_a.txt", "a1");
        let path_b = temp_file("op_order_b.txt", "b1");
        let mut store = BackupStore::new();

        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_a, "buried", Some("op-buried"))
            .unwrap();
        store
            .snapshot(DEFAULT_SESSION_ID, &path_a, "top without op")
            .unwrap();
        store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path_b, "top", Some("op-top"))
            .unwrap();

        let key_a = canonicalize_key(&path_a);
        let key_b = canonicalize_key(&path_b);
        let files = store.entries.get_mut(DEFAULT_SESSION_ID).unwrap();
        files.get_mut(&key_a).unwrap()[0].order = u128::MAX;
        files.get_mut(&key_a).unwrap()[1].order = 1;
        files.get_mut(&key_b).unwrap()[0].order = 2;

        fs::write(&path_a, "a2").unwrap();
        fs::write(&path_b, "b2").unwrap();

        let restored = store.restore_last_operation(DEFAULT_SESSION_ID).unwrap();
        assert_eq!(restored.op_id, "op-top");
        assert_eq!(restored.restored.len(), 1);
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a2");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b1");
    }

    #[test]
    fn append_only_v2_adds_one_content_file_at_steady_depth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append_only.txt");
        fs::write(&path, "v0").unwrap();
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.path().to_path_buf(), 72);

        for i in 0..MAX_UNDO_DEPTH {
            store
                .snapshot(DEFAULT_SESSION_ID, &path, "push")
                .unwrap()
                .unwrap();
            fs::write(&path, format!("v{}", i + 1)).unwrap();
        }

        let key = canonicalize_key(&path);
        let stack_dir = store
            .session_dir(DEFAULT_SESSION_ID)
            .unwrap()
            .join(BackupStore::path_hash(&key));
        let before = backup_content_names(&stack_dir);
        assert_eq!(before.len(), MAX_UNDO_DEPTH);

        store
            .snapshot(DEFAULT_SESSION_ID, &path, "steady push")
            .unwrap()
            .unwrap();
        let after = backup_content_names(&stack_dir);
        assert_eq!(after.len(), MAX_UNDO_DEPTH);
        assert_eq!(after.difference(&before).count(), 1);
        assert_eq!(before.difference(&after).count(), 1);

        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(stack_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            meta.get("format_version").and_then(|v| v.as_str()),
            Some("v2")
        );
        assert!(meta_entries(&meta)
            .unwrap()
            .iter()
            .all(|entry| entry.get("content_path").and_then(|v| v.as_str()).is_some()));
    }

    #[test]
    fn legacy_stack_migrates_to_v2_on_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.txt");
        fs::write(&path, "current").unwrap();
        let key = canonicalize_key(&path);
        let session_dir = dir
            .path()
            .join("backups")
            .join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        let stack_dir = session_dir.join(BackupStore::path_hash(&key));
        fs::create_dir_all(&stack_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(stack_dir.join("0.bak"), "legacy").unwrap();
        fs::write(
            stack_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": DEFAULT_SESSION_ID,
                "path": key.display().to_string(),
                "count": 1,
                "entries": [{
                    "backup_id": "backup-0",
                    "timestamp": current_timestamp(),
                    "order": "1",
                    "description": "legacy",
                    "kind": "content",
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.path().to_path_buf(), 72);
        assert_eq!(
            store.history(DEFAULT_SESSION_ID, &path)[0].content,
            "legacy"
        );

        store
            .snapshot(DEFAULT_SESSION_ID, &path, "migrate")
            .unwrap()
            .unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(stack_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            meta.get("format_version").and_then(|v| v.as_str()),
            Some("v2")
        );
        assert!(!stack_dir.join("0.bak").exists());
        assert_eq!(backup_content_names(&stack_dir).len(), 2);
    }

    #[test]
    fn snapshot_reloads_non_empty_stale_stack_before_append() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = project.path().join("stale-memory.txt");
        fs::write(&path, "v0").unwrap();
        let policy = BackupPolicy {
            enabled: true,
            max_depth: 2,
            max_file_size: None,
        };

        let mut store_a = BackupStore::new();
        store_a.set_storage_dir(storage.path().to_path_buf(), 72);
        store_a.set_policy(policy);
        store_a
            .snapshot(DEFAULT_SESSION_ID, &path, "a captures v0")
            .unwrap();
        fs::write(&path, "v1").unwrap();

        let mut store_b = BackupStore::new();
        store_b.set_storage_dir(storage.path().to_path_buf(), 72);
        store_b.set_policy(policy);
        store_b
            .snapshot(DEFAULT_SESSION_ID, &path, "b captures v1")
            .unwrap();
        fs::write(&path, "v2").unwrap();

        store_a
            .snapshot(DEFAULT_SESSION_ID, &path, "a captures v2")
            .unwrap();

        let mut fresh = BackupStore::new();
        fresh.set_storage_dir(storage.path().to_path_buf(), 72);
        let contents = fresh
            .history(DEFAULT_SESSION_ID, &path)
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>();
        assert_eq!(contents, vec!["v1".to_string(), "v2".to_string()]);
    }

    #[test]
    fn restore_latest_clears_stale_memory_when_disk_stack_disappears() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "stale-resurrection-session";
        let path = project.path().join("stale-resurrection.txt");
        fs::write(&path, "v0").unwrap();

        let mut store_a = BackupStore::new();
        store_a.set_storage_dir(storage.path().to_path_buf(), 72);
        store_a.snapshot(session, &path, "a captures v0").unwrap();
        fs::write(&path, "v1").unwrap();

        let mut store_b = BackupStore::new();
        store_b.set_storage_dir(storage.path().to_path_buf(), 72);
        let (restored, _) = store_b.restore_latest(session, &path).unwrap();
        assert_eq!(restored.content, "v0");

        fs::write(&path, "current after other restore").unwrap();
        let error = store_a.restore_latest(session, &path).unwrap_err();

        assert_eq!(error.code(), "no_undo_history");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "current after other restore"
        );
        let key = canonicalize_key(&path);
        assert!(store_a
            .entries
            .get(session)
            .and_then(|files| files.get(&key))
            .is_none());

        let snapshot_path = project.path().join("stale-snapshot.txt");
        fs::write(&snapshot_path, "snapshot v0").unwrap();
        let mut store_c = BackupStore::new();
        store_c.set_storage_dir(storage.path().to_path_buf(), 72);
        store_c
            .snapshot(session, &snapshot_path, "c captures v0")
            .unwrap();
        fs::write(&snapshot_path, "snapshot v1").unwrap();
        let mut store_d = BackupStore::new();
        store_d.set_storage_dir(storage.path().to_path_buf(), 72);
        store_d.restore_latest(session, &snapshot_path).unwrap();

        fs::write(&snapshot_path, "snapshot current").unwrap();
        store_c
            .snapshot(session, &snapshot_path, "c captures current")
            .unwrap();
        let mut fresh = BackupStore::new();
        fresh.set_storage_dir(storage.path().to_path_buf(), 72);
        let contents = fresh
            .history(session, &snapshot_path)
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>();
        assert_eq!(contents, vec!["snapshot current".to_string()]);
    }

    #[test]
    fn restore_last_operation_returns_retry_error_under_unbounded_key_churn() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "restore-churn-session";
        let base_path = project.path().join("base.txt");
        fs::write(&base_path, "base before").unwrap();
        let mut base_store = BackupStore::new();
        base_store.set_storage_dir(storage.path().to_path_buf(), 72);
        base_store
            .snapshot_with_op(session, &base_path, "base op", Some("op-base"))
            .unwrap();
        fs::write(&base_path, "base after").unwrap();

        let churn_count = Arc::new(Mutex::new(0usize));
        let hook_count = churn_count.clone();
        let hook_project = project.path().to_path_buf();
        let hook_storage = storage.path().to_path_buf();
        set_restore_before_lock_hook_for_tests(session, move |_| {
            let mut count = hook_count.lock().unwrap();
            let churn_path = hook_project.join(format!("churn-{}.txt", *count));
            fs::write(&churn_path, format!("churn before {}", *count)).unwrap();
            let mut churn_store = BackupStore::new();
            churn_store.set_storage_dir(hook_storage.clone(), 72);
            let op_id = format!("op-churn-{}", *count);
            churn_store
                .snapshot_with_op(session, &churn_path, "churn op", Some(&op_id))
                .unwrap();
            fs::write(&churn_path, format!("churn after {}", *count)).unwrap();
            *count += 1;
            *count < MAX_RESTORE_OPERATION_LOCK_RETRIES
        });

        let mut restore_store = BackupStore::new();
        restore_store.set_storage_dir(storage.path().to_path_buf(), 72);
        let error = restore_store.restore_last_operation(session).unwrap_err();

        assert_eq!(error.code(), "io_error");
        assert!(error
            .to_string()
            .contains("backup stack changing under concurrent activity; retry"));
        assert_eq!(
            *churn_count.lock().unwrap(),
            MAX_RESTORE_OPERATION_LOCK_RETRIES
        );
    }

    #[test]
    fn restore_last_operation_test_hooks_are_isolated_per_session() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session_a = "restore-hook-session-a";
        let session_b = "restore-hook-session-b";
        let path_a = project.path().join("restore-hook-a.txt");
        fs::write(&path_a, "v0").unwrap();

        let mut store_a = BackupStore::new();
        store_a.set_storage_dir(storage.path().to_path_buf(), 72);
        store_a
            .snapshot_with_op(session_a, &path_a, "old op", Some("op-old-a"))
            .unwrap();
        fs::write(&path_a, "v1").unwrap();

        let hook_storage = storage.path().to_path_buf();
        let hook_path_a = path_a.clone();
        set_restore_before_lock_hook_for_tests(session_a, move |_| {
            let mut hook_store = BackupStore::new();
            hook_store.set_storage_dir(hook_storage.clone(), 72);
            hook_store
                .snapshot_with_op(session_a, &hook_path_a, "new op", Some("op-new-a"))
                .unwrap();
            fs::write(&hook_path_a, "v2").unwrap();
            false
        });
        set_restore_before_lock_hook_for_tests(session_b, |_| false);

        let restored = store_a.restore_last_operation(session_a).unwrap();

        assert_eq!(restored.op_id, "op-new-a");
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "v1");
        run_restore_before_lock_hook_for_tests(session_b, 0);
    }

    #[test]
    fn restore_last_operation_rescans_stack_after_locking() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "restore-toctou-session";
        let path = project.path().join("restore-toctou.txt");
        fs::write(&path, "v0").unwrap();

        let mut store_a = BackupStore::new();
        store_a.set_storage_dir(storage.path().to_path_buf(), 72);
        store_a
            .snapshot_with_op(session, &path, "old op", Some("op-old"))
            .unwrap();
        fs::write(&path, "v1").unwrap();

        let hook_storage = storage.path().to_path_buf();
        let hook_path = path.clone();
        set_restore_before_lock_hook_for_tests(session, move |_| {
            let mut store_b = BackupStore::new();
            store_b.set_storage_dir(hook_storage.clone(), 72);
            store_b
                .snapshot_with_op(session, &hook_path, "new op", Some("op-new"))
                .unwrap();
            fs::write(&hook_path, "v2").unwrap();
            false
        });

        let restored = store_a.restore_last_operation(session).unwrap();

        assert_eq!(restored.op_id, "op-new");
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
    }

    #[test]
    fn corrupt_v2_meta_fails_closed_for_operation_and_single_restore() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "corrupt-v2-session";
        let path = project.path().join("corrupt-v2.txt");
        fs::write(&path, "current").unwrap();
        let key = canonicalize_key(&path);
        let session_dir = storage
            .path()
            .join("backups")
            .join(BackupStore::session_hash(session));
        let stack_dir = session_dir.join(BackupStore::path_hash(&key));
        fs::create_dir_all(&stack_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": session,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            stack_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "format_version": "v2",
                "session_id": session,
                "path": key.display().to_string(),
                "count": 1,
                "entries": [{
                    "backup_id": "backup-corrupt",
                    "timestamp": current_timestamp(),
                    "order": "9",
                    "description": "corrupt disk should win over DB fallback",
                    "op_id": "op-corrupt",
                    "kind": "content",
                    "content_path": "bak_9_backup-corrupt.bak",
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        let fallback_path = stack_dir.join("db-fallback.bak");
        fs::write(&fallback_path, "db fallback").unwrap();
        crate::db::backups::upsert_backup(
            &conn,
            &BackupRow {
                backup_id: "backup-db".to_string(),
                harness: "opencode".to_string(),
                session_id: session.to_string(),
                project_key: "project".to_string(),
                op_id: Some("op-corrupt".to_string()),
                order: 9,
                file_path: key.display().to_string(),
                path_hash: BackupStore::path_hash(&key),
                backup_path: Some(fallback_path.display().to_string()),
                kind: "content".to_string(),
                description: "db fallback".to_string(),
                created_at: i64::try_from(current_timestamp()).unwrap(),
                is_tombstone: false,
                restore_meta: None,
            },
        )
        .unwrap();
        let shared = Arc::new(Mutex::new(conn));

        let mut single = BackupStore::new();
        single.set_storage_dir(storage.path().to_path_buf(), 72);
        single.set_db_harness(Harness::Opencode);
        single.set_db_project_key("project".to_string());
        single.set_db_pool(shared.clone());
        let single_error = single.restore_latest(session, &path).unwrap_err();
        assert_eq!(single_error.code(), "io_error");
        assert_eq!(fs::read_to_string(&path).unwrap(), "current");

        let mut operation = BackupStore::new();
        operation.set_storage_dir(storage.path().to_path_buf(), 72);
        operation.set_db_harness(Harness::Opencode);
        operation.set_db_project_key("project".to_string());
        operation.set_db_pool(shared);
        let operation_error = operation.restore_last_operation(session).unwrap_err();
        assert_eq!(operation_error.code(), "io_error");
        assert_eq!(fs::read_to_string(&path).unwrap(), "current");
    }

    #[test]
    fn replace_file_replaces_existing_meta_with_single_rename_path() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta.json");
        let temp_path = dir.path().join("meta.tmp");
        fs::write(&meta_path, "old").unwrap();
        fs::write(&temp_path, "new").unwrap();

        replace_file(&temp_path, &meta_path).unwrap();

        assert_eq!(fs::read_to_string(&meta_path).unwrap(), "new");
        assert!(!temp_path.exists());
    }

    #[test]
    fn snapshot_write_failure_restores_full_pre_trim_stack() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "rollback-pretrim-session";
        let path = project.path().join("rollback.txt");
        fs::write(&path, "v0").unwrap();
        let mut store = BackupStore::new();
        store.set_storage_dir(storage.path().to_path_buf(), 72);
        store.set_policy(BackupPolicy {
            enabled: true,
            max_depth: 2,
            max_file_size: None,
        });

        store.snapshot(session, &path, "first").unwrap();
        fs::write(&path, "v1").unwrap();
        store.snapshot(session, &path, "second").unwrap();
        fs::write(&path, "v2").unwrap();
        let key = canonicalize_key(&path);
        let before_file_stack = store.entries.get(session).unwrap().get(&key).unwrap();
        let before_file_identity = before_file_stack
            .iter()
            .map(|entry| (entry.backup_id.clone(), entry.order))
            .collect::<Vec<_>>();

        store.fail_next_disk_write_for_tests();
        let error = store.snapshot(session, &path, "third").unwrap_err();
        assert_eq!(error.code(), "io_error");
        let after_file_stack = store.entries.get(session).unwrap().get(&key).unwrap();
        assert_eq!(after_file_stack.len(), before_file_identity.len());
        assert_eq!(
            after_file_stack
                .iter()
                .map(|entry| (entry.backup_id.clone(), entry.order))
                .collect::<Vec<_>>(),
            before_file_identity
        );

        let successful_id = store
            .snapshot(session, &path, "after failure")
            .unwrap()
            .unwrap();
        let successful_stack = store.entries.get(session).unwrap().get(&key).unwrap();
        assert_eq!(successful_stack.len(), 2);
        assert_eq!(successful_stack[0].description, "second");
        assert_eq!(successful_stack[1].description, "after failure");
        assert_eq!(successful_stack[1].backup_id, successful_id);

        let tombstone = project.path().join("created-by-op.txt");
        store
            .snapshot_op_tombstone(session, "op-one", &tombstone, "created one")
            .unwrap();
        store
            .snapshot_op_tombstone(session, "op-two", &tombstone, "created two")
            .unwrap();
        let tombstone_key = canonicalize_key(&tombstone);
        let before_tombstone_stack = store
            .entries
            .get(session)
            .unwrap()
            .get(&tombstone_key)
            .unwrap();
        let before_tombstone_identity = before_tombstone_stack
            .iter()
            .map(|entry| (entry.backup_id.clone(), entry.order, entry.op_id.clone()))
            .collect::<Vec<_>>();

        store.fail_next_disk_write_for_tests();
        let error = store
            .snapshot_op_tombstone(session, "op-three", &tombstone, "created three")
            .unwrap_err();
        assert_eq!(error.code(), "io_error");
        let after_tombstone_stack = store
            .entries
            .get(session)
            .unwrap()
            .get(&tombstone_key)
            .unwrap();
        assert_eq!(
            after_tombstone_stack
                .iter()
                .map(|entry| (entry.backup_id.clone(), entry.order, entry.op_id.clone()))
                .collect::<Vec<_>>(),
            before_tombstone_identity
        );

        let successful_id = store
            .snapshot_op_tombstone(session, "op-four", &tombstone, "created four")
            .unwrap()
            .unwrap();
        let successful_stack = store
            .entries
            .get(session)
            .unwrap()
            .get(&tombstone_key)
            .unwrap();
        assert_eq!(successful_stack.len(), 2);
        assert_eq!(successful_stack[0].op_id.as_deref(), Some("op-two"));
        assert_eq!(successful_stack[1].op_id.as_deref(), Some("op-four"));
        assert_eq!(successful_stack[1].backup_id, successful_id);
    }

    #[test]
    fn snapshot_at_max_depth_keeps_newest_window() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "depth-window-session";
        let path = project.path().join("window.txt");
        fs::write(&path, "v0").unwrap();
        let mut store = BackupStore::new();
        store.set_storage_dir(storage.path().to_path_buf(), 72);
        store.set_policy(BackupPolicy {
            enabled: true,
            max_depth: 2,
            max_file_size: None,
        });

        store.snapshot(session, &path, "first").unwrap();
        fs::write(&path, "v1").unwrap();
        let second_id = store.snapshot(session, &path, "second").unwrap().unwrap();
        fs::write(&path, "v2").unwrap();
        let third_id = store.snapshot(session, &path, "third").unwrap().unwrap();

        let history = store.history(session, &path);
        assert_eq!(history.len(), 2);
        assert_eq!(
            history
                .iter()
                .map(|entry| entry.backup_id.as_str())
                .collect::<Vec<_>>(),
            vec![second_id.as_str(), third_id.as_str()]
        );
        assert_eq!(history[0].content_bytes.as_ref(), b"v1");
        assert_eq!(history[1].content_bytes.as_ref(), b"v2");
    }

    #[test]
    fn lowering_max_depth_prunes_disk_content_immediately() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = project.path().join("policy-prune.txt");
        fs::write(&path, "v0").unwrap();
        let mut store = BackupStore::new();
        store.set_storage_dir(storage.path().to_path_buf(), 72);

        for i in 0..3 {
            store
                .snapshot(DEFAULT_SESSION_ID, &path, &format!("snapshot {i}"))
                .unwrap();
            fs::write(&path, format!("v{}", i + 1)).unwrap();
        }

        let key = canonicalize_key(&path);
        let stack_dir = store
            .session_dir(DEFAULT_SESSION_ID)
            .unwrap()
            .join(BackupStore::path_hash(&key));
        assert_eq!(backup_content_names(&stack_dir).len(), 3);

        store.set_policy(BackupPolicy {
            enabled: true,
            max_depth: 1,
            max_file_size: None,
        });

        assert_eq!(backup_content_names(&stack_dir).len(), 1);
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(stack_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta_entry_count(&meta), Some(1));
        let mut fresh = BackupStore::new();
        fresh.set_storage_dir(storage.path().to_path_buf(), 72);
        assert_eq!(fresh.history(DEFAULT_SESSION_ID, &path).len(), 1);
    }

    #[test]
    fn v2_missing_content_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-content.txt");
        fs::write(&path, "current").unwrap();
        let key = canonicalize_key(&path);
        let session_dir = dir
            .path()
            .join("backups")
            .join(BackupStore::session_hash(DEFAULT_SESSION_ID));
        let stack_dir = session_dir.join(BackupStore::path_hash(&key));
        fs::create_dir_all(&stack_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "session_id": DEFAULT_SESSION_ID,
                "last_accessed": current_timestamp(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            stack_dir.join("meta.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "format_version": "v2",
                "session_id": DEFAULT_SESSION_ID,
                "path": key.display().to_string(),
                "count": 1,
                "entries": [{
                    "backup_id": "backup-0",
                    "timestamp": current_timestamp(),
                    "order": "1",
                    "description": "missing",
                    "kind": "content",
                    "content_path": "bak_1_backup-0.bak",
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut store = BackupStore::new();
        store.set_storage_dir(dir.path().to_path_buf(), 72);
        let error = store.restore_latest(DEFAULT_SESSION_ID, &path).unwrap_err();
        assert_eq!(error.code(), "io_error");
    }

    #[test]
    fn v2_orphan_files_are_ignored_then_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orphan.txt");
        fs::write(&path, "v0").unwrap();
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.path().to_path_buf(), 72);
        store
            .snapshot(DEFAULT_SESSION_ID, &path, "first")
            .unwrap()
            .unwrap();
        let key = canonicalize_key(&path);
        let stack_dir = store
            .session_dir(DEFAULT_SESSION_ID)
            .unwrap()
            .join(BackupStore::path_hash(&key));
        fs::write(stack_dir.join("bak_999_orphan.bak"), "orphan").unwrap();

        assert_eq!(store.history(DEFAULT_SESSION_ID, &path).len(), 1);
        fs::write(&path, "v1").unwrap();
        store
            .snapshot(DEFAULT_SESSION_ID, &path, "second")
            .unwrap()
            .unwrap();
        assert!(!stack_dir.join("bak_999_orphan.bak").exists());
    }

    #[test]
    fn default_policy_skips_sparse_1_7_gib_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram-large.db");
        let file = fs::File::create(&path).unwrap();
        file.set_len(1_700_000_000).unwrap();

        let store = BackupStore::new();
        assert_eq!(
            store.should_snapshot_path(&path, false).unwrap(),
            SnapshotDecision::Skip(BackupSkippedReason::TooLarge)
        );
        assert_eq!(
            store.policy().max_file_size,
            Some(DEFAULT_MAX_BACKUP_FILE_SIZE)
        );
    }

    #[test]
    fn explicit_larger_cap_allows_a_file_above_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-but-allowed.db");
        let file = fs::File::create(&path).unwrap();
        file.set_len(DEFAULT_MAX_BACKUP_FILE_SIZE + 1).unwrap();

        let mut store = BackupStore::new();
        store.set_policy(BackupPolicy {
            max_file_size: Some(DEFAULT_MAX_BACKUP_FILE_SIZE + 2),
            ..BackupPolicy::default()
        });
        assert_eq!(
            store.should_snapshot_path(&path, false).unwrap(),
            SnapshotDecision::Capture
        );
    }

    #[test]
    fn too_large_snapshots_increment_the_process_counter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("over-cap.txt");
        fs::write(&path, "oversized").unwrap();

        let before = backup_skipped_totals().0;
        let mut store = BackupStore::new();
        store.set_policy(BackupPolicy {
            max_file_size: Some(1),
            ..BackupPolicy::default()
        });
        assert!(store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "large", Some("large-op"))
            .unwrap()
            .is_none());
        assert_eq!(
            store.skipped_reason_for_operation(DEFAULT_SESSION_ID, "large-op", Some(&path)),
            Some(BackupSkippedReason::TooLarge)
        );
        assert!(backup_skipped_totals().0 >= before + 1);
    }

    #[test]
    fn temp_paths_and_zero_cap_report_their_skip_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scratch.txt");
        fs::write(&path, "scratch").unwrap();

        let before_temp = backup_skipped_totals().1;
        let mut store = BackupStore::new();
        store.enforce_temp_path_policy_for_tests();
        assert!(store
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "temp", Some("temp-op"))
            .unwrap()
            .is_none());
        assert_eq!(
            store.skipped_reason_for_operation(DEFAULT_SESSION_ID, "temp-op", Some(&path)),
            Some(BackupSkippedReason::TempPath)
        );
        assert!(backup_skipped_totals().1 >= before_temp + 1);

        let mut disabled = BackupStore::new();
        disabled.set_policy(BackupPolicy {
            max_file_size: Some(0),
            ..BackupPolicy::default()
        });
        assert!(disabled
            .snapshot_with_op(DEFAULT_SESSION_ID, &path, "disabled", Some("disabled-op"))
            .unwrap()
            .is_none());
        assert_eq!(
            disabled.skipped_reason_for_operation(DEFAULT_SESSION_ID, "disabled-op", Some(&path)),
            Some(BackupSkippedReason::Disabled)
        );
    }

    fn backup_content_names(dir: &Path) -> HashSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| name.starts_with("bak_") && name.ends_with(".bak"))
            .collect()
    }
}
