//! Process-wide accounting for SQLite connections opened by AFT-owned seams.
//!
//! The counter deliberately follows connection lifetime rather than query traffic:
//! leaked SQLite handles keep file descriptors, WAL state, and page caches alive even
//! while idle. Callers must use [`TrackedConnection`] at one of the documented seams.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::write_ledger::{Counter as WriteCounter, Domain as WriteDomain};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

pub const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;

/// Layout of the WAL-index header: two identical 48-byte copies, the second of
/// which is a torn-read guard, followed by the checkpoint-info block.
const WAL_INDEX_HEADER_BYTES: usize = 48;
const WAL_INDEX_INITIALISED_OFFSET: usize = 12;
const WAL_INDEX_MX_FRAME_OFFSET: usize = 16;
const WAL_INDEX_SALT_OFFSET: usize = 32;
const WAL_INDEX_BACKFILL_OFFSET: usize = 2 * WAL_INDEX_HEADER_BYTES;
const WAL_INDEX_HEADER_PREFIX_BYTES: usize = WAL_INDEX_BACKFILL_OFFSET + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalCheckpointMode {
    Passive,
    Truncate,
}

impl WalCheckpointMode {
    const fn pragma_sql(self) -> &'static str {
        match self {
            Self::Passive => "PRAGMA wal_checkpoint(PASSIVE)",
            Self::Truncate => "PRAGMA wal_checkpoint(TRUNCATE)",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalCheckpointResult {
    pub busy: i64,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

/// Names each production connection-opening seam so health can attribute a live
/// connection without exposing cache-key paths in the process-wide report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqliteStore {
    AftDb,
    BlobStore,
    CallgraphGeneration,
    CallgraphColdGeneration,
    InspectScopeCache,
    BreakerFile,
    /// A deliberately unmapped seam remains attributable instead of silently
    /// disappearing from the ledger.
    Unmapped(&'static str),
}

impl SqliteStore {
    pub const ALL: [Self; 6] = [
        Self::AftDb,
        Self::BlobStore,
        Self::CallgraphGeneration,
        Self::CallgraphColdGeneration,
        Self::InspectScopeCache,
        Self::BreakerFile,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::AftDb => "aft.db",
            Self::BlobStore => "blob_stores",
            Self::CallgraphGeneration => "callgraph_generations",
            Self::CallgraphColdGeneration => "callgraph_cold_generations",
            Self::InspectScopeCache => "inspect_scope_caches",
            Self::BreakerFile => "breaker_files",
            Self::Unmapped(label) => label,
        }
    }

    pub const fn write_domain(self) -> Option<WriteDomain> {
        match self {
            Self::AftDb | Self::BreakerFile => Some(WriteDomain::AftDb),
            Self::BlobStore => Some(WriteDomain::ViewsBlob),
            Self::CallgraphGeneration => Some(WriteDomain::CallgraphRefresh),
            Self::CallgraphColdGeneration => Some(WriteDomain::CallgraphCold),
            Self::InspectScopeCache => Some(WriteDomain::InspectCache),
            Self::Unmapped(_) => None,
        }
    }

    const fn checkpoint_domain(self) -> Option<WriteDomain> {
        match self {
            Self::CallgraphGeneration | Self::CallgraphColdGeneration => {
                Some(WriteDomain::CallgraphCheckpoint)
            }
            _ => self.write_domain(),
        }
    }
}

/// A store/count row is used instead of a map so status consumers can retain a
/// stable schema even when every tracked count is zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SqliteStoreCount {
    pub store: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SqliteConnectionSnapshot {
    pub open_connections: u64,
    pub open_by_store: Vec<SqliteStoreCount>,
    /// Production openers are all routed through the tracked seams below. Keep
    /// this documented list explicit so a future exception cannot silently make
    /// the process-wide count incomplete.
    pub uninstrumented_openers: Vec<String>,
}

/// Production `rusqlite::Connection::open*` call sites that intentionally do
/// not pass through [`TrackedConnection`]. Read-only probes are listed because
/// they retain SQLite's built-in WAL policy; write-capable seams name the
/// accounting mechanism or residual explicitly. These seams have identity-only
/// RAII records in `file_identity`; this list concerns WAL/health accounting,
/// not the database replacement detector.
pub const SQLITE_UNINSTRUMENTED_OPENERS: &[&str] = &[
    "alias::AliasStore::open: WAL writer retains SQLite's built-in autocheckpoint and remains a named residual",
    "alias::ManifestSqliteStore::open: rollback-journal bytes remain a named residual",
    "gc::sweep_plane: WAL blob-store writer retains SQLite's built-in autocheckpoint and remains a named residual",
    "views::assembly::assemble: derived WAL keeper; checkpoint credited by views::generation::checkpoint_derived",
    "views::generation::checkpoint_derived: raw WAL connection; explicit checkpoint credited at call site",
    "views::materialization::materialize: raw WAL connection; publication phase process-I/O attribution",
    "views::ViewStore::open_pointer_connection: raw WAL connection; publication phase process-I/O attribution",
    "path_status::PathStatusStore::open_at: rollback-journal bytes remain a named residual",
    "commands::semantic_search::view_semantic_search: read-only WAL opener retains SQLite's built-in policy",
];

fn live_counts() -> &'static Mutex<BTreeMap<SqliteStore, u64>> {
    static COUNTS: OnceLock<Mutex<BTreeMap<SqliteStore, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn register_open(store: SqliteStore) {
    let mut counts = live_counts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *counts.entry(store).or_default() += 1;
    #[cfg(test)]
    thread_counts::record(store, 1);
}

fn register_close(store: SqliteStore) {
    let mut counts = live_counts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let count = counts.entry(store).or_default();
    *count = count.saturating_sub(1);
    #[cfg(test)]
    thread_counts::record(store, -1);
}

/// Per-thread mirror of the open/close seam. The process-wide counter above is
/// shared with every other test in the binary, which open and close their own
/// connections concurrently, so a test cannot assert a delta against it. This
/// mirror only ever sees the calling thread's own opens and closes.
#[cfg(test)]
pub(crate) mod thread_counts {
    use super::SqliteStore;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    thread_local! {
        static COUNTS: RefCell<BTreeMap<SqliteStore, i64>> = RefCell::new(BTreeMap::new());
        static OPENS: RefCell<BTreeMap<SqliteStore, u64>> = RefCell::new(BTreeMap::new());
    }

    pub(super) fn record(store: SqliteStore, delta: i64) {
        COUNTS.with(|counts| *counts.borrow_mut().entry(store).or_default() += delta);
        if delta > 0 {
            OPENS.with(|counts| *counts.borrow_mut().entry(store).or_default() += 1);
        }
    }

    pub(crate) fn total_opens_on_this_thread(store: SqliteStore) -> u64 {
        OPENS.with(|counts| counts.borrow().get(&store).copied().unwrap_or(0))
    }

    pub(crate) fn open_on_this_thread(store: SqliteStore) -> i64 {
        COUNTS.with(|counts| counts.borrow().get(&store).copied().unwrap_or(0))
    }
}

/// Snapshot the current number of open connections. This takes a short counter
/// lock only; OS/process enumeration remains outside this module.
pub fn connection_snapshot() -> SqliteConnectionSnapshot {
    let counts = live_counts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let open_by_store = SqliteStore::ALL
        .into_iter()
        .map(|store| SqliteStoreCount {
            store: store.label().to_string(),
            count: counts.get(&store).copied().unwrap_or(0),
        })
        .collect::<Vec<_>>();
    let open_connections = open_by_store.iter().map(|row| row.count).sum();
    SqliteConnectionSnapshot {
        open_connections,
        open_by_store,
        uninstrumented_openers: SQLITE_UNINSTRUMENTED_OPENERS
            .iter()
            .map(|opener| (*opener).to_string())
            .collect(),
    }
}

#[derive(Debug)]
struct WalHookState {
    threshold_pages: AtomicI32,
    last_log_frames: AtomicI32,
    last_checkpointed_frames: AtomicI32,
    last_generation_salt: AtomicU64,
    page_size: u64,
    /// The WAL-index path, resolved once at open. Used to tell one WAL
    /// generation from the next; `None` for databases with no WAL-index of
    /// their own, such as in-memory ones.
    wal_index_path: Option<PathBuf>,
    checkpoint_counter: WriteCounter,
}

impl WalHookState {
    fn observe_log(&self, log_frames: i32) {
        let log_frames = log_frames.max(0);
        let previous = self.last_log_frames.swap(log_frames, Ordering::Relaxed);
        if log_frames < previous {
            self.last_checkpointed_frames.store(0, Ordering::Relaxed);
        }
    }

    fn record_checkpoint(&self, log_frames: i32, checkpointed_frames: i32) -> u64 {
        if log_frames < 0 || checkpointed_frames < 0 {
            return 0;
        }
        self.observe_generation();
        self.observe_log(log_frames);
        let previous = self.last_checkpointed_frames.load(Ordering::Relaxed).max(0);
        let checkpointed_frames = checkpointed_frames.max(previous);
        self.last_checkpointed_frames
            .store(checkpointed_frames, Ordering::Relaxed);
        u64::try_from(checkpointed_frames.saturating_sub(previous)).unwrap_or(0)
    }

    /// Rewind the credited-frame baseline when SQLite has started a new WAL.
    ///
    /// Once a checkpoint has copied a whole WAL into the main database, the next
    /// write transaction restarts the log from frame 1 and stamps it with a
    /// fresh salt. Frame numbers alone cannot see that: a restarted log whose
    /// frames run past the previous log's high-water mark looks exactly like the
    /// previous log growing, and subtracting the old mark then throws away every
    /// frame below it. The salt is what distinguishes the two.
    fn observe_generation(&self) {
        let Some(salt) = self
            .wal_index_path
            .as_deref()
            .and_then(wal_index_generation_salt)
        else {
            return;
        };
        if self.last_generation_salt.swap(salt, Ordering::Relaxed) != salt {
            self.last_log_frames.store(0, Ordering::Relaxed);
            self.last_checkpointed_frames.store(0, Ordering::Relaxed);
        }
    }

    fn outstanding_frames(&self) -> u64 {
        let log_frames = self.last_log_frames.load(Ordering::Relaxed).max(0);
        let checkpointed_frames = self
            .last_checkpointed_frames
            .load(Ordering::Relaxed)
            .clamp(0, log_frames);
        u64::try_from(log_frames.saturating_sub(checkpointed_frames)).unwrap_or(0)
    }

    fn reset_after_truncate(&self) {
        self.last_log_frames.store(0, Ordering::Relaxed);
        self.last_checkpointed_frames.store(0, Ordering::Relaxed);
        self.last_generation_salt.store(0, Ordering::Relaxed);
    }

    fn bytes_for_frames(&self, frames: u64) -> u64 {
        frames.saturating_mul(self.page_size)
    }
}

/// Frames in the current WAL that no checkpoint has copied into the main
/// database file yet.
///
/// Both counts come from the WAL-index rather than from the connection, because
/// the same measurement has to be taken after the handle is gone: `PRAGMA
/// wal_checkpoint` is unavailable once SQLite has closed the database, and the
/// close performs one of the checkpoints whose bytes need attributing.
///
/// `mxFrame` is read rather than derived from the WAL file's length. SQLite
/// restarts a drained WAL from frame 1 without shortening the file, so its
/// length also counts frames from earlier generations that have already been
/// copied and credited.
fn outstanding_wal_frames(path: &Path) -> u64 {
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let Some((log_frames, backfilled)) = wal_index_frame_counts(&PathBuf::from(shm)) else {
        return 0;
    };
    log_frames.saturating_sub(backfilled.min(log_frames))
}

/// `mxFrame` and `nBackfill` from the WAL-index: two identical 48-byte header
/// copies, then the checkpoint-info block whose first field is the backfilled
/// frame count.
///
/// Returns `None` for a WAL-index that is absent, unreadable, uninitialised, or
/// caught mid-update, so an unknown state credits nothing rather than guessing.
/// Only the header prefix is read: a WAL-index is as large as its WAL is long,
/// and this runs on every connection close.
fn wal_index_frame_counts(shm_path: &Path) -> Option<(u64, u64)> {
    let header = wal_index_header_prefix(shm_path)?;
    if header[WAL_INDEX_INITIALISED_OFFSET] == 0 {
        return None;
    }
    let frames = |offset: usize| -> u32 {
        u32::from_ne_bytes(
            header[offset..offset + 4]
                .try_into()
                .expect("the field sits inside the WAL-index prefix"),
        )
    };
    let log_frames = frames(WAL_INDEX_MX_FRAME_OFFSET);
    if log_frames != frames(WAL_INDEX_MX_FRAME_OFFSET + WAL_INDEX_HEADER_BYTES) {
        return None;
    }
    Some((
        u64::from(log_frames),
        u64::from(frames(WAL_INDEX_BACKFILL_OFFSET)),
    ))
}

/// The salt SQLite stamps on the current WAL, taken from the WAL-index.
///
/// Returns `None` unless both copies of the WAL-index header agree, so a header
/// caught mid-update is reported as unknown rather than as a new WAL.
fn wal_index_generation_salt(shm_path: &Path) -> Option<u64> {
    let header = wal_index_header_prefix(shm_path)?;
    let salt = |offset: usize| -> u64 {
        u64::from_ne_bytes(
            header[offset..offset + 8]
                .try_into()
                .expect("both salt fields sit inside the WAL-index prefix"),
        )
    };
    let primary = salt(WAL_INDEX_SALT_OFFSET);
    (primary == salt(WAL_INDEX_SALT_OFFSET + WAL_INDEX_HEADER_BYTES)).then_some(primary)
}

fn wal_index_header_prefix(shm_path: &Path) -> Option<[u8; WAL_INDEX_HEADER_PREFIX_BYTES]> {
    use std::io::Read;

    let mut header = [0_u8; WAL_INDEX_HEADER_PREFIX_BYTES];
    let mut file = std::fs::File::open(shm_path).ok()?;
    file.read_exact(&mut header).ok()?;
    Some(header)
}

unsafe extern "C" fn tracked_wal_hook(
    context: *mut c_void,
    db: *mut rusqlite::ffi::sqlite3,
    database_name: *const c_char,
    frame_count: c_int,
) -> c_int {
    let state = unsafe { &*(context.cast::<WalHookState>()) };
    state.observe_log(frame_count);
    if frame_count < state.threshold_pages.load(Ordering::Relaxed) {
        return rusqlite::ffi::SQLITE_OK;
    }

    let mut log_frames = 0;
    let mut checkpointed_frames = 0;
    // This is the same threshold check and PASSIVE checkpoint used by SQLite's
    // sqlite3WalDefaultHook. The custom hook only adds byte attribution.
    let result = unsafe {
        rusqlite::ffi::sqlite3_wal_checkpoint_v2(
            db,
            database_name,
            rusqlite::ffi::SQLITE_CHECKPOINT_PASSIVE,
            &mut log_frames,
            &mut checkpointed_frames,
        )
    };
    if result == rusqlite::ffi::SQLITE_OK {
        let frames = state.record_checkpoint(log_frames, checkpointed_frames);
        state
            .checkpoint_counter
            .credit(0, state.bytes_for_frames(frames));
    }
    // SQLite's built-in autocheckpoint hook deliberately does not fail the
    // already-committed transaction when a PASSIVE checkpoint is busy.
    rusqlite::ffi::SQLITE_OK
}

/// A `rusqlite::Connection` whose lifetime contributes to the process-wide
/// health census. It dereferences to `Connection`, keeping existing query APIs
/// and transaction helpers unchanged while making close accounting automatic.
#[derive(Debug)]
pub struct TrackedConnection {
    connection: Option<Connection>,
    store: SqliteStore,
    write_counter: WriteCounter,
    page_size: u64,
    wal_hook: Box<WalHookState>,
    /// Key this connection is filed under in [`crate::db::file_identity`],
    /// resolved at open time because the database file may be gone by close.
    /// `None` for connections with no file of their own, such as in-memory ones.
    file_identity_key: Option<PathBuf>,
}

impl TrackedConnection {
    pub fn open(path: &Path, store: SqliteStore) -> rusqlite::Result<Self> {
        Self::open_attributed(path, store, path.display().to_string())
    }

    pub fn open_attributed(
        path: &Path,
        store: SqliteStore,
        root_id: impl Into<String>,
    ) -> rusqlite::Result<Self> {
        let _guard = crate::db::file_identity::filesystem_guard();
        Self::from_connection_attributed(Connection::open(path)?, store, root_id)
    }

    pub fn open_with_flags(
        path: &str,
        flags: OpenFlags,
        store: SqliteStore,
    ) -> rusqlite::Result<Self> {
        let _guard = crate::db::file_identity::filesystem_guard();
        Self::from_connection_attributed(
            Connection::open_with_flags(path, flags)?,
            store,
            path.to_owned(),
        )
    }

    pub fn open_path_with_flags(
        path: &Path,
        flags: OpenFlags,
        store: SqliteStore,
    ) -> rusqlite::Result<Self> {
        let _guard = crate::db::file_identity::filesystem_guard();
        Self::from_connection_attributed(
            Connection::open_with_flags(path, flags)?,
            store,
            path.display().to_string(),
        )
    }

    pub fn open_in_memory(store: SqliteStore) -> rusqlite::Result<Self> {
        Self::from_connection_attributed(Connection::open_in_memory()?, store, "<memory>")
    }

    pub fn from_connection(connection: Connection, store: SqliteStore) -> rusqlite::Result<Self> {
        Self::from_connection_attributed(connection, store, "<unknown>")
    }

    pub fn from_connection_attributed(
        connection: Connection,
        store: SqliteStore,
        root_id: impl Into<String>,
    ) -> rusqlite::Result<Self> {
        if matches!(store, SqliteStore::CallgraphGeneration | SqliteStore::CallgraphColdGeneration) {
            // Keep WAL files across the last close: another opener may already
            // have resolved this generation and be about to attach to its index.
            let mut persist: std::ffi::c_int = 1;
            let result = unsafe {
                rusqlite::ffi::sqlite3_file_control(
                    connection.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
                    std::ptr::from_mut(&mut persist).cast(),
                )
            };
            if result != rusqlite::ffi::SQLITE_OK
                && connection.path().is_some_and(|path| !path.is_empty() && path != ":memory:")
            {
                return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(result), None));
            }
        }
        // Register before the first PRAGMA: it can enter WAL recovery and fault
        // on a truncated shared-memory mapping before later registration runs.
        let file_identity_key = connection
            .path()
            .filter(|path| !path.is_empty() && *path != ":memory:")
            .map(|path| crate::db::file_identity::note_open(Path::new(path), store));
        let page_size = connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))
            .unwrap_or(4096);
        let root_id = root_id.into();
        let write_counter = crate::write_ledger::register(
            store.write_domain().unwrap_or(WriteDomain::Other),
            root_id.clone(),
        );
        if store.write_domain().is_none() {
            write_counter.note_seam_label(store.label());
        }
        let checkpoint_counter = crate::write_ledger::register(
            store.checkpoint_domain().unwrap_or(WriteDomain::Other),
            root_id,
        );
        let wal_hook = Box::new(WalHookState {
            threshold_pages: AtomicI32::new(DEFAULT_WAL_AUTOCHECKPOINT_PAGES as i32),
            last_log_frames: AtomicI32::new(0),
            last_checkpointed_frames: AtomicI32::new(0),
            last_generation_salt: AtomicU64::new(0),
            page_size,
            wal_index_path: connection
                .path()
                .filter(|path| !path.is_empty())
                .map(|path| {
                    let mut shm = std::ffi::OsString::from(path);
                    shm.push("-shm");
                    PathBuf::from(shm)
                }),
            checkpoint_counter,
        });
        let context = std::ptr::from_ref(wal_hook.as_ref())
            .cast_mut()
            .cast::<c_void>();
        unsafe {
            rusqlite::ffi::sqlite3_wal_hook(connection.handle(), Some(tracked_wal_hook), context);
        }
        register_open(store);
        let tracked = Self {
            connection: Some(connection),
            store,
            write_counter,
            page_size,
            wal_hook,
            file_identity_key,
        };
        tracked.reset_write_page_sample();
        Ok(tracked)
    }

    fn cache_write_pages(&self, reset: bool) -> u64 {
        let Some(connection) = self.connection.as_ref() else {
            return 0;
        };
        let mut current = 0;
        let mut highwater = 0;
        // SQLite serializes db-status access with the connection mutex. This is
        // sampled only by the connection owner at maintenance boundaries/close.
        let result = unsafe {
            rusqlite::ffi::sqlite3_db_status(
                connection.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
                &mut current,
                &mut highwater,
                i32::from(reset),
            )
        };
        if result == rusqlite::ffi::SQLITE_OK {
            u64::try_from(current).unwrap_or(0)
        } else {
            0
        }
    }

    fn reset_write_page_sample(&self) {
        let _ = self.cache_write_pages(true);
    }

    pub fn sample_write_pages(&self) -> u64 {
        self.sample_write_pages_as(self.write_counter.domain())
    }

    pub fn sample_write_pages_as(&self, domain: WriteDomain) -> u64 {
        let bytes = self.cache_write_pages(true).saturating_mul(self.page_size);
        crate::write_ledger::register(domain, self.write_counter.root_id()).credit(0, bytes);
        bytes
    }

    /// Replace SQLite's built-in WAL autocheckpoint threshold without changing
    /// its threshold or PASSIVE-mode policy. The installed hook owns the checkpoint
    /// so it can attribute bytes copied from the WAL into the main database,
    /// including copies performed outside the usual pager accounting.
    pub fn set_wal_autocheckpoint(&self, pages: i64) -> rusqlite::Result<()> {
        let pages = i32::try_from(pages).map_err(|_| {
            rusqlite::Error::InvalidParameterName(format!(
                "wal_autocheckpoint pages out of range: {pages}"
            ))
        })?;
        if pages <= 0 {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "wal_autocheckpoint pages must be positive: {pages}"
            )));
        }
        self.wal_hook
            .threshold_pages
            .store(pages, Ordering::Relaxed);
        Ok(())
    }

    pub fn wal_autocheckpoint_pages(&self) -> i64 {
        i64::from(self.wal_hook.threshold_pages.load(Ordering::Relaxed))
    }

    fn query_wal_checkpoint(
        &self,
        mode: WalCheckpointMode,
    ) -> rusqlite::Result<WalCheckpointResult> {
        self.connection
            .as_ref()
            .expect("tracked SQLite connection accessed after drop")
            .query_row(mode.pragma_sql(), [], |row| {
                Ok(WalCheckpointResult {
                    busy: row.get(0)?,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
    }

    fn credit_checkpoint_result_as(&self, result: WalCheckpointResult, domain: WriteDomain) -> u64 {
        let frames = self.wal_hook.record_checkpoint(
            i32::try_from(result.log_frames).unwrap_or(-1),
            i32::try_from(result.checkpointed_frames).unwrap_or(-1),
        );
        let bytes = self.wal_hook.bytes_for_frames(frames);
        crate::write_ledger::register(domain, self.write_counter.root_id()).credit(0, bytes);
        bytes
    }

    fn checkpoint_outstanding_frames(&self) -> u64 {
        let Some(path) = self
            .connection
            .as_ref()
            .and_then(Connection::path)
            .map(Path::new)
        else {
            return self.wal_hook.outstanding_frames();
        };
        outstanding_wal_frames(path)
    }

    /// Run an explicit WAL checkpoint and credit only frames newly copied into
    /// the main database. SQLite zeros the frame counts after a successful
    /// TRUNCATE, so that mode snapshots the WAL-index backfill counter before
    /// running the requested checkpoint once with its original busy policy.
    pub fn checkpoint_wal_as(
        &self,
        mode: WalCheckpointMode,
        domain: WriteDomain,
    ) -> rusqlite::Result<WalCheckpointResult> {
        if mode == WalCheckpointMode::Truncate {
            let outstanding = self.checkpoint_outstanding_frames();
            let result = self.query_wal_checkpoint(WalCheckpointMode::Truncate)?;
            if result.busy == 0 {
                crate::write_ledger::register(domain, self.write_counter.root_id())
                    .credit(0, self.wal_hook.bytes_for_frames(outstanding));
                self.wal_hook.reset_after_truncate();
            } else {
                self.credit_checkpoint_result_as(result, domain);
            }
            Ok(result)
        } else {
            let result = self.query_wal_checkpoint(mode)?;
            self.credit_checkpoint_result_as(result, domain);
            Ok(result)
        }
    }

    /// Snapshot the frames a close-time checkpoint would have to copy.
    ///
    /// Taken before the handle is dropped, because without persistent WAL,
    /// SQLite deletes the sidecars and the count is unreadable after close.
    fn close_checkpoint_probe(&self) -> Option<CloseCheckpointProbe> {
        let path = PathBuf::from(self.connection.as_ref()?.path()?);
        let outstanding_frames = outstanding_wal_frames(&path);
        (outstanding_frames > 0).then_some(CloseCheckpointProbe {
            path,
            outstanding_frames,
        })
    }

    /// Credit the bytes the close-time checkpoint copied into the main database.
    ///
    /// Closing the last connection to a WAL database makes SQLite run a full
    /// checkpoint (retaining the sidecars when persistent WAL is enabled). That
    /// copy is neither the autocheckpoint
    /// the WAL hook models (the hook only runs when a commit appends frames) nor
    /// an explicit `wal_checkpoint` call, so without this the main-file bytes it
    /// pushes are never attributed to anything.
    ///
    /// The frames are re-counted after the handle is gone instead of assumed:
    /// that is what separates a close which did checkpoint (frames backfilled,
    /// WAL emptied or deleted) from one which could not, because another connection to the same
    /// database is still open or this handle was read-only. A close that
    /// checkpointed nothing must credit nothing.
    fn credit_close_checkpoint(&self, probe: Option<CloseCheckpointProbe>) -> u64 {
        let Some(probe) = probe else {
            return 0;
        };
        let remaining = outstanding_wal_frames(&probe.path);
        let backfilled = probe.outstanding_frames.saturating_sub(remaining);
        if backfilled == 0 {
            return 0;
        }
        let bytes = self.wal_hook.bytes_for_frames(backfilled);
        self.wal_hook.checkpoint_counter.credit(0, bytes);
        bytes
    }
}

/// The WAL state captured just before a tracked handle closes, so the frames
/// SQLite's close-time checkpoint copies can be measured across the close.
#[derive(Debug)]
struct CloseCheckpointProbe {
    path: PathBuf,
    outstanding_frames: u64,
}

impl Deref for TrackedConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("tracked SQLite connection accessed after drop")
    }
}

impl DerefMut for TrackedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("tracked SQLite connection accessed after drop")
    }
}

impl Drop for TrackedConnection {
    fn drop(&mut self) {
        self.sample_write_pages();
        let close_checkpoint = self.close_checkpoint_probe();
        // Drop the SQLite handle before decrementing so the counter never says
        // closed while rusqlite still owns the descriptor and page cache.
        drop(self.connection.take());
        self.credit_close_checkpoint(close_checkpoint);
        if let Some(key) = self.file_identity_key.take() {
            crate::db::file_identity::note_close(&key, self.store);
        }
        register_close(self.store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_count(snapshot: &SqliteConnectionSnapshot, store: SqliteStore) -> u64 {
        snapshot
            .open_by_store
            .iter()
            .find(|row| row.store == store.label())
            .expect("every store has a snapshot row")
            .count
    }

    #[test]
    fn tracked_connection_counts_open_and_close_at_each_seam() {
        use super::thread_counts::open_on_this_thread;
        let baseline_aft = open_on_this_thread(SqliteStore::AftDb);
        let baseline_callgraph = open_on_this_thread(SqliteStore::CallgraphGeneration);
        let dir = tempfile::tempdir().expect("tempdir");
        let aft = TrackedConnection::open(&dir.path().join("aft.db"), SqliteStore::AftDb)
            .expect("open aft db");
        let callgraph = TrackedConnection::open(
            &dir.path().join("graph.sqlite"),
            SqliteStore::CallgraphGeneration,
        )
        .expect("open callgraph");
        assert_eq!(open_on_this_thread(SqliteStore::AftDb), baseline_aft + 1);
        assert_eq!(
            open_on_this_thread(SqliteStore::CallgraphGeneration),
            baseline_callgraph + 1
        );
        // The process-wide snapshot must at least contain this thread's opens;
        // it may also contain other tests' connections, so only a lower bound
        // is a stable assertion here.
        let snapshot = connection_snapshot();
        assert!(store_count(&snapshot, SqliteStore::AftDb) >= 1);
        assert!(store_count(&snapshot, SqliteStore::CallgraphGeneration) >= 1);
        assert!(snapshot.open_connections >= 2);

        drop(aft);
        assert_eq!(open_on_this_thread(SqliteStore::AftDb), baseline_aft);
        assert_eq!(
            open_on_this_thread(SqliteStore::CallgraphGeneration),
            baseline_callgraph + 1
        );

        drop(callgraph);
        assert_eq!(open_on_this_thread(SqliteStore::AftDb), baseline_aft);
        assert_eq!(
            open_on_this_thread(SqliteStore::CallgraphGeneration),
            baseline_callgraph
        );
    }

    #[test]
    fn callgraph_refresh_credits_physical_pages_in_wal_frame_order_of_magnitude() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("callgraph.sqlite");
        let root = format!("/callgraph-ledger/{}", std::process::id());
        let conn = TrackedConnection::open_attributed(
            &path,
            SqliteStore::CallgraphGeneration,
            root.clone(),
        )
        .unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE nodes(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        conn.sample_write_pages();
        let tx = conn.unchecked_transaction().unwrap();
        for id in 0..128_u64 {
            tx.execute(
                "INSERT INTO nodes(id, payload) VALUES (?1, zeroblob(1024))",
                [id],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let physical = conn.sample_write_pages();
        assert_eq!(
            conn.sample_write_pages(),
            0,
            "an idle follow-up sample must not recount prior page writes"
        );
        let page_size: u64 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let wal_bytes = std::fs::metadata(std::path::PathBuf::from(wal))
            .unwrap()
            .len();
        let frames = wal_bytes.saturating_sub(32) / (page_size + 24);
        assert!(physical > 0, "callgraph refresh must credit physical bytes");
        assert!(frames > 0, "fixture must retain WAL frames");
        let credited_pages = physical / page_size;
        assert!(
            credited_pages.saturating_mul(4) >= frames
                && credited_pages <= frames.saturating_mul(4),
            "credited pages={credited_pages}, WAL frames={frames}"
        );
        let credited = crate::write_ledger::pending_for_test(
            crate::write_ledger::Domain::CallgraphRefresh,
            &root,
        )
        .1;
        let other =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::Other, &root).1;
        let labels =
            crate::write_ledger::seam_labels_for_test(crate::write_ledger::Domain::Other, &root);
        assert!(
            credited >= physical,
            "CallgraphRefresh credited={credited}, expected at least {physical}; Other credited={other}, labels={labels:?}"
        );
    }

    fn wal_path(path: &Path) -> std::path::PathBuf {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        wal.into()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WalWorkloadMetrics {
        checkpoints: u64,
        wal_high_water: u64,
        cache_write_bytes: u64,
        backfill_bytes: u64,
    }

    fn shm_checkpoint_state(path: &Path) -> (u32, u32) {
        // Reading mxFrame/nBackfill from the WAL-index is observational. Using
        // PRAGMA wal_checkpoint(PASSIVE) here would itself move the quantity the
        // parity test is intended to measure.
        let mut shm = path.as_os_str().to_owned();
        shm.push("-shm");
        let bytes = std::fs::read(std::path::PathBuf::from(shm)).unwrap();
        let mx_frame = u32::from_ne_bytes(bytes[16..20].try_into().unwrap());
        let duplicate_mx_frame = u32::from_ne_bytes(bytes[64..68].try_into().unwrap());
        assert_eq!(mx_frame, duplicate_mx_frame, "unstable WAL-index header");
        let backfilled = u32::from_ne_bytes(bytes[96..100].try_into().unwrap());
        (mx_frame, backfilled)
    }

    fn raw_cache_write_bytes(connection: &Connection, page_size: u64, reset: bool) -> u64 {
        let mut current = 0;
        let mut highwater = 0;
        let result = unsafe {
            rusqlite::ffi::sqlite3_db_status(
                connection.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
                &mut current,
                &mut highwater,
                i32::from(reset),
            )
        };
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        u64::try_from(current).unwrap().saturating_mul(page_size)
    }

    fn run_fixed_wal_workload(connection: &Connection, path: &Path) -> (u64, u64, u64) {
        let mut checkpoints = 0_u64;
        let mut wal_high_water = 0_u64;
        let mut backfilled_frames = 0_u64;
        for batch in 0..12_i64 {
            let tx = connection.unchecked_transaction().unwrap();
            for offset in 0..3_i64 {
                tx.execute(
                    "INSERT INTO payloads(id, payload) VALUES (?1, zeroblob(3500))",
                    [batch * 3 + offset],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            wal_high_water = wal_high_water.max(std::fs::metadata(wal_path(path)).unwrap().len());
            let (log_frames, checkpointed_frames) = shm_checkpoint_state(path);
            if log_frames > 0 && checkpointed_frames == log_frames {
                checkpoints += 1;
                backfilled_frames = backfilled_frames.saturating_add(u64::from(log_frames));
            }
        }
        (checkpoints, wal_high_water, backfilled_frames)
    }

    // This fixed-workload byte-identity test proves the hook is behaviour-preserving
    // where the checkpoint predicate is deterministic (the mechanism). The live
    // fs_usage before/after proves the same policy under real writer interleaving
    // (the population). Either proof can pass while the other fails, so neither is
    // a duplicate of the other.
    #[test]
    fn tracked_wal_hook_matches_builtin_checkpoint_frequency_size_and_bytes() {
        const THRESHOLD: i64 = 8;
        let dir = tempfile::tempdir().unwrap();
        let builtin_path = dir.path().join("builtin.sqlite");
        let builtin = Connection::open(&builtin_path).unwrap();
        builtin.pragma_update(None, "journal_mode", "WAL").unwrap();
        builtin
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        builtin
            .execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        builtin
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let page_size: u64 = builtin
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        raw_cache_write_bytes(&builtin, page_size, true);
        builtin
            .pragma_update(None, "wal_autocheckpoint", THRESHOLD)
            .unwrap();
        let (checkpoints, wal_high_water, backfilled_frames) =
            run_fixed_wal_workload(&builtin, &builtin_path);
        let builtin_metrics = WalWorkloadMetrics {
            checkpoints,
            wal_high_water,
            cache_write_bytes: raw_cache_write_bytes(&builtin, page_size, true),
            backfill_bytes: backfilled_frames.saturating_mul(page_size),
        };

        let tracked_path = dir.path().join("tracked.sqlite");
        let root = format!("/wal-parity/{}", std::process::id());
        let tracked =
            TrackedConnection::open_attributed(&tracked_path, SqliteStore::AftDb, root.clone())
                .unwrap();
        tracked.pragma_update(None, "journal_mode", "WAL").unwrap();
        tracked.set_wal_autocheckpoint(1_000).unwrap();
        tracked
            .execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        tracked
            .checkpoint_wal_as(WalCheckpointMode::Truncate, WriteDomain::AftDb)
            .unwrap();
        tracked.cache_write_pages(true);
        let credited_before = crate::write_ledger::pending_for_test(WriteDomain::AftDb, &root).1;
        tracked.set_wal_autocheckpoint(THRESHOLD).unwrap();
        let (checkpoints, wal_high_water, observed_backfilled_frames) =
            run_fixed_wal_workload(&tracked, &tracked_path);
        let credited_after = crate::write_ledger::pending_for_test(WriteDomain::AftDb, &root).1;
        let tracked_metrics = WalWorkloadMetrics {
            checkpoints,
            wal_high_water,
            cache_write_bytes: tracked.cache_write_pages(true).saturating_mul(page_size),
            backfill_bytes: credited_after.saturating_sub(credited_before),
        };

        assert!(
            builtin_metrics.checkpoints > 0 && builtin_metrics.backfill_bytes > 0,
            "fixed workload did not cross the autocheckpoint threshold"
        );
        assert_eq!(
            tracked_metrics.backfill_bytes,
            observed_backfilled_frames.saturating_mul(page_size),
            "the hook credit must equal the WAL-index backfill movement"
        );
        assert_eq!(tracked_metrics, builtin_metrics);
    }

    #[test]
    fn forced_wal_checkpoint_credits_cache_writes_and_main_file_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint-credit.sqlite");
        let root = format!("/checkpoint-credit/{}", std::process::id());
        let conn =
            TrackedConnection::open_attributed(&path, SqliteStore::AftDb, root.clone()).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.sample_write_pages();

        let page_size: u64 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let main_before = std::fs::metadata(&path).unwrap().len();
        let credited_before = crate::write_ledger::pending_for_test(WriteDomain::AftDb, &root).1;
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        for id in 0..64_i64 {
            tx.execute(
                "INSERT INTO payloads(id, payload) VALUES (?1, zeroblob(3500))",
                [id],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let wal_bytes = std::fs::metadata(wal_path(&path)).unwrap().len();
        let wal_frames = wal_bytes.saturating_sub(32) / (page_size + 24);
        let cache_write_bytes = conn.sample_write_pages();
        let checkpoint = conn
            .checkpoint_wal_as(WalCheckpointMode::Passive, WriteDomain::AftDb)
            .unwrap();
        let main_after = std::fs::metadata(&path).unwrap().len();
        let credited_after = crate::write_ledger::pending_for_test(WriteDomain::AftDb, &root).1;
        let credited = credited_after.saturating_sub(credited_before);
        let expected = main_after
            .saturating_sub(main_before)
            .saturating_add(wal_frames.saturating_mul(page_size));

        assert!(wal_frames > 0, "fixture did not write WAL frames");
        assert!(checkpoint.checkpointed_frames > 0);
        assert!(cache_write_bytes > 0);
        assert!(
            credited.abs_diff(expected) <= page_size,
            "credited={credited}, expected={expected}, main_growth={}, WAL frames={wal_frames}",
            main_after.saturating_sub(main_before)
        );
    }

    #[test]
    fn closing_the_last_wal_connection_credits_the_frames_it_backfills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("close-checkpoint.sqlite");
        let root = format!("/close-checkpoint/{}", std::process::id());
        let credited_before =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1;

        let (page_size, outstanding_frames, main_before) = {
            let conn = TrackedConnection::open_attributed(
                &path,
                SqliteStore::CallgraphGeneration,
                root.clone(),
            )
            .unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            // Park the autocheckpoint threshold out of reach so the WAL hook
            // credits nothing during the workload. Whatever the ledger ends up
            // holding for this root is then attributable to the close alone.
            conn.set_wal_autocheckpoint(1_000_000).unwrap();
            let page_size: u64 = conn
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .unwrap();

            // One transaction, so each database page reaches the WAL once and
            // the frame count the credit uses is also the page count the
            // checkpoint pushes into the main file.
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
                .unwrap();
            for id in 0..256_i64 {
                tx.execute(
                    "INSERT INTO payloads(id, payload) VALUES (?1, zeroblob(3500))",
                    [id],
                )
                .unwrap();
            }
            tx.commit().unwrap();

            let main_before = std::fs::metadata(&path).unwrap().len();
            let (log_frames, backfilled) = shm_checkpoint_state(&path);
            assert!(log_frames > 0, "fixture wrote no WAL frames");
            assert_eq!(
                backfilled, 0,
                "fixture checkpointed before the close; the close has nothing left to credit"
            );
            assert_eq!(
                crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1,
                credited_before,
                "no checkpoint should have been credited before the close"
            );
            (page_size, u64::from(log_frames - backfilled), main_before)
        };

        let credited =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root)
                .1
                .saturating_sub(credited_before);
        let main_growth = std::fs::metadata(&path).unwrap().len() - main_before;

        // Persistent WAL retains the sidecars after close. Check the backfill
        // count and query a copy of the main file alone to prove its contents
        // reached the database rather than merely surviving in the WAL.
        assert!(wal_path(&path).exists());
        let (log_frames, backfilled) = shm_checkpoint_state(&path);
        assert_eq!(log_frames, backfilled);
        assert_eq!(
            credited,
            outstanding_frames * page_size,
            "close-checkpoint credit must equal the frames it backfilled \
             (frames={outstanding_frames}, page_size={page_size}, main_growth={main_growth})"
        );
        assert!(
            main_growth.abs_diff(credited) <= 4 * page_size,
            "credited {credited} bytes but the main file grew by {main_growth}"
        );
        let standalone = dir.path().join("standalone.sqlite");
        std::fs::copy(&path, &standalone).unwrap();
        let reopened = Connection::open(&standalone).unwrap();
        let rows: i64 = reopened
            .query_row("SELECT COUNT(*) FROM payloads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 256);
    }

    /// The close-time credit reads the WAL and WAL-index files and issues no
    /// SQL, so it must not move SQLite's checkpoint policy. Running the same
    /// workload through a plain connection and a tracked one pins that: both
    /// have to reach the close having checkpointed the same number of times and
    /// leave the same main file behind.
    #[test]
    fn crediting_the_close_checkpoint_does_not_add_a_checkpoint() {
        const THRESHOLD: i64 = 8;
        let dir = tempfile::tempdir().unwrap();

        let builtin_path = dir.path().join("builtin-close.sqlite");
        let builtin_checkpoints = {
            let builtin = Connection::open(&builtin_path).unwrap();
            builtin.pragma_update(None, "journal_mode", "WAL").unwrap();
            builtin
                .execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
                .unwrap();
            builtin
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
            builtin
                .pragma_update(None, "wal_autocheckpoint", THRESHOLD)
                .unwrap();
            run_fixed_wal_workload(&builtin, &builtin_path).0
        };

        let tracked_path = dir.path().join("tracked-close.sqlite");
        let root = format!("/close-parity/{}", std::process::id());
        let tracked_checkpoints = {
            let tracked = TrackedConnection::open_attributed(
                &tracked_path,
                SqliteStore::CallgraphGeneration,
                root.clone(),
            )
            .unwrap();
            tracked.pragma_update(None, "journal_mode", "WAL").unwrap();
            tracked
                .execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
                .unwrap();
            tracked
                .checkpoint_wal_as(
                    WalCheckpointMode::Truncate,
                    WriteDomain::CallgraphCheckpoint,
                )
                .unwrap();
            tracked.set_wal_autocheckpoint(THRESHOLD).unwrap();
            run_fixed_wal_workload(&tracked, &tracked_path).0
        };

        assert!(
            builtin_checkpoints > 0,
            "fixed workload did not cross the autocheckpoint threshold"
        );
        assert_eq!(
            tracked_checkpoints, builtin_checkpoints,
            "crediting the close changed how often SQLite checkpoints"
        );
        assert!(!wal_path(&builtin_path).exists());
        assert!(wal_path(&tracked_path).exists());
        let (log_frames, backfilled) = shm_checkpoint_state(&tracked_path);
        assert_eq!(log_frames, backfilled, "persistent WAL still needs a checkpoint");
        assert_eq!(
            std::fs::metadata(&builtin_path).unwrap().len(),
            std::fs::metadata(&tracked_path).unwrap().len(),
            "the tracked close wrote a different amount into the main file"
        );
    }

    /// A restarted WAL whose frames run past the previous WAL's high-water mark
    /// is the case a frame-number comparison cannot see, so it is the case that
    /// silently dropped the frames below that mark. Two transactions, the second
    /// larger than the first, reproduce it.
    #[test]
    fn credit_survives_a_wal_restart_into_a_longer_log() {
        const THRESHOLD: i64 = 8;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-restart.sqlite");
        let root = format!("/wal-restart/{}", std::process::id());
        let conn = TrackedConnection::open_attributed(
            &path,
            SqliteStore::CallgraphGeneration,
            root.clone(),
        )
        .unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE payloads(id INTEGER PRIMARY KEY, payload BLOB);")
            .unwrap();
        conn.checkpoint_wal_as(
            WalCheckpointMode::Truncate,
            WriteDomain::CallgraphCheckpoint,
        )
        .unwrap();
        let page_size: u64 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        conn.set_wal_autocheckpoint(THRESHOLD).unwrap();

        let credited_before =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1;
        let mut backfilled_frames = 0_u64;
        let mut log_lengths = Vec::new();
        let mut next_id = 0_i64;
        for rows in [16_i64, 48] {
            let tx = conn.unchecked_transaction().unwrap();
            for _ in 0..rows {
                tx.execute(
                    "INSERT INTO payloads(id, payload) VALUES (?1, zeroblob(3500))",
                    [next_id],
                )
                .unwrap();
                next_id += 1;
            }
            tx.commit().unwrap();
            let (log_frames, backfilled) = shm_checkpoint_state(&path);
            // Each commit's autocheckpoint has to drain its WAL completely,
            // otherwise SQLite never restarts the log and the case under test
            // does not arise.
            assert_eq!(
                log_frames, backfilled,
                "commit of {rows} rows left an undrained WAL"
            );
            backfilled_frames += u64::from(backfilled);
            log_lengths.push(log_frames);
        }
        let credited =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root)
                .1
                .saturating_sub(credited_before);

        assert!(
            log_lengths[1] > log_lengths[0],
            "the second log must outgrow the first for this to test anything: {log_lengths:?}"
        );
        assert_eq!(
            credited,
            backfilled_frames * page_size,
            "credit lost the frames below the previous log's high-water mark \
             (logs={log_lengths:?}, frames={backfilled_frames})"
        );
    }

    #[test]
    fn unmapped_sqlite_seam_lands_in_other_and_reports_its_label() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = format!("/unmapped-ledger/{}", std::process::id());
        let label = "fixture_unmapped_callgraph";
        let conn = TrackedConnection::open_attributed(
            &dir.path().join("unknown.sqlite"),
            SqliteStore::Unmapped(label),
            root.clone(),
        )
        .unwrap();
        conn.execute_batch("CREATE TABLE writes(value TEXT); INSERT INTO writes VALUES ('x');")
            .unwrap();
        conn.sample_write_pages();
        assert!(
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::Other, &root).1 > 0
        );
        assert_eq!(
            crate::write_ledger::seam_labels_for_test(crate::write_ledger::Domain::Other, &root,),
            vec![label.to_owned()]
        );
    }

    #[test]
    fn every_uninstrumented_production_opener_is_documented() {
        assert!(SQLITE_UNINSTRUMENTED_OPENERS.iter().any(|opener| {
            opener.contains("path_status::PathStatusStore::open_at")
                && opener.contains("rollback-journal")
        }));
        assert!(SQLITE_UNINSTRUMENTED_OPENERS.iter().any(|opener| {
            opener.contains("views::generation::checkpoint_derived")
                && opener.contains("credited at call site")
        }));
    }
}
