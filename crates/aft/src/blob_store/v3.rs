//! Prototype-only v3 storage. Writers serialize at the storage root before
//! taking a registry transaction; ledger readers never enter that lock order.
//! Different storage roots on one device do not coordinate reservations.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use super::v2::{FamilyKey, FamilyPlane};
use crate::db::file_identity::{identity_of, FileIdentity, IdentityConnection};
use crate::persisted_format::{
    self, IncompatibleReason, PersistedStore, UnsupportedPersistedFormat,
};
use crate::pins::PinOwner;
use crate::views::contracts::{observe, DurabilityObserver, DurabilityStep};

pub const FORMAT_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 1;
pub const LAYOUT_TAG: u32 = 2;
pub const REGISTRY_APPLICATION_ID: i64 = 0x41465452;
pub const PLANE_APPLICATION_ID: i64 = 0x41465450;
pub const NAMESPACE_BUDGET: u64 = 4 * 1024 * 1024 * 1024;
/// Sweep epochs a symbol-plane row survives without being touched. Symbol
/// rows are a reusable cache that no manifest names, so the plane-row sweep
/// ages them out by epoch instead of by marking.
pub const SYMBOL_RETENTION_SWEEPS: i64 = 8;
const TIMEOUT_MS: i64 = 5_000;
mod connections;
mod copy;
mod initialization;
#[cfg(test)]
mod layout_gate;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod open_tests;
#[cfg(test)]
mod read_audit;
#[cfg(test)]
mod replay;
#[doc(hidden)]
pub use copy::{copy_on_first_use, CopyOutcome, CopySource, SWEEP_LEASE_WAIT};

const META: &str = "CREATE TABLE format_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, layout_tag INTEGER NOT NULL, schema_text TEXT NOT NULL);";
/// Registry format/schema version of the base-reference protocol
/// (`views_v3_registry` 2/2 in docs/design/views-v3-format.md). A version-1
/// registry is upgraded in place, by the exact-predecessor migration in
/// [`RegistryWriter::upgrade_to_base_refs`], only by a writer that raised the
/// registry reader floor to 2 first.
pub const REGISTRY_BASE_REFS_VERSION: u32 = 2;
macro_rules! registry_v1_schema {
    () => {
        "
CREATE TABLE members (
 scope TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('view','base')),
 root BLOB, view_dir TEXT NOT NULL, registered_at_ms INTEGER NOT NULL,
 last_bind_ms INTEGER NOT NULL, last_publish_ms INTEGER,
 state TEXT NOT NULL, missing_sweeps INTEGER NOT NULL DEFAULT 0);
CREATE TABLE view_sessions (session_id TEXT NOT NULL PRIMARY KEY, scope TEXT NOT NULL,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, opened_at_ms INTEGER NOT NULL);
CREATE TABLE reservations (id INTEGER PRIMARY KEY AUTOINCREMENT,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('reserved','committed','released')),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0),
 charge_bytes INTEGER NOT NULL CHECK(charge_bytes>=0), completed_seq INTEGER);
CREATE TABLE accounting (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 measured_bytes INTEGER NOT NULL, watermark INTEGER NOT NULL);
"
    };
}
macro_rules! base_refs_schema {
    () => {
        "CREATE TABLE base_refs (worker_scope TEXT NOT NULL, base_scope TEXT NOT NULL, nonce TEXT NOT NULL,
 membership_digest TEXT NOT NULL, worker_generation TEXT,
 state TEXT NOT NULL CHECK(state IN ('intent','orphaned','published')),
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, updated_epoch INTEGER NOT NULL,
 PRIMARY KEY(worker_scope, base_scope, nonce));
CREATE INDEX base_refs_by_base ON base_refs(base_scope);
CREATE TABLE base_builds (base_scope TEXT NOT NULL PRIMARY KEY,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, owner_token TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt>=1),
 state TEXT NOT NULL CHECK(state IN ('building','serving')), heartbeat_at_ms INTEGER NOT NULL);
CREATE TABLE strict_rebuild (worker_scope TEXT NOT NULL, worker_generation TEXT NOT NULL,
 reason TEXT NOT NULL, recorded_epoch INTEGER NOT NULL, PRIMARY KEY(worker_scope, worker_generation));
CREATE TABLE base_sweeps (base_scope TEXT NOT NULL PRIMARY KEY,
 unreferenced_sweeps INTEGER NOT NULL DEFAULT 0);
CREATE TABLE sweep_state (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 gc_epoch INTEGER NOT NULL, lease_pid INTEGER, lease_start INTEGER, reconcile_cursor TEXT,
 dangling_base_edges INTEGER NOT NULL DEFAULT 0, live_base_cap_hits INTEGER NOT NULL DEFAULT 0);
"
    };
}
const REGISTRY_SCHEMA: &str = registry_v1_schema!();
const BASE_REFS_SCHEMA: &str = base_refs_schema!();
const REGISTRY_SCHEMA_V2: &str = concat!(registry_v1_schema!(), base_refs_schema!());
const PLANE_SCHEMA: &str = "
CREATE TABLE store_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), plane TEXT NOT NULL, gc_epoch INTEGER NOT NULL);
CREATE TABLE blob_payloads (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32),
 payload BLOB NOT NULL, payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
 payload_schema INTEGER NOT NULL, ref_epoch INTEGER NOT NULL, created_at_ms INTEGER NOT NULL) WITHOUT ROWID;
CREATE INDEX blob_membership ON blob_payloads(full_key, ref_epoch);
CREATE TABLE blob_quarantine (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32)) WITHOUT ROWID;
CREATE TRIGGER immutable_payload BEFORE UPDATE OF payload, payload_digest ON blob_payloads
 BEGIN SELECT RAISE(ABORT, 'immutable_payload'); END;
";

const ROWID_SCHEMA: &str = "
CREATE TABLE store_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), plane TEXT NOT NULL, gc_epoch INTEGER NOT NULL);
CREATE TABLE blob_payloads (id INTEGER PRIMARY KEY,
 full_key BLOB NOT NULL CHECK(length(full_key)=32),
 payload BLOB NOT NULL, payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
 payload_schema INTEGER NOT NULL, created_at_ms INTEGER NOT NULL);
CREATE UNIQUE INDEX blob_locator ON blob_payloads(full_key);
CREATE TABLE blob_liveness (blob_id INTEGER PRIMARY KEY REFERENCES blob_payloads(id) ON DELETE CASCADE,
 ref_epoch INTEGER NOT NULL);
CREATE INDEX blob_membership ON blob_liveness(ref_epoch, blob_id);
CREATE TABLE blob_quarantine (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32)) WITHOUT ROWID;
CREATE TRIGGER immutable_payload BEFORE UPDATE OF payload, payload_digest ON blob_payloads
 BEGIN SELECT RAISE(ABORT, 'immutable_payload'); END;
";

#[cfg(test)]
thread_local! { static CREATE_3A_LAYOUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

fn plane_layout(c: &Connection) -> rusqlite::Result<u32> {
    c.query_row(
        "SELECT layout_tag FROM format_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Incompatible(UnsupportedPersistedFormat),
    Refused(&'static str),
    InvalidIdentity,
    ReservationRegistryMismatch,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::Incompatible(e) => write!(f, "{e}"),
            Self::Refused(reason) => f.write_str(reason),
            Self::InvalidIdentity => f.write_str("invalid v3 family, scope or lock identity"),
            Self::ReservationRegistryMismatch => f.write_str("reservation_registry_mismatch"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

pub fn family_dir(storage: &Path, family: &str) -> Result<PathBuf> {
    super::v2::validate_family(family).map_err(|_| Error::InvalidIdentity)?;
    Ok(storage.join("blobs/v3").join(family))
}
pub fn plane_path(storage: &Path, family: &str, plane: FamilyPlane) -> Result<PathBuf> {
    Ok(family_dir(storage, family)?.join(format!("{}.sqlite", plane.as_str())))
}
pub fn view_dir(storage: &Path, family: &str, scope: &str) -> Result<PathBuf> {
    if scope.is_empty() || scope.contains(['/', '\\', '\0']) || scope == "." || scope == ".." {
        return Err(Error::InvalidIdentity);
    }
    Ok(family_dir(storage, family)?.join("views").join(scope))
}

static FREE_SPACE: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// Process-local probe override for integration children. Production never calls it.
#[doc(hidden)]
pub fn set_free_space_for_test(free: u64, capacity: u64) {
    *FREE_SPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((free, capacity));
}
/// The platform probe itself, bypassing the test override.
#[cfg(test)]
pub(crate) fn probe_free_space_for_test(path: &Path) -> Option<(u64, u64)> {
    probe_free_space(path)
}
fn free_space(path: &Path) -> Option<(u64, u64)> {
    if let Some(value) = *FREE_SPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Some(value);
    }
    probe_free_space(path)
}
#[cfg(unix)]
fn probe_free_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    // The storage root may not exist yet. Probe its nearest existing ancestor
    // without creating anything, so a first-floor refusal is read-only.
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
    }
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and initialized stat.
    let stat = unsafe { stat.assume_init() };
    Some((
        (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64),
    ))
}
#[cfg(windows)]
fn probe_free_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    // As on unix: probe the nearest existing ancestor without creating it.
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
    }
    let wide: Vec<u16> = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // SAFETY: `wide` is NUL-terminated and the three out-pointers name live
    // u64 locals for the duration of the call.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            &mut total_free,
        )
    };
    // `available` honours per-user quotas, like statvfs's f_bavail.
    (ok != 0).then_some((available, total))
}
#[cfg(not(any(unix, windows)))]
fn probe_free_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Read the whole namespace, including SQLite sidecars and member directories.
/// Symlinks are charged as entries, not followed outside the namespace.
pub fn allocated_bytes(path: &Path) -> Result<u64> {
    Ok(allocated_bytes_until(path, None)?.expect("a walk without a deadline completes"))
}

/// [`allocated_bytes`], stopping at `deadline`. The deadline is checked before
/// each directory entry is read, and a walk that reaches it returns `None`:
/// its partial sum is never a total.
pub fn allocated_bytes_until(path: &Path, deadline: Option<Instant>) -> Result<Option<u64>> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        #[cfg(test)]
        WALK_DELAY.with(|delay| {
            if !delay.get().is_zero() {
                std::thread::sleep(delay.get());
            }
        });
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            let Some(bytes) = allocated_bytes_until(&entry.path(), deadline)? else {
                return Ok(None);
            };
            total = total.saturating_add(bytes);
        } else {
            total = total.saturating_add(file_allocated(&meta));
        }
    }
    Ok(Some(total))
}

/// The bytes one non-directory entry is charged in the namespace inventory.
fn file_allocated(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// The inventory charge of the registry's own files (`members.sqlite` and
/// its `-wal` and `-shm`), the only files a measurement's commit and
/// checkpoint change. A few stats, never a walk.
fn registry_allocated(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for name in ["members.sqlite", "members.sqlite-wal", "members.sqlite-shm"] {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(meta) => total = total.saturating_add(file_allocated(&meta)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(total)
}

#[cfg(test)]
thread_local! {
    static WALK_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}
/// Test seam: every namespace walk on this thread sleeps this long per entry.
/// Its only caller is a unix-only test.
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn set_walk_delay_for_test(delay: Duration) {
    WALK_DELAY.with(|cell| cell.set(delay));
}

/// The outcome of a covering measurement taken under a deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measurement {
    /// A complete walk was recorded, with this many allocated bytes.
    Recorded(u64),
    /// The deadline passed before a measurement was confirmed. Either the
    /// first walk was cut short, and nothing was recorded or released (the
    /// registry keeps its last complete measurement, which can only overstate
    /// usage after deletions), or the walk was recorded but the deadline
    /// stopped the recheck, and the saved total was raised by the registry's
    /// own growth so it is never below what the family uses. Every writer
    /// open measures again, so the next writer (or the collector's next
    /// visit) repays it.
    Owed,
}

/// Owning capability for the non-reentrant storage-root lease. Pass this guard
/// into nested writers rather than acquiring the same lease again.
pub struct ReservationLock {
    guard: crate::fs_lock::LockGuard,
    storage: PathBuf,
}
impl ReservationLock {
    pub fn acquire(storage: &Path) -> Result<Self> {
        fs::create_dir_all(storage.join("blobs/v3"))?;
        let guard = crate::fs_lock::try_acquire(
            &storage.join("blobs/v3/reservations.lock"),
            Duration::from_secs(120),
        )
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        Ok(Self {
            guard,
            storage: storage.to_path_buf(),
        })
    }
    /// Like [`Self::acquire`], but waits no later than `deadline`; a deadline already past makes exactly one attempt.
    pub fn acquire_until(storage: &Path, deadline: std::time::Instant) -> Result<Self> {
        fs::create_dir_all(storage.join("blobs/v3"))?;
        let guard = crate::fs_lock::try_acquire_until(
            &storage.join("blobs/v3/reservations.lock"),
            deadline,
        )
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        Ok(Self {
            guard,
            storage: storage.to_path_buf(),
        })
    }
    fn verify(&self, storage: &Path) -> Result<()> {
        if self.storage != storage || !self.guard.verify_writer_epoch()? {
            return Err(Error::InvalidIdentity);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Registry,
    Plane(FamilyPlane),
}
impl Kind {
    fn store(self) -> PersistedStore {
        match self {
            Self::Registry => PersistedStore::ViewsV3Registry,
            Self::Plane(_) => PersistedStore::ViewsV3Store,
        }
    }
    fn schema(self) -> &'static str {
        self.schema_for(self.layout())
            .expect("compiled creation layout")
    }
    fn schema_for(self, layout: u32) -> Option<&'static str> {
        match (self, layout) {
            (Self::Registry, 0) => Some(REGISTRY_SCHEMA),
            (Self::Plane(_), 1) => Some(PLANE_SCHEMA),
            (Self::Plane(_), 2) => Some(ROWID_SCHEMA),
            _ => None,
        }
    }
    fn app_id(self) -> i64 {
        match self {
            Self::Registry => REGISTRY_APPLICATION_ID,
            Self::Plane(_) => PLANE_APPLICATION_ID,
        }
    }
    fn layout(self) -> u32 {
        match self {
            Self::Registry => 0,
            Self::Plane(_) => {
                #[cfg(test)]
                if CREATE_3A_LAYOUT.get() {
                    return 1;
                }
                LAYOUT_TAG
            }
        }
    }
}
fn incompatible(kind: Kind, path: &Path, found: u64, reason: IncompatibleReason) -> Error {
    let displayed = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let refusal = UnsupportedPersistedFormat::incompatible(kind.store(), displayed, found, reason);
    persisted_format::record(&refusal, path);
    Error::Incompatible(refusal)
}
type SchemaRow = (String, String, String, Option<String>);
fn schema_rows(c: &Connection) -> rusqlite::Result<Vec<SchemaRow>> {
    c.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE substr(name,1,7) != 'sqlite_' ORDER BY type,name,tbl_name,sql")?
        .query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect()
}
fn validate(c: &Connection, path: &Path, kind: Kind) -> Result<()> {
    let version = c
        .query_row(
            "SELECT format_version FROM format_meta WHERE singleton=1",
            [],
            |r| r.get::<_, u64>(0),
        )
        .unwrap_or(0);
    let displayed = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    persisted_format::gate(kind.store(), &displayed, path, Some(version))
        .map_err(Error::Incompatible)?;
    if version == 0 {
        return Err(incompatible(kind, path, 0, IncompatibleReason::SchemaDrift));
    }
    let app = c
        .query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);
    if app != kind.app_id() {
        return Err(incompatible(
            kind,
            path,
            version,
            IncompatibleReason::ApplicationId,
        ));
    }
    let (schema, layout, text): (u32, u32, String) = c
        .query_row(
            "SELECT schema_version,layout_tag,schema_text FROM format_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| incompatible(kind, path, version, IncompatibleReason::SchemaDrift))?;
    let compiled_schema = match (kind, version, schema) {
        // A registry of the base-reference protocol carries its own exact
        // text; any other version/schema pair falls through to the drift check.
        (Kind::Registry, 2, 2) if layout == 0 => Some(REGISTRY_SCHEMA_V2),
        _ => kind.schema_for(layout),
    };
    let Some(compiled_schema) = compiled_schema else {
        return Err(incompatible(
            kind,
            path,
            version,
            IncompatibleReason::LayoutTag(layout),
        ));
    };
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(META)?;
    expected.execute_batch(compiled_schema)?;
    let known_version = match kind {
        Kind::Registry => {
            (version, schema) == (u64::from(FORMAT_VERSION), SCHEMA_VERSION)
                || (version, schema)
                    == (
                        u64::from(REGISTRY_BASE_REFS_VERSION),
                        REGISTRY_BASE_REFS_VERSION,
                    )
        }
        Kind::Plane(_) => version == u64::from(FORMAT_VERSION) && schema == SCHEMA_VERSION,
    };
    if !known_version
        || text != format!("{META}{compiled_schema}")
        || schema_rows(c).ok() != Some(schema_rows(&expected)?)
        || !initialization::has_seed(c, kind)
    {
        return Err(incompatible(
            kind,
            path,
            version,
            IncompatibleReason::SchemaDrift,
        ));
    }
    Ok(())
}
fn pragma(c: &Connection, path: &Path, kind: Kind, name: &'static str, value: i64) -> Result<()> {
    c.pragma_update(None, name, value)?;
    let actual: i64 = c.pragma_query_value(None, name, |r| r.get(0))?;
    #[cfg(test)]
    let actual = BAD_PRAGMA.with(|bad| {
        if bad.get() == Some(name) {
            actual + 1
        } else {
            actual
        }
    });
    if actual != value {
        return Err(incompatible(
            kind,
            path,
            1,
            IncompatibleReason::Pragma(name),
        ));
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static BAD_PRAGMA: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }

fn open_db(path: &Path, kind: Kind, create: bool, readonly: bool) -> Result<IdentityConnection> {
    let flags = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    #[cfg(test)]
    let mut c = if replay::enabled() {
        IdentityConnection::new(replay::open(path, flags)?, "views-v3")
    } else if read_audit::enabled() {
        IdentityConnection::new(read_audit::open(path, flags)?, "views-v3")
    } else {
        IdentityConnection::open_with_flags(path, flags, "views-v3")?
    };
    #[cfg(not(test))]
    let mut c = IdentityConnection::open_with_flags(path, flags, "views-v3")?;
    if !readonly {
        initialization::ensure(&mut c, path, kind)?;
    }
    validate(&c, path, kind)?;
    pragma(&c, path, kind, "busy_timeout", TIMEOUT_MS)?;
    if readonly {
        pragma(&c, path, kind, "query_only", 1)?;
    } else {
        let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(incompatible(
                kind,
                path,
                1,
                IncompatibleReason::Pragma("journal_mode"),
            ));
        }
        pragma(&c, path, kind, "synchronous", 2)?;
        pragma(&c, path, kind, "foreign_keys", 1)?;
        pragma(&c, path, kind, "temp_store", 2)?;
        pragma(&c, path, kind, "wal_autocheckpoint", 0)?;
    }
    let vacuum: i64 = c.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if vacuum != 2 {
        return Err(incompatible(
            kind,
            path,
            1,
            IncompatibleReason::Pragma("auto_vacuum"),
        ));
    }
    persisted_format::clear(kind.store(), path);
    Ok(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accounting {
    pub measured_bytes: u64,
    pub watermark: i64,
    pub committed_charges: u64,
    pub reserved_bytes: u64,
}
impl Accounting {
    pub fn usage(self) -> u64 {
        self.measured_bytes
            .saturating_add(self.committed_charges)
            .saturating_add(self.reserved_bytes)
    }
}
fn accounting(c: &Connection) -> Result<Accounting> {
    Ok(c.query_row("SELECT measured_bytes,watermark,COALESCE((SELECT SUM(charge_bytes) FROM reservations WHERE state='committed' AND completed_seq>watermark),0),COALESCE((SELECT SUM(reserved_bytes) FROM reservations WHERE state='reserved'),0) FROM accounting WHERE singleton=1",[],|r|Ok(Accounting { measured_bytes:r.get(0)?,watermark:r.get(1)?,committed_charges:r.get(2)?,reserved_bytes:r.get(3)? }))?)
}

/// Existing-only SELECT capability, with no storage lease, registry barrier or
/// measurement. Local readers share the owning SQLite connection; otherwise
/// SQLite opens READ_ONLY and may maintain its own shared-memory WAL index.
pub struct LedgerReader {
    database: Arc<connections::Database>,
}
impl LedgerReader {
    pub fn open_existing(storage: &Path, family: &str) -> Result<Self> {
        persisted_format::register_storage_root(storage);
        let path = family_dir(storage, family)?.join("members.sqlite");
        Ok(Self {
            database: connections::shared(&path, Kind::Registry, false, true)?,
        })
    }
    pub fn accounting(&self) -> Result<Accounting> {
        accounting(&**self.database.connection()?)
    }
    /// The member row of `scope` as `(kind, state, view_dir)`, read without
    /// any lock or barrier. Pin-then-verify readers use it before and after
    /// taking their marker.
    pub fn member(&self, scope: &str) -> Result<Option<(String, String, String)>> {
        Ok(self
            .database
            .connection()?
            .query_row(
                "SELECT kind,state,view_dir FROM members WHERE scope=?1",
                [scope],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }
    /// Runs a read-only query on the ledger connection.
    pub fn read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        body(&**self.database.connection()?)
    }
    fn reservation_owners(&self) -> Result<(u64, bool)> {
        let connection = self.database.connection()?;
        let mut statement = connection.prepare(
            "SELECT owner_pid,owner_start,reserved_bytes FROM reservations WHERE state='reserved'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                PinOwner {
                    pid: row.get(0)?,
                    start_time: row.get(1)?,
                },
                row.get::<_, u64>(2)?,
            ))
        })?;
        let mut live = 0u64;
        let mut abandoned = false;
        for row in rows {
            let (owner, bytes) = row?;
            if crate::pins::owner_is_live(&owner) {
                live = live
                    .checked_add(bytes)
                    .ok_or(Error::Refused("device_free_space_floor"))?;
            } else {
                abandoned = true;
            }
        }
        Ok((live, abandoned))
    }
}
fn for_each_registry(
    storage: &Path,
    mut body: impl FnMut(&str, &Path) -> Result<()>,
) -> Result<()> {
    let root = storage.join("blobs/v3");
    if !root.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("members.sqlite");
        if !path
            .try_exists()
            .map_err(|_| Error::Refused("reservation_ledger_unreadable"))?
        {
            continue;
        }
        let name = entry.file_name();
        body(
            name.to_str()
                .ok_or(Error::Refused("reservation_ledger_unreadable"))?,
            &path,
        )?;
    }
    Ok(())
}
fn check_space(storage: &Path, request: u64, reserved: u64) -> Result<()> {
    let (free, capacity) = free_space(storage).ok_or(Error::Refused("device_free_space_floor"))?;
    let floor = NAMESPACE_BUDGET.max(capacity / 20);
    if free
        .checked_sub(request)
        .and_then(|bytes| bytes.checked_sub(reserved))
        .is_none_or(|bytes| bytes < floor)
    {
        return Err(Error::Refused("device_free_space_floor"));
    }
    Ok(())
}
fn preflight_floor(storage: &Path, request: u64) -> Result<()> {
    check_space(storage, request, 0)?;
    let mut live = 0u64;
    for_each_registry(storage, |family, _| {
        // This read-only preflight must not prevent abandoned rows or an
        // interrupted bootstrap from reaching recovery under the root lease.
        // Final admission rechecks every ledger after that recovery succeeds.
        if let Ok((bytes, _)) = LedgerReader::open_existing(storage, family)
            .and_then(|reader| reader.reservation_owners())
        {
            live = live
                .checked_add(bytes)
                .ok_or(Error::Refused("device_free_space_floor"))?;
        }
        Ok(())
    })?;
    check_space(storage, request, live)
}
fn recover_ledgers(storage: &Path, lock: &ReservationLock) -> Result<()> {
    lock.verify(storage)?;
    for_each_registry(storage, |family, _| {
        let recover = LedgerReader::open_existing(storage, family)
            .and_then(|reader| reader.reservation_owners())
            .map_or(true, |(_, abandoned)| abandoned);
        if recover {
            RegistryWriter::open_existing(storage, family, lock)
                .map_err(|_| Error::Refused("reservation_ledger_unreadable"))?;
        }
        Ok(())
    })
}
fn check_floor(storage: &Path, request: u64, own: Option<(&Path, &Connection)>) -> Result<()> {
    let (free, capacity) = free_space(storage).ok_or(Error::Refused("device_free_space_floor"))?;
    let mut reserved = 0u64;
    let root = storage.join("blobs/v3");
    if root.try_exists()? {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let registry = entry.path().join("members.sqlite");
            if !registry
                .try_exists()
                .map_err(|_| Error::Refused("reservation_ledger_unreadable"))?
            {
                continue;
            }
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or(Error::Refused("reservation_ledger_unreadable"))?;
            let own_connection = own.filter(|(path, _)| {
                registry == *path
                    || crate::db::file_identity::identity_of(&registry).is_some_and(|identity| {
                        Some(identity) == crate::db::file_identity::identity_of(path)
                    })
            });
            let ledger = own_connection
                .map_or_else(
                    || LedgerReader::open_existing(storage, name).and_then(|r| r.accounting()),
                    |(_, connection)| accounting(connection),
                )
                .map_err(|_| Error::Refused("reservation_ledger_unreadable"))?;
            reserved = reserved
                .checked_add(ledger.reserved_bytes)
                .ok_or(Error::Refused("device_free_space_floor"))?;
        }
    }
    let floor = NAMESPACE_BUDGET.max(capacity / 20);
    if free
        .checked_sub(request)
        .and_then(|v| v.checked_sub(reserved))
        .is_none_or(|v| v < floor)
    {
        return Err(Error::Refused("device_free_space_floor"));
    }
    Ok(())
}

/// A writer holds the root capability before any registry barrier. Creating a
/// registry is private to publication; recovery/collection open existing files.
pub struct RegistryWriter<'a> {
    database: Arc<connections::Database>,
    dir: PathBuf,
    lock: &'a ReservationLock,
}
impl<'a> RegistryWriter<'a> {
    pub fn open_existing(storage: &Path, family: &str, lock: &'a ReservationLock) -> Result<Self> {
        Self::open(storage, family, lock, false)
    }
    /// [`Self::open_existing`] whose covering measurement stops at `deadline`.
    /// An [`Measurement::Owed`] open still returns the writer.
    pub fn open_existing_until(
        storage: &Path,
        family: &str,
        lock: &'a ReservationLock,
        deadline: Instant,
    ) -> Result<(Self, Measurement)> {
        let mut writer = Self::open_unmeasured(storage, family, lock, false)?;
        let measurement = writer.covering_measurement_until(deadline)?;
        Ok((writer, measurement))
    }
    fn open(storage: &Path, family: &str, lock: &'a ReservationLock, create: bool) -> Result<Self> {
        let mut writer = Self::open_unmeasured(storage, family, lock, create)?;
        writer.covering_measurement()?;
        Ok(writer)
    }
    fn open_unmeasured(
        storage: &Path,
        family: &str,
        lock: &'a ReservationLock,
        create: bool,
    ) -> Result<Self> {
        persisted_format::register_storage_root(storage);
        lock.verify(storage)?;
        let dir = family_dir(storage, family)?;
        if create {
            fs::create_dir_all(&dir)?;
        }
        Ok(Self {
            database: connections::shared(
                &dir.join("members.sqlite"),
                Kind::Registry,
                create,
                false,
            )?,
            dir,
            lock,
        })
    }
    pub fn accounting(&self) -> Result<Accounting> {
        accounting(&**self.connection()?)
    }
    /// An existing registry for barrier-only work (base references, build
    /// claims, sweeps). It takes no covering measurement: nothing it does
    /// changes the namespace inventory, and admission still measures through
    /// [`Self::reserve`].
    pub fn open_for_barrier(
        storage: &Path,
        family: &str,
        lock: &'a ReservationLock,
    ) -> Result<Self> {
        Self::open_unmeasured(storage, family, lock, false)
    }
    pub fn storage(&self) -> &Path {
        &self.lock.storage
    }
    pub fn family(&self) -> Result<&str> {
        self.dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(Error::InvalidIdentity)
    }
    pub fn family_dir(&self) -> &Path {
        &self.dir
    }
    /// The registry's format version: 1 before the base-reference upgrade,
    /// [`REGISTRY_BASE_REFS_VERSION`] after it.
    pub fn registry_version(&self) -> Result<u32> {
        Ok(self.connection()?.query_row(
            "SELECT format_version FROM format_meta WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    }
    /// Exact-predecessor migration from registry 1/1 to 2/2: adds the
    /// base-reference tables in one barrier transaction and rewrites
    /// `format_meta`. The caller must already have raised the registry reader
    /// floor to 2. A registry already at 2/2 is left alone; any other state is
    /// schema drift.
    pub fn upgrade_to_base_refs(&mut self) -> Result<()> {
        let path = self.dir.join("members.sqlite");
        self.with_barrier(|tx| {
            let (version, schema, text): (u32, u32, String) = tx.query_row(
                "SELECT format_version,schema_version,schema_text FROM format_meta WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if (version, schema) == (REGISTRY_BASE_REFS_VERSION, REGISTRY_BASE_REFS_VERSION)
                && text == format!("{META}{REGISTRY_SCHEMA_V2}")
            {
                return Ok(());
            }
            if (version, schema) != (FORMAT_VERSION, SCHEMA_VERSION)
                || text != format!("{META}{REGISTRY_SCHEMA}")
            {
                return Err(incompatible(
                    Kind::Registry,
                    &path,
                    u64::from(version),
                    IncompatibleReason::SchemaDrift,
                ));
            }
            tx.execute_batch(BASE_REFS_SCHEMA)?;
            tx.execute(
                "INSERT INTO sweep_state(singleton,gc_epoch) VALUES(1,1)",
                [],
            )?;
            tx.execute(
                "UPDATE format_meta SET format_version=?1,schema_version=?1,schema_text=?2 WHERE singleton=1",
                params![REGISTRY_BASE_REFS_VERSION, format!("{META}{REGISTRY_SCHEMA_V2}")],
            )?;
            Ok(())
        })
    }
    fn connection(&self) -> Result<connections::Guard<'_>> {
        self.database.connection()
    }
    /// After deleting namespace files, call this to refresh accounting. It releases dead rows
    /// in the same barrier transaction as the measurement, retaining plane data.
    pub fn covering_measurement(&mut self) -> Result<u64> {
        match self.measure_and_release(None, None)? {
            Measurement::Recorded(bytes) => Ok(bytes),
            Measurement::Owed => unreachable!("a measurement without a deadline completes"),
        }
    }
    /// [`Self::covering_measurement`] that stops at `deadline`, recording
    /// nothing when it does.
    pub fn covering_measurement_until(&mut self, deadline: Instant) -> Result<Measurement> {
        self.measure_and_release(None, Some(deadline))
    }
    fn measure_and_release(
        &mut self,
        release: Option<i64>,
        deadline: Option<Instant>,
    ) -> Result<Measurement> {
        self.lock.verify(&self.lock.storage)?;
        // Checkpoint first: recording a measurement must not itself leave a new
        // WAL charge. Repeat if the accounting update allocated registry pages.
        let mut connection = self.connection()?;
        checkpoint(&connection)?;
        // The last total this call committed, with the registry-file bytes the
        // walk behind it counted.
        let mut committed: Option<(u64, u64)> = None;
        for _ in 0..8 {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Nothing writes the registry files between this stat and the
            // walk: this transaction holds the registry's write lock and its
            // own writes come later.
            let counted = registry_allocated(&self.dir)?;
            // Dropping the transaction rolls it back: a walk cut short by the
            // deadline records nothing and releases nothing.
            let Some(bytes) = allocated_bytes_until(&self.dir, deadline)? else {
                drop(tx);
                return match committed {
                    None => Ok(Measurement::Owed),
                    Some((bytes, counted)) => {
                        settle_registry_growth(&mut connection, &self.dir, bytes, counted)
                    }
                };
            };
            #[cfg(test)]
            let expire_recheck = GROW_AND_EXPIRE_RECHECK.with(|seam| seam.replace(false));
            #[cfg(test)]
            if expire_recheck {
                // Make this commit allocate registry pages the walk did not see.
                for index in 0..256 {
                    tx.execute(
                        "INSERT INTO view_sessions VALUES(?1,'seam',1,1,0)",
                        [format!("{index:04}{}", "x".repeat(1024))],
                    )?;
                }
            }
            if let Some(id) = release {
                tx.execute(
                    "UPDATE reservations SET state='released',reserved_bytes=0 WHERE id=?1",
                    [id],
                )?;
            }
            let rows: Vec<(i64, PinOwner)> = tx
                .prepare(
                    "SELECT id,owner_pid,owner_start FROM reservations WHERE state='reserved'",
                )?
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        PinOwner {
                            pid: r.get(1)?,
                            start_time: r.get(2)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            for (id, owner) in rows {
                if !crate::pins::owner_is_live(&owner) {
                    tx.execute(
                        "UPDATE reservations SET state='released',reserved_bytes=0 WHERE id=?1",
                        [id],
                    )?;
                }
            }
            let watermark: i64 = tx.query_row(
                "SELECT COALESCE(MAX(completed_seq),0) FROM reservations",
                [],
                |r| r.get(0),
            )?;
            tx.execute(
                "UPDATE accounting SET measured_bytes=?1,watermark=?2 WHERE singleton=1",
                params![bytes, watermark],
            )?;
            tx.commit()?;
            checkpoint(&connection)?;
            committed = Some((bytes, counted));
            #[cfg(test)]
            let deadline = if expire_recheck {
                Some(Instant::now())
            } else {
                deadline
            };
            // The commit and checkpoint above can allocate registry pages the
            // walk did not count, so only a complete recheck that finds the
            // same total confirms it. A recheck the deadline stops is not a
            // confirmation.
            match allocated_bytes_until(&self.dir, deadline)? {
                Some(again) if again == bytes => return Ok(Measurement::Recorded(bytes)),
                Some(_) => {}
                None => return settle_registry_growth(&mut connection, &self.dir, bytes, counted),
            }
        }
        Err(Error::Refused("reservation_ledger_unreadable"))
    }
    pub fn with_barrier<T>(
        &mut self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.lock.verify(&self.lock.storage)?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = body(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn register_member(
        &mut self,
        scope: &str,
        kind: &str,
        root: Option<&Path>,
    ) -> Result<PathBuf> {
        let dir = view_dir(
            &self.lock.storage,
            self.dir
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or(Error::InvalidIdentity)?,
            scope,
        )?;
        let root = root.and_then(encode_root);
        self.with_barrier(|tx| { tx.execute("INSERT INTO members(scope,kind,root,view_dir,registered_at_ms,last_bind_ms,state) VALUES(?1,?2,?3,?4,?5,?5,'active') ON CONFLICT(scope) DO UPDATE SET last_bind_ms=excluded.last_bind_ms,root=excluded.root",params![scope,kind,root,dir.to_string_lossy(),crate::pins::now_ms()])?; Ok(()) })?;
        Ok(dir)
    }
    pub fn release(&mut self, reservation: Reservation) -> Result<()> {
        if reservation.registry != self.registry_identity()? {
            return Err(Error::ReservationRegistryMismatch);
        }
        self.measure_and_release(Some(reservation.id), None)?;
        Ok(())
    }
    pub fn reserve(&mut self, request: u64) -> Result<Reservation> {
        let storage = self.lock.storage.clone();
        recover_ledgers(&storage, self.lock)?;
        let owner_start = crate::root_cache::process_start_time_ms(std::process::id())
            .ok_or(Error::Refused("reservation_ledger_unreadable"))?;
        let before = allocated_bytes(&self.dir)?;
        let registry = self.registry_identity()?;
        let path = self.dir.join("members.sqlite");
        let id = self.with_barrier(|tx| {
            // The current barrier already borrows this owner's connection.
            // Use that borrow rather than re-entering its connection mutex.
            check_floor(&storage,request,Some((&path,tx)))?;
            if accounting(tx)?.usage().checked_add(request).is_none_or(|n|n>NAMESPACE_BUDGET) {
                return Err(Error::Refused("v3_namespace_budget"));
            }
            #[cfg(test)]
            if request == 100 { tests::admission_hook(); }
            tx.execute("INSERT INTO reservations(owner_pid,owner_start,state,reserved_bytes,charge_bytes) VALUES(?1,?2,'reserved',?3,0)",params![std::process::id(),owner_start,request])?;
            Ok(tx.last_insert_rowid())
        })?;
        Ok(Reservation {
            id,
            before,
            registry,
        })
    }
    /// Grows a live reservation by `additional` bytes under the same
    /// admission checks as [`Self::reserve`]: the device floor and the
    /// namespace budget. An owner extends before it writes past its
    /// reservation, never after.
    pub fn extend(&mut self, reservation: &Reservation, additional: u64) -> Result<()> {
        if reservation.registry != self.registry_identity()? {
            return Err(Error::ReservationRegistryMismatch);
        }
        let storage = self.lock.storage.clone();
        let path = self.dir.join("members.sqlite");
        let id = reservation.id;
        self.with_barrier(|tx| {
            check_floor(&storage, additional, Some((&path, tx)))?;
            if accounting(tx)?
                .usage()
                .checked_add(additional)
                .is_none_or(|n| n > NAMESPACE_BUDGET)
            {
                return Err(Error::Refused("v3_namespace_budget"));
            }
            let updated = tx.execute(
                "UPDATE reservations SET reserved_bytes=reserved_bytes+?1 WHERE id=?2 AND state='reserved'",
                params![additional, id],
            )?;
            if updated != 1 {
                return Err(Error::Refused("reservation_not_live"));
            }
            Ok(())
        })
    }
    /// Bytes still reserved by a live reservation (0 once committed or
    /// released).
    pub fn reserved_bytes(&self, reservation: &Reservation) -> Result<u64> {
        Ok(self.connection()?.query_row(
            "SELECT CASE WHEN state='reserved' THEN reserved_bytes ELSE 0 END FROM reservations WHERE id=?1",
            [reservation.id],
            |r| r.get(0),
        )?)
    }
    fn registry_identity(&self) -> Result<RegistryIdentity> {
        let path = fs::canonicalize(self.dir.join("members.sqlite"))?;
        Ok(RegistryIdentity {
            file: identity_of(&path),
            path,
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
struct RegistryIdentity {
    path: PathBuf,
    file: Option<FileIdentity>,
}
#[derive(Debug)]
pub struct Reservation {
    id: i64,
    before: u64,
    registry: RegistryIdentity,
}
/// After a committed measurement whose recheck the deadline stopped: raises
/// the saved total by the registry files' growth since the walk counted them,
/// so it is never below what the family uses, and returns
/// [`Measurement::Owed`]. Only the registry's own commits and checkpoints change
/// files while the reservation lock is held, so stats of those files replace
/// the walk; each correction is itself committed and checkpointed, and the
/// files are re-stated until that leaves them unchanged.
fn settle_registry_growth(
    connection: &mut Connection,
    dir: &Path,
    bytes: u64,
    counted: u64,
) -> Result<Measurement> {
    let mut recorded = bytes;
    for _ in 0..8 {
        let safe = bytes
            .saturating_sub(counted)
            .saturating_add(registry_allocated(dir)?)
            .max(recorded);
        if safe == recorded {
            // No growth since the total last saved: it stands.
            return Ok(Measurement::Owed);
        }
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE accounting SET measured_bytes=?1 WHERE singleton=1",
            [safe],
        )?;
        tx.commit()?;
        checkpoint(connection)?;
        recorded = safe;
    }
    Err(Error::Refused("reservation_ledger_unreadable"))
}

#[cfg(test)]
thread_local! {
    static GROW_AND_EXPIRE_RECHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// Test seam: the next deadline-bounded measurement on this thread commits
/// extra registry pages after its walk, and its recheck finds the deadline
/// already passed. Its only caller is a unix-only test.
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn grow_and_expire_recheck_for_test() {
    GROW_AND_EXPIRE_RECHECK.with(|seam| seam.set(true));
}

fn checkpoint(c: &Connection) -> Result<()> {
    let (busy, _, _): (i64, i64, i64) =
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    if busy != 0 {
        return Err(Error::Refused("reservation_ledger_unreadable"));
    }
    Ok(())
}

fn encode_root(root: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(root.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        root.to_str().map(|text| text.as_bytes().to_vec())
    }
}

/// No production caller uses this publication boundary. The read-only preflight
/// counts live reservations; final admission follows locked recovery and counts
/// every remaining reservation.
#[doc(hidden)]
pub fn publish<T>(
    storage: &Path,
    family: &str,
    request: u64,
    observer: Option<&dyn DurabilityObserver>,
    body: impl FnOnce(&mut Publication<'_, '_>) -> Result<T>,
) -> Result<T> {
    family_dir(storage, family)?;
    preflight_floor(storage, request)?;
    crate::reader_floor::publish_v3_with_floor(storage, FORMAT_VERSION, FORMAT_VERSION, || {
        observe(observer, DurabilityStep::V3FloorRaised);
        let lock = ReservationLock::acquire(storage)?;
        publish_under_lock(storage, family, request, &lock, observer, body)
    })
    .map_err(|e| Error::Io(std::io::Error::other(e)))?
}
/// Nested operations borrow the existing capability and never reacquire it.
#[doc(hidden)]
pub fn publish_locked<T>(
    storage: &Path,
    family: &str,
    request: u64,
    lock: &ReservationLock,
    observer: Option<&dyn DurabilityObserver>,
    body: impl FnOnce(&mut Publication<'_, '_>) -> Result<T>,
) -> Result<T> {
    lock.verify(storage)?;
    preflight_floor(storage, request)?;
    crate::reader_floor::publish_v3_with_floor(storage, FORMAT_VERSION, FORMAT_VERSION, || {
        observe(observer, DurabilityStep::V3FloorRaised);
        publish_under_lock(storage, family, request, lock, observer, body)
    })
    .map_err(|e| Error::Io(std::io::Error::other(e)))?
}
/// Per-visit progress of the v3 plane-row sweep: for each plane, the last
/// key (hex) the previous bounded visit examined.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaneRowCursor {
    pub format_version: u32,
    pub after: std::collections::BTreeMap<String, String>,
}

/// What one bounded plane-row visit did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaneRowSweep {
    pub examined: u64,
    pub deleted: u64,
    /// Every plane was scanned to its end in this visit.
    pub complete: bool,
}

impl RegistryWriter<'_> {
    /// Deletes plane rows of this family that no marked generation, pin or
    /// protected reader names, under the storage-root lock this writer holds.
    /// Every v3 plane write happens inside a publication that holds the same
    /// lock, so no row can be put or touched between the caller's marking and
    /// this deletion. Rows with `ref_epoch >= epoch` are never deleted, and
    /// every plane's `gc_epoch` is raised to `epoch` first, so a row written by
    /// a later publication carries an epoch this sweep cannot delete.
    ///
    /// The scan stops after `limit` examined rows or at `deadline` and resumes
    /// from `cursor` on the next visit; reaching a bound never counts as having
    /// seen the remaining rows.
    pub fn sweep_plane_rows(
        &mut self,
        epoch: i64,
        marked: &std::collections::HashSet<[u8; 32]>,
        cursor: &mut PlaneRowCursor,
        limit: u64,
        deadline: Option<Instant>,
    ) -> Result<PlaneRowSweep> {
        self.lock.verify(&self.lock.storage)?;
        cursor.format_version = 1;
        let mut report = PlaneRowSweep {
            complete: true,
            ..PlaneRowSweep::default()
        };
        // Every v3 plane store, including the v3-only symbol plane.
        for plane in FamilyPlane::V3_STORES {
            let path = self.dir.join(format!("{}.sqlite", plane.as_str()));
            if !path.try_exists()? {
                cursor.after.remove(plane.as_str());
                continue;
            }
            let database = connections::shared(&path, Kind::Plane(plane), false, false)?;
            let mut c = database.connection()?;
            {
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE store_meta SET gc_epoch=?1 WHERE singleton=1 AND gc_epoch<?1",
                    [epoch],
                )?;
                tx.commit()?;
            }
            let layout = plane_layout(&c)?;
            let (select, delete) = if layout == 1 {
                (
                    "SELECT full_key FROM blob_payloads WHERE ref_epoch<?1 AND full_key>?2 ORDER BY full_key LIMIT 64",
                    "DELETE FROM blob_payloads WHERE full_key=?1 AND ref_epoch<?2",
                )
            } else {
                (
                    "SELECT p.full_key FROM blob_payloads p JOIN blob_liveness l ON l.blob_id=p.id WHERE l.ref_epoch<?1 AND p.full_key>?2 ORDER BY p.full_key LIMIT 64",
                    "DELETE FROM blob_payloads WHERE full_key=?1 AND id IN (SELECT blob_id FROM blob_liveness WHERE ref_epoch<?2)",
                )
            };
            let mut after = cursor
                .after
                .get(plane.as_str())
                .and_then(|hex| super::v2::parse_hex32(hex))
                .map(|key| key.to_vec())
                .unwrap_or_default();
            // No manifest names a symbol row, so marking never protects one.
            // A symbol row is instead kept until it has gone untouched for
            // SYMBOL_RETENTION_SWEEPS sweep epochs; every other plane is
            // collected as soon as it is unmarked and older than this epoch.
            let collect_below = if plane == FamilyPlane::Symbol {
                epoch.saturating_sub(SYMBOL_RETENTION_SWEEPS)
            } else {
                epoch
            };
            loop {
                if report.examined >= limit || deadline.is_some_and(|d| Instant::now() >= d) {
                    report.complete = false;
                    break;
                }
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let keys: Vec<Vec<u8>> = tx
                    .prepare(select)?
                    .query_map(params![collect_below, after], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                if keys.is_empty() {
                    tx.commit()?;
                    after.clear();
                    break;
                }
                for key in &keys {
                    report.examined += 1;
                    let protected = <[u8; 32]>::try_from(key.as_slice())
                        .map_or(true, |key| marked.contains(&key));
                    if !protected {
                        report.deleted += tx.execute(delete, params![key, collect_below])? as u64;
                    }
                }
                tx.commit()?;
                after = keys.last().cloned().unwrap_or_default();
            }
            if after.is_empty() {
                cursor.after.remove(plane.as_str());
            } else {
                cursor
                    .after
                    .insert(plane.as_str().to_owned(), super::v2::to_hex(&after));
            }
            if !report.complete {
                break;
            }
        }
        Ok(report)
    }
}

/// Opens (creating when absent) the family registry for the base-reference
/// protocol and upgrades it to registry 2/2. The registry floor is raised to
/// 2 before any byte of the new schema exists, and the store floor to 1,
/// through the combined v3 floor boundary every v3 writer uses.
#[doc(hidden)]
pub fn open_base_refs_registry<'a>(
    storage: &Path,
    family: &str,
    lock: &'a ReservationLock,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<RegistryWriter<'a>> {
    family_dir(storage, family)?;
    lock.verify(storage)?;
    crate::reader_floor::publish_v3_with_floor(
        storage,
        REGISTRY_BASE_REFS_VERSION,
        FORMAT_VERSION,
        || {
            observe(observer, DurabilityStep::V3FloorRaised);
            let exists = family_dir(storage, family)?
                .join("members.sqlite")
                .try_exists()?;
            let mut registry = if exists {
                RegistryWriter::open_for_barrier(storage, family, lock)?
            } else {
                RegistryWriter::open(storage, family, lock, true)?
            };
            registry.upgrade_to_base_refs()?;
            Ok(registry)
        },
    )
    .map_err(|e| Error::Io(std::io::Error::other(e)))?
}
fn publish_under_lock<T>(
    storage: &Path,
    family: &str,
    request: u64,
    lock: &ReservationLock,
    observer: Option<&dyn DurabilityObserver>,
    body: impl FnOnce(&mut Publication<'_, '_>) -> Result<T>,
) -> Result<T> {
    recover_ledgers(storage, lock)?;
    let mut registry = RegistryWriter::open(storage, family, lock, true)?;
    let reservation = registry.reserve(request)?;
    let Reservation { id, before, .. } = reservation;
    let mut publication = Publication {
        registry: &mut registry,
        planes: Vec::new(),
        payload_bytes: 0,
    };
    let outcome = body(&mut publication).and_then(|result| {
        publication.flush()?;
        observe(observer, DurabilityStep::ClosureFlushed);
        Ok(result)
    });
    let payload_bytes = publication.payload_bytes;
    drop(publication);
    let finished = outcome.and_then(|result| {
            let charge = allocated_bytes(&registry.dir)?
                .saturating_sub(before)
                .max(payload_bytes);
            registry.with_barrier(|tx| {
                tx.execute("UPDATE reservations SET state='committed',reserved_bytes=0,charge_bytes=?1,completed_seq=(SELECT COALESCE(MAX(completed_seq),0)+1 FROM reservations) WHERE id=?2",params![charge,id])?;
                Ok(())
            })?;
            // The registry's own publication writes belong to this attempt too.
            checkpoint(&**registry.connection()?)?;
            let charge = allocated_bytes(&registry.dir)?
                .saturating_sub(before)
                .max(payload_bytes);
            registry.with_barrier(|tx| {
                tx.execute(
                    "UPDATE reservations SET charge_bytes=?1 WHERE id=?2",
                    params![charge, id],
                )?;
                Ok(())
            })?;
            checkpoint(&**registry.connection()?)?;
            Ok(result)
    });
    match finished {
        Ok(result) => Ok(result),
        Err(error) => {
            registry.release(reservation)?;
            Err(error)
        }
    }
}

pub struct Publication<'a, 'b> {
    pub registry: &'a mut RegistryWriter<'b>,
    planes: Vec<(FamilyPlane, Arc<connections::Database>)>,
    payload_bytes: u64,
}
impl Publication<'_, '_> {
    fn plane(&mut self, plane: FamilyPlane) -> Result<connections::Guard<'_>> {
        self.plane_with(plane, true)?
            .ok_or(Error::Refused("plane_store_missing"))
    }
    /// The plane's publication connection. Without `create`, a plane store
    /// that does not exist is `None` and nothing is created.
    fn plane_with(
        &mut self,
        plane: FamilyPlane,
        create: bool,
    ) -> Result<Option<connections::Guard<'_>>> {
        if let Some(index) = self.planes.iter().position(|(p, _)| *p == plane) {
            return self.planes[index].1.connection().map(Some);
        }
        let path = self.registry.dir.join(format!("{}.sqlite", plane.as_str()));
        if !create && !path.try_exists()? {
            return Ok(None);
        }
        let database = connections::shared(&path, Kind::Plane(plane), create, false)?;
        let c = database.connection()?;
        let stored: String =
            c.query_row("SELECT plane FROM store_meta WHERE singleton=1", [], |r| {
                r.get(0)
            })?;
        if stored != plane.as_str() {
            return Err(incompatible(
                Kind::Plane(plane),
                &path,
                1,
                IncompatibleReason::SchemaDrift,
            ));
        }
        drop(c);
        self.planes.push((plane, database));
        self.planes
            .last()
            .expect("inserted plane")
            .1
            .connection()
            .map(Some)
    }
    /// Stamps the plane's current `gc_epoch` on an existing row whose stored
    /// digest is `digest`. Never inserts and never creates a plane store, so a
    /// reservation sized for a touch cannot be overrun. `false` means no such
    /// row exists (any more).
    fn touch_row(&mut self, key: &FamilyKey, digest: &[u8]) -> Result<bool> {
        let Some(mut c) = self.plane_with(key.plane(), false)? else {
            return Ok(false);
        };
        #[cfg(test)]
        if replay::unflushed_closure() {
            // Test-only fault injection for the power-loss replay, as in put_row.
            c.pragma_update(None, "synchronous", 0)?;
        }
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sql = if plane_layout(&tx)? == 1 {
            "UPDATE blob_payloads SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE full_key=?1 AND payload_digest=?2"
        } else {
            "UPDATE blob_liveness SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE blob_id=(SELECT id FROM blob_payloads WHERE full_key=?1 AND payload_digest=?2)"
        };
        let touched = tx.execute(sql, params![key.as_bytes().as_slice(), digest])?;
        tx.commit()?;
        Ok(touched == 1)
    }
    pub fn put(&mut self, key: &FamilyKey, payload: &[u8], payload_schema: u32) -> Result<()> {
        self.put_row(key, payload, payload_schema).map(|_| ())
    }
    /// Inserts the row, or touches an equal existing one. `true` means the
    /// row was inserted by this call.
    pub fn put_or_touch(
        &mut self,
        key: &FamilyKey,
        payload: &[u8],
        payload_schema: u32,
    ) -> Result<bool> {
        self.put_row(key, payload, payload_schema)
    }
    /// Stamps the plane's current `gc_epoch` on each existing row of `keys`
    /// and returns the keys that have no row. Never inserts, and never creates
    /// a plane store. The caller protects every key (a pin or live pin) before
    /// touching it, so a sweep either marks it or sees the new epoch.
    pub fn touch(&mut self, keys: &[FamilyKey]) -> Result<Vec<FamilyKey>> {
        let mut missing = Vec::new();
        for key in keys {
            let Some(mut c) = self.plane_with(key.plane(), false)? else {
                missing.push(*key);
                continue;
            };
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sql = if plane_layout(&tx)? == 1 {
                "UPDATE blob_payloads SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE full_key=?1"
            } else {
                "UPDATE blob_liveness SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE blob_id=(SELECT id FROM blob_payloads WHERE full_key=?1)"
            };
            if tx.execute(sql, [key.as_bytes().as_slice()])? == 0 {
                missing.push(*key);
            }
            tx.commit()?;
        }
        Ok(missing)
    }
    /// Inserts the row, or touches an equal existing one. `true` means inserted.
    fn put_row(&mut self, key: &FamilyKey, payload: &[u8], payload_schema: u32) -> Result<bool> {
        let mut c = self.plane(key.plane())?;
        #[cfg(test)]
        if replay::unflushed_closure() {
            // Test-only fault injection for the power-loss replay: plane
            // commits stop syncing, so nothing but the closure flush could make
            // them durable.
            c.pragma_update(None, "synchronous", 0)?;
        }
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let digest = blake3::hash(payload);
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT payload_digest FROM blob_payloads WHERE full_key=?1",
                [key.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        let written = if let Some(existing) = existing {
            if existing != digest.as_bytes() {
                return Err(Error::Refused("payload_mismatch"));
            }
            let sql = if plane_layout(&tx)? == 1 {
                "UPDATE blob_payloads SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE full_key=?1"
            } else {
                "UPDATE blob_liveness SET ref_epoch=(SELECT gc_epoch FROM store_meta) WHERE blob_id=(SELECT id FROM blob_payloads WHERE full_key=?1)"
            };
            tx.execute(sql, [key.as_bytes().as_slice()])?;
            false
        } else {
            if plane_layout(&tx)? == 1 {
                tx.execute("INSERT INTO blob_payloads VALUES(?1,?2,?3,?4,(SELECT gc_epoch FROM store_meta),?5)",params![key.as_bytes().as_slice(),payload,digest.as_bytes().as_slice(),payload_schema,crate::pins::now_ms()])?;
            } else {
                tx.execute("INSERT INTO blob_payloads(full_key,payload,payload_digest,payload_schema,created_at_ms) VALUES(?1,?2,?3,?4,?5)",params![key.as_bytes().as_slice(),payload,digest.as_bytes().as_slice(),payload_schema,crate::pins::now_ms()])?;
                tx.execute("INSERT INTO blob_liveness VALUES(last_insert_rowid(),(SELECT gc_epoch FROM store_meta))",[])?;
            }
            true
        };
        tx.commit()?;
        drop(c);
        if written {
            self.payload_bytes = self.payload_bytes.saturating_add(payload.len() as u64);
        }
        Ok(written)
    }
    fn flush(&self) -> Result<()> {
        #[cfg(test)]
        if replay::unflushed_closure() {
            return Ok(());
        }
        for (_, database) in &self.planes {
            checkpoint(&**database.connection()?)?;
        }
        crate::durability::sync_dir(&self.registry.dir)?;
        Ok(())
    }
}

/// Per-file metadata; format support advertised by `aft --formats` is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3FormatReport {
    pub store_name: &'static str,
    pub format_version: u32,
    pub schema_version: u32,
    pub layout_tag: u32,
}

/// Validates an existing v3 file through its owning connection, without creating
/// a store, acquiring a reservation lock, or changing its schema.
pub fn v3_format_report(path: &Path) -> Result<V3FormatReport> {
    let kind = match path.file_stem().and_then(|s| s.to_str()) {
        Some("members") => Kind::Registry,
        Some("semantic") => Kind::Plane(FamilyPlane::Semantic),
        Some("callgraph") => Kind::Plane(FamilyPlane::Callgraph),
        Some("trigram") => Kind::Plane(FamilyPlane::Trigram),
        Some("symbol") => Kind::Plane(FamilyPlane::Symbol),
        _ => return Err(Error::InvalidIdentity),
    };
    let database = connections::shared(path, kind, false, true)?;
    let c = database.connection()?;
    Ok(c.query_row(
        "SELECT format_version,schema_version,layout_tag FROM format_meta WHERE singleton=1",
        [],
        |r| {
            Ok(V3FormatReport {
                store_name: kind.store().name(),
                format_version: r.get(0)?,
                schema_version: r.get(1)?,
                layout_tag: r.get(2)?,
            })
        },
    )?)
}

pub struct PlaneReader {
    database: Arc<connections::Database>,
}
impl PlaneReader {
    pub fn open_existing(storage: &Path, family: &str, plane: FamilyPlane) -> Result<Self> {
        persisted_format::register_storage_root(storage);
        Ok(Self {
            database: connections::shared(
                &plane_path(storage, family, plane)?,
                Kind::Plane(plane),
                false,
                true,
            )?,
        })
    }
    /// True when the store holds a row for `key`. A presence probe through
    /// the key index: the payload is not read.
    pub fn contains(&self, key: &FamilyKey) -> Result<bool> {
        Ok(self.database.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM blob_payloads WHERE full_key=?1)",
            [key.as_bytes().as_slice()],
            |r| r.get(0),
        )?)
    }
    pub fn get(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .database
            .connection()?
            .query_row(
                "SELECT payload,payload_digest FROM blob_payloads WHERE full_key=?1",
                [key.as_bytes().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(payload, digest)| {
            if blake3::hash(&payload).as_bytes().as_slice() != digest {
                Err(Error::Refused("payload_digest"))
            } else {
                Ok(payload)
            }
        })
        .transpose()
    }
}

/// Serializes tests that depend on the process-wide free-space override and
/// sets it to unlimited, so admission in other modules' tests never races the
/// refusal cases in `tests`.
#[cfg(test)]
pub(crate) fn serial_for_test() -> std::sync::MutexGuard<'static, ()> {
    tests::serial()
}

#[cfg(test)]
mod copy_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod source_guard;
#[cfg(test)]
mod tests;
