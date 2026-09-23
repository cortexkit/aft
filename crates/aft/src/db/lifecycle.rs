//! Process-wide accounting for SQLite connections opened by AFT-owned seams.
//!
//! The counter deliberately follows connection lifetime rather than query traffic:
//! leaked SQLite handles keep file descriptors, WAL state, and page caches alive even
//! while idle. Callers must use [`TrackedConnection`] at one of the documented seams.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::write_ledger::{Counter as WriteCounter, Domain as WriteDomain};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

pub const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;

const CLOSE_CHECKPOINT_SEAM: &str = "db::TrackedConnection::drop";
const CLOSE_CHECKPOINT_REASON: &str =
    "the close-time checkpoint result is unavailable without reopening the live SQLite file set, which would release this process's POSIX locks";
const CLOSE_CHECKPOINT_ESTIMATE_BASIS: &str =
    "upper-bound estimate from outstanding WAL frames observed by the existing hook times the database page size; repeated database pages can make the actual backfill smaller";
const AMBIGUOUS_WAL_RESTART_SEAM: &str = "db::tracked_wal_hook";
const AMBIGUOUS_WAL_RESTART_REASON: &str =
    "commit frame counts cannot distinguish append growth from a restarted WAL that grew past the prior generation";

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
            // The cold-build staging connection writes `<key>.staging.sqlite.tmp.resume`
            // and its WAL for minutes before the finished build is published under a
            // generation name. Its measured pages get their own domain so a census
            // window mid-build attributes them instead of leaving them unexplained.
            Self::CallgraphColdGeneration => Some(WriteDomain::CallgraphColdStaging),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UninstrumentedOpenerClass {
    Unmeasurable,
    AttributedElsewhere,
    NoWrites,
}

impl UninstrumentedOpenerClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unmeasurable => "unmeasurable",
            Self::AttributedElsewhere => "attributed_elsewhere",
            Self::NoWrites => "no_writes",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqliteUninstrumentedOpener {
    pub seam: &'static str,
    pub class: UninstrumentedOpenerClass,
    pub reason: &'static str,
}

/// Production raw SQLite openers, classified from the operations at each site.
/// They still register connection identity so replacement protection sees their
/// lock lifetimes, but they do not install WAL byte-attribution hooks. The class
/// says whether each opener can produce unmeasurable write bytes.
pub const SQLITE_UNINSTRUMENTED_OPENERS: &[SqliteUninstrumentedOpener] = &[
    SqliteUninstrumentedOpener {
        seam: "alias::AliasStore::open",
        class: UninstrumentedOpenerClass::Unmeasurable,
        reason: "the raw WAL writer uses SQLite's built-in autocheckpoint and installs no attribution hook",
    },
    SqliteUninstrumentedOpener {
        seam: "alias::ManifestSqliteStore::open",
        class: UninstrumentedOpenerClass::Unmeasurable,
        reason: "the raw rollback-journal connection exposes no pager byte counter",
    },
    SqliteUninstrumentedOpener {
        seam: "gc::sweep_plane",
        class: UninstrumentedOpenerClass::Unmeasurable,
        reason: "the raw blob-store WAL writer uses SQLite's built-in autocheckpoint and installs no attribution hook",
    },
    SqliteUninstrumentedOpener {
        seam: "views::assembly::assemble",
        class: UninstrumentedOpenerClass::NoWrites,
        reason: "the connection only keeps the derived WAL alive and reads sqlite_schema; checkpointing is classified at checkpoint_derived",
    },
    SqliteUninstrumentedOpener {
        seam: "views::generation::clone_derived source",
        class: UninstrumentedOpenerClass::NoWrites,
        reason: "the SQLite backup source is read-only at this seam",
    },
    SqliteUninstrumentedOpener {
        seam: "views::generation::clone_derived destination",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the enclosing publication clone phase credits its process-I/O delta to views_derived",
    },
    SqliteUninstrumentedOpener {
        seam: "views::generation::checkpoint_derived",
        class: UninstrumentedOpenerClass::Unmeasurable,
        reason: "a successful TRUNCATE checkpoint returns zero frame counts and the raw connection has no earlier WAL-hook baseline",
    },
    SqliteUninstrumentedOpener {
        seam: "views::materialization::materialize",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the enclosing publication materialize phase credits its process-I/O delta to views_derived",
    },
    SqliteUninstrumentedOpener {
        seam: "views::materialization::blob_reader",
        class: UninstrumentedOpenerClass::NoWrites,
        reason: "the materializer uses this connection only to read immutable blob payloads",
    },
    SqliteUninstrumentedOpener {
        seam: "views::ViewStore::open_pointer_connection",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the enclosing publication closure phase credits the pointer transaction's process-I/O delta to views_closure",
    },
    SqliteUninstrumentedOpener {
        seam: "views::sync_database_with_passive_checkpoint",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the enclosing publication closure phase credits the durability checkpoint's process-I/O delta to views_closure",
    },
    SqliteUninstrumentedOpener {
        seam: "views::checkpoint_and_sync_database",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the enclosing publication closure phase credits the durability checkpoint's process-I/O delta to views_closure",
    },
    SqliteUninstrumentedOpener {
        seam: "views::checkpoint_pointer_after_cas",
        class: UninstrumentedOpenerClass::AttributedElsewhere,
        reason: "the publication profile remains in its closure phase until the pointer checkpoint completes",
    },
    SqliteUninstrumentedOpener {
        seam: "path_status::PathStatusStore::open_at",
        class: UninstrumentedOpenerClass::Unmeasurable,
        reason: "the raw rollback-journal connection exposes no pager byte counter",
    },
    SqliteUninstrumentedOpener {
        seam: "commands::semantic_search::view_semantic_search",
        class: UninstrumentedOpenerClass::NoWrites,
        reason: "the connection is opened read-only and executes only search queries",
    },
];

pub(crate) fn uninstrumented_opener(seam: &str) -> Option<SqliteUninstrumentedOpener> {
    SQLITE_UNINSTRUMENTED_OPENERS
        .iter()
        .copied()
        .find(|opener| opener.seam == seam)
}

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
            .map(|opener| {
                format!(
                    "{}: {} ({})",
                    opener.seam,
                    opener.reason,
                    opener.class.as_str()
                )
            })
            .collect(),
    }
}

#[derive(Debug)]
struct WalHookState {
    threshold_pages: AtomicI32,
    last_log_frames: AtomicI32,
    last_checkpointed_frames: AtomicI32,
    restart_possible: AtomicBool,
    generation_credit_ambiguous: AtomicBool,
    page_size: u64,
    checkpoint_counter: WriteCounter,
    residual_counter: WriteCounter,
}

impl WalHookState {
    /// Record the frame count SQLite supplies to the WAL hook after a commit.
    ///
    /// A lower or equal count after a fully backfilled WAL proves that SQLite
    /// restarted the log, so the checkpoint baseline can be reset. Growth past
    /// the previous count is ambiguous: it can be either an append or a restart
    /// that already outgrew the old generation. Keep the old baseline in that
    /// case so attribution is conservative, and expose the omitted bytes as a
    /// named residual instead of opening the WAL-index to inspect its salt.
    fn observe_commit(&self, log_frames: i32) {
        let log_frames = log_frames.max(0);
        let previous = self.last_log_frames.swap(log_frames, Ordering::Relaxed);
        let restart_possible = self.restart_possible.swap(false, Ordering::Relaxed);
        if log_frames < previous || (restart_possible && log_frames <= previous) {
            self.last_checkpointed_frames.store(0, Ordering::Relaxed);
        } else if restart_possible && log_frames > previous {
            self.generation_credit_ambiguous
                .store(true, Ordering::Relaxed);
        }
    }

    /// Credit the movement reported by SQLite's checkpoint result triple.
    fn record_checkpoint(&self, log_frames: i32, checkpointed_frames: i32) -> u64 {
        if log_frames < 0 || checkpointed_frames < 0 {
            return 0;
        }
        let previous = self.last_checkpointed_frames.load(Ordering::Relaxed).max(0);
        let baseline = if checkpointed_frames < previous {
            // nBackfill cannot decrease within one WAL generation, so this is
            // direct evidence that SQLite restarted the WAL.
            0
        } else {
            previous
        };
        self.last_log_frames.store(log_frames, Ordering::Relaxed);
        self.last_checkpointed_frames
            .store(checkpointed_frames, Ordering::Relaxed);
        self.restart_possible.store(
            log_frames > 0 && checkpointed_frames >= log_frames,
            Ordering::Relaxed,
        );
        u64::try_from(checkpointed_frames.saturating_sub(baseline)).unwrap_or(0)
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
        self.restart_possible.store(false, Ordering::Relaxed);
    }

    fn note_residuals_before_close(&self, is_last_file_handle: bool) {
        let outstanding_frames = self.outstanding_frames();
        if is_last_file_handle && outstanding_frames > 0 {
            self.residual_counter.note_unmeasurable(
                CLOSE_CHECKPOINT_SEAM,
                CLOSE_CHECKPOINT_REASON,
                Some(self.bytes_for_frames(outstanding_frames)),
                Some(CLOSE_CHECKPOINT_ESTIMATE_BASIS),
            );
        }
        if self.generation_credit_ambiguous.load(Ordering::Relaxed) {
            self.residual_counter.note_unmeasurable(
                AMBIGUOUS_WAL_RESTART_SEAM,
                AMBIGUOUS_WAL_RESTART_REASON,
                None,
                None,
            );
        }
    }

    fn bytes_for_frames(&self, frames: u64) -> u64 {
        frames.saturating_mul(self.page_size)
    }
}

unsafe extern "C" fn tracked_wal_hook(
    context: *mut c_void,
    db: *mut rusqlite::ffi::sqlite3,
    database_name: *const c_char,
    frame_count: c_int,
) -> c_int {
    let state = unsafe { &*(context.cast::<WalHookState>()) };
    state.observe_commit(frame_count);
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
            root_id.clone(),
        );
        let residual_counter = crate::write_ledger::register(WriteDomain::Other, root_id);
        let wal_hook = Box::new(WalHookState {
            threshold_pages: AtomicI32::new(DEFAULT_WAL_AUTOCHECKPOINT_PAGES as i32),
            last_log_frames: AtomicI32::new(0),
            last_checkpointed_frames: AtomicI32::new(0),
            restart_possible: AtomicBool::new(false),
            generation_credit_ambiguous: AtomicBool::new(false),
            page_size,
            checkpoint_counter,
            residual_counter,
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
        self.wal_hook.outstanding_frames()
    }

    /// Run an explicit WAL checkpoint and credit only frames newly copied into
    /// the main database. SQLite zeros the returned counts after a successful
    /// TRUNCATE, so that mode snapshots the outstanding count already observed
    /// from WAL-hook commits and earlier checkpoint result triples.
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
        // SQLite may run a checkpoint as the last handle closes, but there is no
        // connection API that reports its result afterward. Reopening any member
        // of the file set to infer it would release this process's advisory locks,
        // so those bytes intentionally remain in the ledger's residual.
        let is_last_file_handle = self
            .file_identity_key
            .as_deref()
            .is_some_and(|key| crate::db::file_identity::registered_connections_for_key(key) == 1);
        self.wal_hook
            .note_residuals_before_close(is_last_file_handle);
        // Drop the SQLite handle before decrementing so the counter never says
        // closed while rusqlite still owns the descriptor and page cache.
        drop(self.connection.take());
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

    /// Closing may copy outstanding WAL frames into the main file, but SQLite
    /// exposes no result triple after the handle is gone. The ledger must leave
    /// those bytes in its named residual rather than reopen the live file set.
    #[test]
    fn closing_the_last_wal_connection_leaves_checkpoint_bytes_in_residual() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("close-checkpoint.sqlite");
        let root = format!("/close-checkpoint/{}", std::process::id());
        let credited_before =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1;

        let (page_size, main_before) = {
            let conn = TrackedConnection::open_attributed(
                &path,
                SqliteStore::CallgraphGeneration,
                root.clone(),
            )
            .unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.set_wal_autocheckpoint(1_000_000).unwrap();
            let page_size: u64 = conn
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .unwrap();

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

            assert!(
                conn.wal_hook.outstanding_frames() > 0,
                "the WAL hook did not observe the fixture's committed frames"
            );
            assert_eq!(
                crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1,
                credited_before,
                "no checkpoint should have been credited before the close"
            );
            (page_size, std::fs::metadata(&path).unwrap().len())
        };

        let credited =
            crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root)
                .1
                .saturating_sub(credited_before);
        let main_growth = std::fs::metadata(&path).unwrap().len() - main_before;

        assert!(
            !wal_path(&path).exists(),
            "ordinary last-close cleanup should remove the WAL without PERSIST_WAL"
        );
        assert_eq!(
            credited, 0,
            "an unobservable close checkpoint must not receive guessed credit"
        );
        assert!(
            main_growth > 0 && main_growth % page_size == 0,
            "fixture did not make the close checkpoint write the main file: {main_growth}"
        );
        let unmeasurable = crate::write_ledger::unmeasurable_for_test(&root);
        let close = unmeasurable
            .iter()
            .find(|entry| entry.seam == CLOSE_CHECKPOINT_SEAM)
            .expect("close-time checkpoint must be classified as unmeasurable");
        let estimate = close
            .estimated_physical_bytes
            .expect("close-time WAL frames provide an estimate");
        assert!(
            estimate >= main_growth && estimate - main_growth <= page_size,
            "frame estimate {estimate} should bound main-file growth {main_growth} within one page"
        );
        assert_eq!(
            close.estimate_basis.as_deref(),
            Some(CLOSE_CHECKPOINT_ESTIMATE_BASIS)
        );

        let standalone = dir.path().join("standalone.sqlite");
        std::fs::copy(&path, &standalone).unwrap();
        let reopened = Connection::open(&standalone).unwrap();
        let rows: i64 = reopened
            .query_row("SELECT COUNT(*) FROM payloads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 256);
    }

    /// Residual accounting issues no SQL and must not move SQLite's checkpoint
    /// policy. Running the same workload through a plain connection and a
    /// tracked one pins that: both have to reach the close having checkpointed
    /// the same number of times and leave the same main file behind.
    #[test]
    fn classifying_close_checkpoint_bytes_as_unmeasurable_does_not_add_a_checkpoint() {
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
            "residual accounting changed how often SQLite checkpoints"
        );
        assert!(!wal_path(&builtin_path).exists());
        assert!(!wal_path(&tracked_path).exists());
        assert_eq!(
            std::fs::metadata(&builtin_path).unwrap().len(),
            std::fs::metadata(&tracked_path).unwrap().len(),
            "the tracked close wrote a different amount into the main file"
        );
    }

    /// A restarted WAL that outgrows the previous generation is indistinguishable
    /// from an append when only frame counts are available. The safe result is a
    /// conservative credit plus a named residual, not a WAL-index header read.
    #[test]
    fn longer_wal_restart_drops_ambiguous_credit_into_residual() {
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
        drop(conn);

        assert!(
            log_lengths[1] > log_lengths[0],
            "the second log must outgrow the first for this to test anything: {log_lengths:?}"
        );
        assert_eq!(
            credited,
            u64::from(log_lengths[1]) * page_size,
            "the conservative baseline should credit only count movement"
        );
        assert!(
            credited < backfilled_frames * page_size,
            "the ambiguous restarted frames must remain uncredited"
        );
        assert!(crate::write_ledger::unmeasurable_for_test(&root)
            .iter()
            .any(|entry| entry.seam == AMBIGUOUS_WAL_RESTART_SEAM
                && entry.reason == AMBIGUOUS_WAL_RESTART_REASON));
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
    fn every_uninstrumented_production_opener_is_classified() {
        let classified = SQLITE_UNINSTRUMENTED_OPENERS
            .iter()
            .map(|opener| (opener.seam, opener.class))
            .collect::<Vec<_>>();
        assert_eq!(
            classified,
            vec![
                (
                    "alias::AliasStore::open",
                    UninstrumentedOpenerClass::Unmeasurable
                ),
                (
                    "alias::ManifestSqliteStore::open",
                    UninstrumentedOpenerClass::Unmeasurable
                ),
                ("gc::sweep_plane", UninstrumentedOpenerClass::Unmeasurable),
                (
                    "views::assembly::assemble",
                    UninstrumentedOpenerClass::NoWrites
                ),
                (
                    "views::generation::clone_derived source",
                    UninstrumentedOpenerClass::NoWrites
                ),
                (
                    "views::generation::clone_derived destination",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "views::generation::checkpoint_derived",
                    UninstrumentedOpenerClass::Unmeasurable
                ),
                (
                    "views::materialization::materialize",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "views::materialization::blob_reader",
                    UninstrumentedOpenerClass::NoWrites
                ),
                (
                    "views::ViewStore::open_pointer_connection",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "views::sync_database_with_passive_checkpoint",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "views::checkpoint_and_sync_database",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "views::checkpoint_pointer_after_cas",
                    UninstrumentedOpenerClass::AttributedElsewhere
                ),
                (
                    "path_status::PathStatusStore::open_at",
                    UninstrumentedOpenerClass::Unmeasurable
                ),
                (
                    "commands::semantic_search::view_semantic_search",
                    UninstrumentedOpenerClass::NoWrites
                ),
            ]
        );
        assert!(SQLITE_UNINSTRUMENTED_OPENERS
            .iter()
            .all(|opener| !opener.reason.is_empty()));
    }
}
