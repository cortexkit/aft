//! Which file on disk each open SQLite connection is actually attached to.
//!
//! SQLite keeps one WAL-index (the `-shm` file) per database file inside a
//! process, and the first connection to win the exclusive dead-man-switch lock
//! on a `-shm` shrinks that file (to three bytes on Darwin, zero on other Unix
//! platforms) before mapping it. POSIX
//! advisory locks never conflict between two descriptors held by the *same*
//! process, so that lock protects nothing once one process holds two
//! connections attached to two *different* database files at the *same* path:
//! the newer connection truncates a `-shm` the older connection has already
//! memory-mapped, and the older connection dies with SIGBUS the next time it
//! reads through the mapping.
//!
//! Nothing in that crash names its cause. The fault surfaces deep inside
//! SQLite, in whichever later query happens to touch the mapping, long after
//! the delete-and-recreate that set it up. So this registry records the
//! identity of the file behind every tracked connection and reports the moment
//! two identities diverge at one path, naming the database and both
//! connections. It also reports a database file that is deleted, renamed, or
//! replaced while this process still has a connection open on it, which is the
//! operation that creates the divergence in the first place.
//!
//! Callgraph file-set mutations also use this registry for enforcement: opening
//! and registering a handle is serialized with checking for live users and
//! removing or renaming its files.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::SqliteStore;

/// Most recent hazards retained for inspection. Bounded because a process that
/// keeps replacing databases under open connections would otherwise grow this
/// without limit.
const RETAINED_HAZARDS: usize = 64;

/// Identity of the file a connection is attached to, as SQLite keys it: the
/// device and inode, not the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl std::fmt::Display for FileIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "dev={}/ino={}", self.device, self.inode)
    }
}

/// Read the identity of the file currently at `path`.
///
/// Returns `None` when the path names nothing, and on platforms that do not
/// expose a stable per-file identity from a path stat. The open-connection half
/// of this registry still works there; the WAL-index truncation this identity
/// detects is a POSIX shared-memory behaviour, and Windows uses a different
/// WAL-index implementation with locks that do conflict within a process.
#[cfg(unix)]
pub fn identity_of(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
pub fn identity_of(_path: &Path) -> Option<FileIdentity> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HazardKind {
    /// Two live connections in this process are attached to two different files
    /// at one path. This is the precondition for the WAL-index truncation
    /// described at the top of this module.
    Replaced,
    /// A database file was deleted, renamed, or replaced while this process
    /// still had a connection open on it.
    MutatedWhileOpen,
}

impl HazardKind {
    const fn summary(self) -> &'static str {
        match self {
            Self::Replaced => "two open connections are attached to different files at one path",
            Self::MutatedWhileOpen => "database file changed while connections were open on it",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseHazard {
    pub kind: HazardKind,
    pub path: PathBuf,
    pub detail: String,
}

impl DatabaseHazard {
    pub fn message(&self) -> String {
        format!(
            "sqlite database hazard ({}): {} — {}",
            self.kind.summary(),
            self.path.display(),
            self.detail
        )
    }
}

#[derive(Clone, Debug)]
struct Opener {
    store: &'static str,
    thread: String,
    identity: Option<FileIdentity>,
}

impl Opener {
    fn describe(&self) -> String {
        match self.identity {
            Some(identity) => format!("{} on {} attached to {identity}", self.store, self.thread),
            None => format!("{} on {} (file identity unavailable)", self.store, self.thread),
        }
    }
}

#[derive(Debug, Default)]
struct OpenDatabase {
    openers: Vec<Opener>,
}

fn open_databases() -> &'static Mutex<BTreeMap<PathBuf, OpenDatabase>> {
    static DATABASES: OnceLock<Mutex<BTreeMap<PathBuf, OpenDatabase>>> = OnceLock::new();
    DATABASES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn reported() -> &'static Mutex<Vec<DatabaseHazard>> {
    static REPORTED: OnceLock<Mutex<Vec<DatabaseHazard>>> = OnceLock::new();
    REPORTED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Resolve a path to the key this registry files it under.
///
/// Symlinks are followed so two spellings of one database agree, matching how
/// SQLite itself keys a database by the inode it ends up at. A path that names
/// nothing yet still resolves through its parent directory, so the key a
/// removal computes matches the key its connections registered under.
fn registry_key(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent).map_or_else(|_| path.to_path_buf(), |dir| dir.join(name))
        }
        _ => path.to_path_buf(),
    }
}

fn thread_label() -> String {
    let current = std::thread::current();
    match current.name() {
        Some(name) => format!("thread {name}"),
        None => format!("thread {:?}", current.id()),
    }
}

fn crash_diagnostics_armed() -> bool {
    static ARMED: OnceLock<bool> = OnceLock::new();
    *ARMED.get_or_init(|| {
        std::env::var_os("AFT_CAPTURE_CRASH_DIAGNOSTICS").as_deref() == Some(OsStr::new("1"))
    })
}

fn report(hazard: DatabaseHazard) {
    let message = hazard.message();
    crate::slog_warn!("{message}");
    if crash_diagnostics_armed() {
        // The test run that arms fatal-signal diagnostics collects stderr, and a
        // hazard reported here is the earlier event a later SIGBUS is downstream
        // of, so the two land in one log next to each other.
        write_stderr_line(&format!("\nAFT sqlite database hazard: {message}\n"));
    }
    let mut reported = reported()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reported.len() >= RETAINED_HAZARDS {
        reported.remove(0);
    }
    reported.push(hazard);
}

/// Write straight to the standard error descriptor.
///
/// Rust's test harness captures `eprintln!` and discards it for a test that
/// passes, and throws it away entirely when the process dies by signal. A
/// hazard is most valuable in exactly those two cases: a run that did not crash
/// but set the crash up, and the run that crashed. Writing to the descriptor
/// bypasses the capture, which is what the fatal-signal handler already does.
#[cfg(unix)]
fn write_stderr_line(line: &str) {
    let bytes = line.as_bytes();
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            bytes.as_ptr().cast::<std::ffi::c_void>(),
            bytes.len(),
        );
    }
}

#[cfg(not(unix))]
fn write_stderr_line(line: &str) {
    eprint!("{line}");
}

/// Serialize SQLite opens with AFT file-set mutations. The staging database
/// keeps this gate while checking whether its file exists and calling the open
/// helper; reentrancy permits that nested call. Connection lifetimes do not hold
/// this gate; the registry protects live users.
pub(crate) fn filesystem_guard() -> parking_lot::ReentrantMutexGuard<'static, ()> {
    static GATE: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());
    GATE.lock()
}

/// Identity-only accounting for connections that retain SQLite's native WAL policy.
/// Kept after the raw connection in `IdentityConnection`, so close completes before
/// the registry forgets the handle, including on early returns and worker moves.
#[derive(Debug)]
pub(crate) struct OpenRecord {
    key: Option<PathBuf>,
    store: SqliteStore,
}

impl OpenRecord {
    pub(crate) fn new(connection: &rusqlite::Connection, store: SqliteStore) -> Self {
        let key = connection.path()
            .filter(|path| !path.is_empty() && *path != ":memory:")
            .map(|path| note_open(Path::new(path), store));
        Self { key, store }
    }
}

impl Drop for OpenRecord {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            note_close(key, self.store);
        }
    }
}

/// Own the raw handle and its identity record together without installing a WAL hook.
/// Field order matters: Rust drops the connection before its registration.
#[derive(Debug)]
pub(crate) struct IdentityConnection {
    connection: rusqlite::Connection,
    _record: OpenRecord,
}

impl IdentityConnection {
    pub(crate) fn open(path: impl AsRef<Path>, seam: &'static str) -> rusqlite::Result<Self> {
        let _guard = filesystem_guard();
        Ok(Self::new(rusqlite::Connection::open(path)?, seam))
    }

    pub(crate) fn open_with_flags(path: impl AsRef<Path>, flags: rusqlite::OpenFlags, seam: &'static str) -> rusqlite::Result<Self> {
        let _guard = filesystem_guard();
        Ok(Self::new(rusqlite::Connection::open_with_flags(path, flags)?, seam))
    }

    pub(crate) fn new(connection: rusqlite::Connection, seam: &'static str) -> Self {
        let record = OpenRecord::new(&connection, SqliteStore::Unmapped(seam));
        Self { connection, _record: record }
    }
}

impl std::ops::Deref for IdentityConnection {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target { &self.connection }
}

impl std::ops::DerefMut for IdentityConnection {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.connection }
}

/// Record that a tracked connection has opened `path`, and return the key the
/// matching [`note_close`] must use.
///
/// The key is returned rather than recomputed at close time because the
/// database file may be gone by then, and a path that no longer resolves would
/// otherwise file the close under a different key than the open.
pub(crate) fn note_open(path: &Path, store: SqliteStore) -> PathBuf {
    let key = registry_key(path);
    let opener = Opener {
        store: store.label(),
        thread: thread_label(),
        identity: identity_of(&key),
    };
    let hazard = {
        let mut databases = open_databases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = databases.entry(key.clone()).or_default();
        let hazard = replacement_hazard(&key, entry, &opener);
        entry.openers.push(opener);
        hazard
    };
    if let Some(hazard) = hazard {
        report(hazard);
    }
    key
}

/// Record that a tracked connection opened under `key` has closed.
pub(crate) fn note_close(key: &Path, store: SqliteStore) {
    let mut databases = open_databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = databases.get_mut(key) else {
        return;
    };
    if let Some(index) = entry
        .openers
        .iter()
        .rposition(|opener| opener.store == store.label())
    {
        entry.openers.remove(index);
    }
    if entry.openers.is_empty() {
        databases.remove(key);
    }
}

fn replacement_hazard(
    key: &Path,
    entry: &OpenDatabase,
    opener: &Opener,
) -> Option<DatabaseHazard> {
    let identity = opener.identity?;
    let previous = entry
        .openers
        .iter()
        .find(|held| held.identity.is_some_and(|held| held != identity))?;
    Some(DatabaseHazard {
        kind: HazardKind::Replaced,
        path: key.to_path_buf(),
        detail: format!(
            "the already-open {} met a new {}; SQLite shares one WAL-index per path, \
             so the newer connection truncates the WAL-index the older one has mapped",
            previous.describe(),
            opener.describe()
        ),
    })
}

/// Number of registered connections at `path` or at another name for its inode.
/// Hold [`filesystem_guard`] across this query and any file-set mutation so an
/// opener cannot appear between checking the registry and changing the files.
pub fn open_connections(path: &Path) -> usize {
    let key = registry_key(path);
    let identity = identity_of(&key);
    open_databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(registered_path, entry)| {
            entry.openers.iter().filter(|opener| {
                registered_path == &key || identity.is_some() && opener.identity == identity
            }).count()
        })
        .sum()
}

/// Report `action` if it is about to delete, rename, or replace the database
/// file at `path` while this process still has a connection open on it.
///
/// `action` names the caller in the report, so it should read as a description
/// of the operation, for example `"callgraph generation sweep"`.
pub fn guard_replacement(path: &Path, action: &str) {
    let key = registry_key(path);
    let detail = {
        let databases = open_databases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = databases.get(&key) else {
            return;
        };
        if entry.openers.is_empty() {
            return;
        }
        let openers = entry
            .openers
            .iter()
            .map(Opener::describe)
            .collect::<Vec<_>>()
            .join("; ");
        format!("{action} ran while these connections were still open: [{openers}]")
    };
    report(DatabaseHazard {
        kind: HazardKind::MutatedWhileOpen,
        path: key,
        detail,
    });
}

/// Hazards reported for `path` so far, newest last.
pub fn hazards_for_path(path: &Path) -> Vec<DatabaseHazard> {
    let key = registry_key(path);
    reported()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|hazard| hazard.path == key)
        .cloned()
        .collect()
}

/// Every hazard retained so far, newest last.
pub fn reported_hazards() -> Vec<DatabaseHazard> {
    reported()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TrackedConnection;
    use rusqlite::Connection;

    #[test]
    fn identity_only_connection_tracks_lifetime_without_changing_wal_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.sqlite");
        let connection = IdentityConnection::new(Connection::open(&path).unwrap(), "raw test");
        assert_eq!(open_connections(&path), 1);
        assert_eq!(connection.pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, i64>(0)).unwrap(), 1000);
        drop(connection);
        assert_eq!(open_connections(&path), 0);
    }

    #[cfg(unix)]
    fn shm_path(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}-shm", path.display()))
    }

        /// The mechanism, stated as an executable fact about SQLite rather than as
    /// prose: replace the database file at a path while a connection still has
    /// that path's WAL-index mapped, and the next connection truncates the
    /// WAL-index under the first one.
    ///
    /// SQLite maps a WAL-index in 32 KiB regions and re-extends the file to
    /// cover region 0 immediately after truncating it, so the visible damage is
    /// the file shrinking back to one region while the first connection still
    /// holds pointers into region 1. Reading through those pointers is a read
    /// past the end of a file-backed mapping, which the kernel answers with a
    /// page-aligned SIGBUS. That is why the fixture first grows the WAL past
    /// the 4062 frames region 0 can index: with only region 0 mapped, the
    /// re-extension hides the truncation behind a page of zeroes instead.
    ///
    /// The first connection is deliberately never used or dropped after the
    /// truncation: touching it is the fault. Leaking one handle for the rest of
    /// the test binary is the price of observing the state safely.
    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn replacing_the_database_file_truncates_the_wal_index_under_a_live_mapping() {
        /// SQLite maps the WAL-index one 32 KiB region at a time.
        const WAL_INDEX_REGION_BYTES: u64 = 32 * 1024;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("replaced.sqlite");

        let first = Connection::open(&path).expect("open first connection");
        // A small page size keeps the WAL small in bytes while still producing
        // the thousands of frames needed to reach the second WAL-index region.
        first
            .pragma_update(None, "page_size", 512)
            .expect("small pages");
        first
            .pragma_update(None, "journal_mode", "WAL")
            .expect("first connection WAL");
        first
            .pragma_update(None, "synchronous", "OFF")
            .expect("unsynced fixture writes");
        first
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autocheckpoint");
        first
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, payload BLOB);")
            .expect("seed");

        // One transaction per row, so every commit appends WAL frames and the
        // log grows past what a single WAL-index region can address.
        let mut mapped_bytes = 0;
        for id in 0..20_000_i64 {
            first
                .execute(
                    "INSERT INTO t(id, payload) VALUES (?1, zeroblob(400))",
                    [id],
                )
                .expect("append a WAL frame");
            if id % 256 == 0 {
                mapped_bytes = std::fs::metadata(shm_path(&path))
                    .expect("WAL-index exists")
                    .len();
                if mapped_bytes > WAL_INDEX_REGION_BYTES {
                    break;
                }
            }
        }
        assert!(
            mapped_bytes > WAL_INDEX_REGION_BYTES,
            "fixture never mapped a second WAL-index region (got {mapped_bytes} bytes); \
             without one the truncation is hidden by SQLite re-extending region 0"
        );
        let first_identity = identity_of(&path).expect("first database identity");

        // Replace the database file at the same path, leaving the WAL-index
        // exactly where it is. This is the shape being hunted: a new database
        // file inode under an unchanged `-shm` inode.
        std::fs::remove_file(&path).expect("remove the database file");
        let second = Connection::open(&path).expect("open second connection");
        second
            .pragma_update(None, "journal_mode", "WAL")
            .expect("second connection WAL");
        second
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("write through the replacement");
        let second_identity = identity_of(&path).expect("second database identity");
        assert_ne!(
            first_identity, second_identity,
            "fixture did not actually replace the database file"
        );

        let after_bytes = std::fs::metadata(shm_path(&path))
            .expect("WAL-index still exists")
            .len();
        assert!(
            after_bytes < mapped_bytes,
            "the second connection did not truncate the WAL-index the first one has mapped \
             (before={mapped_bytes} bytes, after={after_bytes} bytes)"
        );

        drop(second);
        // Dropping `first` would make SQLite read the truncated mapping during
        // its close-time checkpoint, so the handle is deliberately leaked.
        std::mem::forget(first);
    }


    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn a_database_file_replaced_under_a_live_connection_is_reported_with_both_openers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reported.sqlite");

        let first = TrackedConnection::open(&path, SqliteStore::CallgraphGeneration)
            .expect("open first tracked connection");
        first
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        assert!(
            hazards_for_path(&path).is_empty(),
            "a single connection is not a hazard"
        );

        std::fs::remove_file(&path).expect("remove the database file");
        let second = TrackedConnection::open(&path, SqliteStore::AftDb)
            .expect("open second tracked connection");
        second
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed the replacement");

        let hazards = hazards_for_path(&path);
        let replaced = hazards
            .iter()
            .find(|hazard| hazard.kind == HazardKind::Replaced)
            .unwrap_or_else(|| {
                panic!("replacing the database file was not reported; hazards={hazards:?}")
            });
        assert!(
            replaced.detail.contains(SqliteStore::CallgraphGeneration.label())
                && replaced.detail.contains(SqliteStore::AftDb.label()),
            "the report must name both connections: {}",
            replaced.detail
        );

        drop(second);
        drop(first);
        assert_eq!(open_connections(&path), 0);
    }

    #[test]
    fn removing_a_database_file_under_an_open_connection_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("removed.sqlite");
        let connection = TrackedConnection::open(&path, SqliteStore::InspectScopeCache)
            .expect("open tracked connection");
        connection
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        assert_eq!(open_connections(&path), 1);

        guard_replacement(&path, "fixture sweep");

        let hazards = hazards_for_path(&path);
        let mutated = hazards
            .iter()
            .find(|hazard| hazard.kind == HazardKind::MutatedWhileOpen)
            .unwrap_or_else(|| panic!("the removal was not reported; hazards={hazards:?}"));
        assert!(
            mutated.detail.contains("fixture sweep")
                && mutated
                    .detail
                    .contains(SqliteStore::InspectScopeCache.label()),
            "the report must name the action and the open connection: {}",
            mutated.detail
        );

        drop(connection);
        // A removal after the last close is ordinary housekeeping, not a hazard.
        let before = hazards_for_path(&path).len();
        guard_replacement(&path, "fixture sweep after close");
        assert_eq!(hazards_for_path(&path).len(), before);
    }

    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn closing_a_connection_after_its_database_file_is_gone_still_clears_the_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vanished.sqlite");
        let connection =
            TrackedConnection::open(&path, SqliteStore::BlobStore).expect("open tracked");
        connection
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        std::fs::remove_file(&path).expect("remove the database file");
        drop(connection);
        assert_eq!(
            open_connections(&path),
            0,
            "the close must be filed under the key the open used"
        );
    }
}
