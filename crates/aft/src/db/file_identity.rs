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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use super::SqliteStore;

/// Most recent hazards retained for inspection. Bounded because a process that
/// keeps replacing databases under open connections would otherwise grow this
/// without limit.
const RETAINED_HAZARDS: usize = 64;

/// Identity of the file a connection is attached to, as SQLite keys it: the
/// device and inode, not the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
/// Unix uses device/inode; Windows uses volume serial number/file index from
/// a metadata-only handle. Returns `None` when no identity is available.
pub fn identity_of(path: &Path) -> Option<FileIdentity> {
    #[cfg(test)]
    stat_probe::record(path);
    native_identity(crate::file_identity::metadata(path).ok()?.1)
}

fn native_identity(identity: crate::file_identity::FileIdentity) -> Option<FileIdentity> {
    match identity {
        crate::file_identity::FileIdentity::Known { volume, index } => Some(FileIdentity {
            device: volume,
            inode: index,
        }),
        crate::file_identity::FileIdentity::Unavailable => None,
    }
}

fn metadata_identity(metadata: &std::fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

// Cached sidecar identities outlive unlinks while a database remains open.
// Include birth time where available to distinguish recycled inodes from live
// SQLite aliases. Unlike ctime, birth time survives renames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LiveFileIdentity {
    file: FileIdentity,
    created: Option<std::time::SystemTime>,
}

impl LiveFileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            file: metadata_identity(metadata)?,
            // Filesystems without birth times retain inode-only protection.
            created: metadata.created().ok(),
        })
    }

    fn of(path: &Path) -> Option<Self> {
        #[cfg(test)]
        stat_probe::record_followed(path);
        let (metadata, identity) = crate::file_identity::metadata(path).ok()?;
        Some(Self {
            file: native_identity(identity)?,
            created: metadata.created().ok(),
        })
    }
}

pub(crate) fn candidate_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    #[cfg(test)]
    stat_probe::record(path);
    std::fs::symlink_metadata(path)
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
            None => format!(
                "{} on {} (file identity unavailable)",
                self.store, self.thread
            ),
        }
    }
}

#[derive(Debug, Default)]
struct OpenDatabase {
    openers: Vec<Opener>,
    identities: HashSet<LiveFileIdentity>,
}

fn open_databases() -> &'static Mutex<BTreeMap<PathBuf, OpenDatabase>> {
    static DATABASES: OnceLock<Mutex<BTreeMap<PathBuf, OpenDatabase>>> = OnceLock::new();
    DATABASES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Default)]
struct LiveFiles {
    names: HashMap<PathBuf, usize>,
    identities: HashMap<LiveFileIdentity, usize>,
}

fn live_files() -> &'static RwLock<LiveFiles> {
    static FILES: OnceLock<RwLock<LiveFiles>> = OnceLock::new();
    FILES.get_or_init(RwLock::default)
}

/// Identity-admission caches must be revoked when an external database opens or
/// changes identity. Ignore the publisher's storage, whose transient connections
/// reopen on every publication; its paths are excluded by name instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionSnapshot(BTreeMap<PathBuf, HashSet<LiveFileIdentity>>);

pub(crate) fn admission_snapshot(storage: &Path) -> AdmissionSnapshot {
    #[cfg(unix)]
    refresh_sidecar_identities();
    let storage = storage
        .canonicalize()
        .unwrap_or_else(|_| storage.to_path_buf());
    AdmissionSnapshot(
        open_databases()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(path, _)| !path.starts_with(&storage))
            .map(|(path, database)| (path.clone(), database.identities.clone()))
            .collect(),
    )
}

fn database_files(key: &Path) -> [PathBuf; 4] {
    let sidecar = |suffix| {
        let mut path = key.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    [
        key.to_path_buf(),
        sidecar("-wal"),
        sidecar("-shm"),
        sidecar("-journal"),
    ]
}

fn retain_identities(
    database: &mut OpenDatabase,
    identities: impl IntoIterator<Item = LiveFileIdentity>,
) {
    let mut files = live_files().write().unwrap_or_else(|p| p.into_inner());
    for identity in identities {
        if database.identities.insert(identity) {
            *files.identities.entry(identity).or_default() += 1;
        }
    }
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
fn filesystem_gate() -> &'static parking_lot::ReentrantMutex<()> {
    static GATE: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());
    &GATE
}

pub(crate) fn filesystem_guard() -> parking_lot::ReentrantMutexGuard<'static, ()> {
    filesystem_gate().lock()
}

pub(crate) fn try_filesystem_guard() -> Option<parking_lot::ReentrantMutexGuard<'static, ()>> {
    filesystem_gate().try_lock()
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
        let key = connection
            .path()
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
    pub(crate) fn open_census(path: &Path, seam: &'static str) -> rusqlite::Result<Self> {
        let flags =
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
        let _guard = filesystem_guard();
        crate::private_storage::prepare_sqlite(path, flags)?;
        let connection = rusqlite::Connection::open_with_flags(
            crate::db::lifecycle::census_open_path(path)?,
            flags,
        )?;
        connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        Ok(Self::new(connection, seam))
    }

    pub(crate) fn open(path: impl AsRef<Path>, seam: &'static str) -> rusqlite::Result<Self> {
        super::lifecycle::production_write_gate(path.as_ref())?;
        crate::views::donor::refuse_sealed_write(path.as_ref())?;
        let _guard = filesystem_guard();
        crate::private_storage::prepare_sqlite(path.as_ref(), rusqlite::OpenFlags::default())?;
        // Long view-store paths (and their `-journal`/`-wal` sidecars) pass
        // Windows' MAX_PATH; `sqlite_open_path` spells those in `\\?\` form.
        let sqlite_path = super::lifecycle::sqlite_open_path(path.as_ref());
        #[cfg(test)]
        let connection = crate::views::materialization::open_probe_connection(
            &sqlite_path,
            rusqlite::OpenFlags::default(),
        )?;
        #[cfg(not(test))]
        let connection = crate::views::write_accounting::open_connection(
            &sqlite_path,
            rusqlite::OpenFlags::default(),
        )?;
        Ok(Self::new(connection, seam))
    }

    pub(crate) fn open_with_flags(
        path: impl AsRef<Path>,
        flags: rusqlite::OpenFlags,
        seam: &'static str,
    ) -> rusqlite::Result<Self> {
        if !flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            super::lifecycle::production_write_gate(path.as_ref())?;
            crate::views::donor::refuse_sealed_write(path.as_ref())?;
        }
        let _guard = filesystem_guard();
        crate::private_storage::prepare_sqlite(path.as_ref(), flags)?;
        // A `file:` URI is SQLite syntax, not a filesystem path, so it is
        // passed through; plain paths go through `sqlite_open_path`, which
        // adds the Windows long-path spelling when a path needs it.
        let sqlite_path = if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_URI)
            && path.as_ref().to_string_lossy().starts_with("file:")
        {
            std::borrow::Cow::Borrowed(path.as_ref())
        } else {
            super::lifecycle::sqlite_open_path(path.as_ref())
        };
        #[cfg(test)]
        let connection = crate::views::materialization::open_probe_connection(&sqlite_path, flags)?;
        #[cfg(not(test))]
        let connection = crate::views::write_accounting::open_connection(&sqlite_path, flags)?;
        Ok(Self::new(connection, seam))
    }

    pub(crate) fn new(connection: rusqlite::Connection, seam: &'static str) -> Self {
        let record = OpenRecord::new(&connection, SqliteStore::Unmapped(seam));
        if let Some(opener) = crate::db::lifecycle::uninstrumented_opener(seam) {
            if opener.class == crate::db::lifecycle::UninstrumentedOpenerClass::Unmeasurable {
                crate::write_ledger::note_process_unmeasurable(
                    opener.seam,
                    opener.reason,
                    None,
                    None,
                );
            }
        }
        Self {
            connection,
            _record: record,
        }
    }
}

impl std::ops::Deref for IdentityConnection {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl std::ops::DerefMut for IdentityConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

/// Record that a tracked connection has opened `path`, and return the key the
/// matching [`note_close`] must use.
///
/// The key is returned rather than recomputed at close time because the
/// database file may be gone by then, and a path that no longer resolves would
/// otherwise file the close under a different key than the open.
pub(crate) fn note_open(path: &Path, store: SqliteStore) -> PathBuf {
    let key = registry_key(path);
    let paths = database_files(&key);
    let identities = paths.each_ref().map(|path| LiveFileIdentity::of(path));
    let opener = Opener {
        store: store.label(),
        thread: thread_label(),
        identity: identities[0].map(|identity| identity.file),
    };
    let hazard = {
        let mut databases = open_databases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = databases.entry(key.clone()).or_default();
        if entry.openers.is_empty() {
            let mut files = live_files().write().unwrap_or_else(|p| p.into_inner());
            for path in paths {
                *files.names.entry(path).or_default() += 1;
            }
        }
        let hazard = replacement_hazard(&key, entry, &opener);
        retain_identities(entry, identities.into_iter().flatten());
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
        let mut files = live_files().write().unwrap_or_else(|p| p.into_inner());
        for path in database_files(key) {
            if let Some(count) = files.names.get_mut(&path) {
                *count -= 1;
                if *count == 0 {
                    files.names.remove(&path);
                }
            }
        }
        for identity in &entry.identities {
            if let Some(count) = files.identities.get_mut(identity) {
                *count -= 1;
                if *count == 0 {
                    files.identities.remove(identity);
                }
            }
        }
        databases.remove(key);
    }
}

fn replacement_hazard(key: &Path, entry: &OpenDatabase, opener: &Opener) -> Option<DatabaseHazard> {
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
pub(crate) fn registered_connections_for_key(key: &Path) -> usize {
    open_databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .map_or(0, |entry| entry.openers.len())
}

pub fn open_connections(path: &Path) -> usize {
    let key = registry_key(path);
    let identity = identity_of(&key);
    open_databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(registered_path, entry)| {
            entry
                .openers
                .iter()
                .filter(|opener| {
                    registered_path == &key || identity.is_some() && opener.identity == identity
                })
                .count()
        })
        .sum()
}

/// Cheap name check for project walkers whose roots are already canonical.
pub(crate) fn is_registered_file_path(path: &Path) -> bool {
    live_files()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .names
        .contains_key(path)
}

/// Source readers must also reject aliases to live databases and their
/// sidecars. `stat` inspects the inode without opening a descriptor, so it does
/// not release SQLite's process-wide file locks.
pub(crate) fn is_live_sqlite_file(path: &Path) -> bool {
    if is_registered_file_path(path) {
        return true;
    }
    #[cfg(test)]
    stat_probe::record(path);
    let Ok((metadata, identity, links)) = crate::file_identity::metadata_with_link_count(path)
    else {
        return false;
    };
    let Some(file) = native_identity(identity) else {
        return false;
    };
    is_live_identity(
        LiveFileIdentity {
            file,
            created: metadata.created().ok(),
        },
        links,
    )
}

fn is_live_identity(identity: LiveFileIdentity, links: u64) -> bool {
    let known = || {
        live_files()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .identities
            .contains_key(&identity)
    };
    if known() {
        return true;
    }
    if links < 2 {
        return false;
    }
    refresh_sidecar_identities();
    known()
}

pub(crate) fn is_live_sqlite_file_with_metadata(
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> bool {
    #[cfg(windows)]
    {
        // On Windows, std::fs::Metadata omits the file index; crate::file_identity
        // reads it from a handle opened without data access.
        let _ = metadata;
        return is_live_sqlite_file(path);
    }
    #[cfg(not(windows))]
    {
        // A symlink's own inode and link count say nothing about SQLite's target.
        // Resolve only this uncommon case; ordinary source reads need one stat.
        if metadata.is_some_and(|metadata| metadata.file_type().is_symlink()) {
            let key = registry_key(path);
            if is_registered_file_path(&key) {
                return true;
            }
            let target = std::fs::metadata(path).ok();
            return is_live_sqlite_file_with_metadata(&key, target.as_ref());
        }
        let Some(metadata) = metadata else {
            return false;
        };
        let Some(identity) = LiveFileIdentity::from_metadata(metadata) else {
            return false;
        };
        #[cfg(unix)]
        let links = {
            use std::os::unix::fs::MetadataExt;
            metadata.nlink()
        };
        #[cfg(not(unix))]
        let links = 1;
        is_live_identity(identity, links)
    }
}

fn refresh_sidecar_identities() {
    let paths = open_databases()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    // Never stat while holding the connection registry or the reader cache lock.
    let refreshed = paths
        .into_iter()
        .map(|key| {
            let identities = database_files(&key)[1..]
                .iter()
                .filter_map(|path| LiveFileIdentity::of(path))
                .collect::<Vec<_>>();
            (key, identities)
        })
        .collect::<Vec<_>>();
    let mut databases = open_databases().lock().unwrap_or_else(|p| p.into_inner());
    for (key, identities) in refreshed {
        if let Some(database) = databases.get_mut(&key) {
            retain_identities(database, identities);
        }
    }
}

/// Whether an already-open file is a live SQLite database or sidecar, judged
/// by the handle's own identity (`metadata` must come from the open handle):
/// its device and inode, plus its birth time where the filesystem records
/// one, exactly as the registry keys live files. Unlike the path checks above
/// there is no link-count shortcut: by the time a file is open, the names
/// that made it look innocent may already be gone. Birth time keeps a
/// recycled inode of an unlinked sidecar from refusing an unrelated file.
///
/// On Windows, std::fs::Metadata omits the file index. Use is_live_sqlite_file
/// there to obtain the identity through a metadata-only handle.
pub(crate) fn is_live_sqlite_handle(metadata: &std::fs::Metadata) -> bool {
    let Some(identity) = LiveFileIdentity::from_metadata(metadata) else {
        return false;
    };
    let known = || {
        live_files()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .identities
            .contains_key(&identity)
    };
    if known() {
        return true;
    }
    // Sidecars created after their database opened are learned only here.
    #[cfg(unix)]
    refresh_sidecar_identities();
    known()
}

/// Called under the filesystem gate before unlinking a whole cache directory.
/// Consult the registry, not a pre-gate directory inventory: another opener may
/// have created a new database after that inventory was collected.
pub(crate) fn has_open_connections_under(path: &Path) -> bool {
    let key = registry_key(path);
    open_databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|(path, database)| path.starts_with(&key) && !database.openers.is_empty())
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
pub(crate) mod stat_probe {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static COUNTS: RefCell<Option<BTreeMap<PathBuf, usize>>> = const { RefCell::new(None) };
        static CANDIDATES_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(crate) fn record_followed(path: &Path) {
        if !CANDIDATES_ONLY.with(|only| only.get()) {
            record(path);
        }
    }

    /// Count lstat candidates without counting stat calls that refresh SQLite
    /// sidecar identities. Their warm-up state must not look like a walker gain.
    #[cfg(unix)]
    pub(crate) fn count_candidates(f: impl FnOnce()) -> BTreeMap<PathBuf, usize> {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CANDIDATES_ONLY.with(|only| only.set(false));
            }
        }
        let _reset = Reset;
        CANDIDATES_ONLY.with(|only| only.set(true));
        count(f)
    }

    pub(crate) fn record(path: &Path) {
        COUNTS.with(|counts| {
            if let Some(counts) = counts.borrow_mut().as_mut() {
                *counts.entry(path.to_path_buf()).or_default() += 1;
            }
        });
    }

    pub(crate) fn count(f: impl FnOnce()) -> BTreeMap<PathBuf, usize> {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                COUNTS.with(|counts| *counts.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        COUNTS.with(|counts| *counts.borrow_mut() = Some(BTreeMap::new()));
        f();
        COUNTS.with(|counts| counts.borrow_mut().take().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TrackedConnection;
    use rusqlite::Connection;

    #[cfg(unix)]
    #[test]
    fn reused_sidecar_inode_does_not_block_source_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("registered.cache");
        let source = dir.path().join("source.rs");
        std::fs::write(&database, "database").unwrap();
        std::fs::write(&source, "source").unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let Ok(created) = metadata.created() else {
            // Without birth times the registry conservatively retains its
            // device/inode protection, including possible reuse false positives.
            return;
        };
        let key = note_open(&database, SqliteStore::AftDb);
        // Model an unlinked sidecar's recycled inode without relying on the
        // filesystem allocator to reuse a particular inode during the test.
        let old = LiveFileIdentity {
            file: metadata_identity(&metadata).unwrap(),
            created: Some(created - std::time::Duration::from_secs(1)),
        };
        {
            let mut databases = open_databases().lock().unwrap();
            retain_identities(databases.get_mut(&key).unwrap(), [old]);
        }
        let result = crate::cache_freshness::collect(&source);
        note_close(&key, SqliteStore::AftDb);
        assert!(
            result.is_ok(),
            "recycled sidecar inode denied source: {result:?}"
        );
    }

    #[test]
    fn sidecars_created_after_registration_are_live_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        std::fs::write(&database, "database").unwrap();
        let key = note_open(&database, SqliteStore::AftDb);
        let sidecars = &database_files(&key)[1..];
        for path in sidecars {
            assert!(!path.exists());
            std::fs::write(path, "created after registration").unwrap();
        }
        let probe = crate::project_content::Probe::new(&root);
        let counts = stat_probe::count(|| {
            for path in sidecars {
                assert!(
                    is_live_sqlite_file(path),
                    "late sidecar was not live: {path:?}"
                );
            }
        });
        assert!(
            counts.is_empty(),
            "named sidecars needed identity stats: {counts:?}"
        );
        for path in sidecars {
            assert_eq!(
                crate::project_content::read(path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        assert!(probe.paths().is_empty());
        note_close(&key, SqliteStore::AftDb);
        for path in database_files(&key) {
            assert!(!is_registered_file_path(&path));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cached_sidecar_identity_survives_rename_until_last_database_close() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("registered.cache");
        let sidecar = dir.path().join("registered.cache-shm");
        std::fs::write(&database, "database").unwrap();
        std::fs::write(&sidecar, "sidecar").unwrap();
        let key = note_open(&database, SqliteStore::AftDb);
        note_open(&database, SqliteStore::BlobStore);
        let alias = dir.path().join("innocent.rs");
        std::fs::rename(&sidecar, &alias).unwrap();
        assert!(is_live_sqlite_file(&alias));
        note_close(&key, SqliteStore::AftDb);
        assert!(is_live_sqlite_file(&alias));
        note_close(&key, SqliteStore::BlobStore);
        assert!(!is_live_sqlite_file(&alias));
    }

    #[cfg(unix)]
    #[test]
    fn shared_database_identities_remain_live_until_all_registered_paths_close() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.cache");
        let second = dir.path().join("second.cache");
        let alias = dir.path().join("alias.rs");
        std::fs::write(&first, "database").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        std::fs::hard_link(&first, &alias).unwrap();
        let first_key = note_open(&first, SqliteStore::AftDb);
        let second_key = note_open(&second, SqliteStore::BlobStore);
        note_close(&first_key, SqliteStore::AftDb);
        assert!(is_live_sqlite_file(&alias));
        note_close(&second_key, SqliteStore::BlobStore);
        assert!(!is_live_sqlite_file(&alias));
    }

    #[test]
    fn identity_only_connection_tracks_lifetime_without_changing_wal_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.sqlite");
        let connection = IdentityConnection::new(Connection::open(&path).unwrap(), "raw test");
        assert_eq!(open_connections(&path), 1);
        assert_eq!(
            connection
                .pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, i64>(0))
                .unwrap(),
            1000
        );
        drop(connection);
        assert_eq!(open_connections(&path), 0);
    }

    #[test]
    fn census_connection_tracks_identity_without_creating_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("census.sqlite");
        {
            let connection = IdentityConnection::open(&path, "census fixture").unwrap();
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .unwrap();
            connection
                .execute_batch("CREATE TABLE value (n INTEGER); INSERT INTO value VALUES (7);")
                .unwrap();
        }
        assert_eq!(open_connections(&path), 0);
        let before = std::fs::read(&path).unwrap();
        let connection = IdentityConnection::open_census(&path, "census test").unwrap();
        assert_eq!(
            open_connections(&path),
            1,
            "URI opens must retain the real file identity"
        );
        assert_eq!(
            connection
                .query_row("SELECT n FROM value", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert!(!PathBuf::from(format!("{}-wal", path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", path.display())).exists());
        drop(connection);
        assert_eq!(open_connections(&path), 0);
        assert_eq!(std::fs::read(path).unwrap(), before);
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
            replaced
                .detail
                .contains(SqliteStore::CallgraphGeneration.label())
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
