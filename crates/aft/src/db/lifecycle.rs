//! Process-wide accounting for SQLite connections opened by AFT-owned seams.
//!
//! The counter deliberately follows connection lifetime rather than query traffic:
//! leaked SQLite handles keep file descriptors, WAL state, and page caches alive even
//! while idle. Callers must use [`TrackedConnection`] at one of the documented seams.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::write_ledger::{Counter as WriteCounter, Domain as WriteDomain};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

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
/// not pass through [`TrackedConnection`]. The list is empty today. Test-only
/// fixture openers are excluded because they cannot affect daemon lifecycle
/// health and are compiled out of non-test builds.
pub const SQLITE_UNINSTRUMENTED_OPENERS: &[&str] = &[];

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

/// A `rusqlite::Connection` whose lifetime contributes to the process-wide
/// health census. It dereferences to `Connection`, keeping existing query APIs
/// and transaction helpers unchanged while making close accounting automatic.
#[derive(Debug)]
pub struct TrackedConnection {
    connection: Option<Connection>,
    store: SqliteStore,
    write_counter: WriteCounter,
    page_size: u64,
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
        Self::from_connection_attributed(Connection::open(path)?, store, root_id)
    }

    pub fn open_with_flags(
        path: &str,
        flags: OpenFlags,
        store: SqliteStore,
    ) -> rusqlite::Result<Self> {
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
        let page_size = connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))
            .unwrap_or(4096);
        let write_counter = crate::write_ledger::register(
            store.write_domain().unwrap_or(WriteDomain::Other),
            root_id,
        );
        if store.write_domain().is_none() {
            write_counter.note_seam_label(store.label());
        }
        register_open(store);
        let tracked = Self {
            connection: Some(connection),
            store,
            write_counter,
            page_size,
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
        // Drop the SQLite handle before decrementing so the counter never says
        // closed while rusqlite still owns the descriptor and page cache.
        drop(self.connection.take());
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
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
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
        // This assertion intentionally names the documentation seam. If a
        // production bypass is ever necessary, add its stable module/function
        // name to the constant before accepting an incomplete health count.
        assert!(SQLITE_UNINSTRUMENTED_OPENERS.is_empty());
    }
}
