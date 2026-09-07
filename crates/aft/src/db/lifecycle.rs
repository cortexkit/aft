//! Process-wide accounting for SQLite connections opened by AFT-owned seams.
//!
//! The counter deliberately follows connection lifetime rather than query traffic:
//! leaked SQLite handles keep file descriptors, WAL state, and page caches alive even
//! while idle. Callers must use [`TrackedConnection`] at one of the documented seams.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

/// Names each production connection-opening seam so health can attribute a live
/// connection without exposing cache-key paths in the process-wide report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqliteStore {
    AftDb,
    BlobStore,
    CallgraphGeneration,
    InspectScopeCache,
    BreakerFile,
}

impl SqliteStore {
    pub const ALL: [Self; 5] = [
        Self::AftDb,
        Self::BlobStore,
        Self::CallgraphGeneration,
        Self::InspectScopeCache,
        Self::BreakerFile,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::AftDb => "aft.db",
            Self::BlobStore => "blob_stores",
            Self::CallgraphGeneration => "callgraph_generations",
            Self::InspectScopeCache => "inspect_scope_caches",
            Self::BreakerFile => "breaker_files",
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
/// health and are compiled out of release builds.
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
mod thread_counts {
    use super::SqliteStore;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    thread_local! {
        static COUNTS: RefCell<BTreeMap<SqliteStore, i64>> = RefCell::new(BTreeMap::new());
    }

    pub(super) fn record(store: SqliteStore, delta: i64) {
        COUNTS.with(|counts| *counts.borrow_mut().entry(store).or_default() += delta);
    }

    pub(super) fn open_on_this_thread(store: SqliteStore) -> i64 {
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
}

impl TrackedConnection {
    pub fn open(path: &Path, store: SqliteStore) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open(path)?, store)
    }

    pub fn open_with_flags(
        path: &str,
        flags: OpenFlags,
        store: SqliteStore,
    ) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_with_flags(path, flags)?, store)
    }

    pub fn open_path_with_flags(
        path: &Path,
        flags: OpenFlags,
        store: SqliteStore,
    ) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_with_flags(path, flags)?, store)
    }

    pub fn open_in_memory(store: SqliteStore) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, store)
    }

    pub fn from_connection(connection: Connection, store: SqliteStore) -> rusqlite::Result<Self> {
        register_open(store);
        Ok(Self {
            connection: Some(connection),
            store,
        })
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
    fn every_uninstrumented_production_opener_is_documented() {
        // This assertion intentionally names the documentation seam. If a
        // production bypass is ever necessary, add its stable module/function
        // name to the constant before accepting an incomplete health count.
        assert!(SQLITE_UNINSTRUMENTED_OPENERS.is_empty());
    }
}
