//! The family registry: which views and registry readers exist for one
//! repository family, the family GC epoch, and the one-sweeper lease.
//!
//! It lives at `<storage>/blobs/v2/<family>/members.sqlite`. The safety of the
//! family GC rests on one ordering rule: a view is registered here, in an
//! IMMEDIATE transaction, **before** it creates any pin, put, segment, seed pin
//! or derived clone. A sweep marks every registered member, so a view that
//! registers after marking began can only create references that touch at the
//! new epoch, which the sweep's conditional delete never removes.
//!
//! Two kinds of participant register:
//!
//! - a **view** ([`ViewRegistration`]) owns a checkout's view directory and may
//!   write family stores (it is the only source of `StoreWriteAccess`);
//! - a **registry reader** ([`ReaderRegistration`]) holds no view of its own.
//!   A multi-repo parent-folder session is one: it reads its child
//!   repositories' views and never writes, builds or repairs them. Its
//!   registration is a reader marker, not a membership, and the generations it
//!   serves are protected by read markers in the member's view directory,
//!   taken with the pin-then-verify protocol of
//!   [`ReaderRegistration::protect_current`].
//!
//! Deregistering a member whose checkout root is gone happens under the
//! registry's write lock (the handoff barrier), and only when no protection of
//! any class exists for it; see `gc::family`. Assembly and live pins are
//! created under that same lock (`ViewRegistration::under_pin_barrier`), so
//! the deregistration's last protection check cannot miss one.

use std::collections::HashMap;
use std::fmt;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

#[cfg(debug_assertions)]
#[doc(hidden)]
pub use crate::storage_retention::TestVolumeUuidGuard;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::blob_store::v2::{
    family_dir, FamilyPlane, FamilyStore, FamilyStoreReader, StoreError, StoreWriteAccess,
};
use crate::db::lifecycle::{SqliteStore, TrackedConnection};
use crate::pins::{PinError, PinOwner};

/// Schema version of `members.sqlite`. A newer version is refused rather than
/// read; extending the registry means bumping this and migrating forward.
pub const REGISTRY_SCHEMA_VERSION: i64 = 1;
pub const REGISTRY_FILE: &str = "members.sqlite";
/// Consecutive sweeps that must find a missing-root member with no protection
/// of any class before it and its view directory are removed.
pub const MISSING_ROOT_SWEEPS: u32 = 2;
/// How often a reader retries pin-then-verify when the pointer keeps moving.
const PROTECT_ATTEMPTS: usize = 8;

const REGISTRY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS registry_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    gc_epoch INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
    scope TEXT NOT NULL PRIMARY KEY,
    root BLOB,
    view_dir TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    last_bind_ms INTEGER NOT NULL,
    last_publish_ms INTEGER,
    state TEXT NOT NULL,
    missing_sweeps INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS readers (
    reader_id TEXT NOT NULL PRIMARY KEY,
    label TEXT NOT NULL,
    owner_pid INTEGER NOT NULL,
    owner_start INTEGER NOT NULL,
    registered_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sweep_lease (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    owner_pid INTEGER NOT NULL,
    owner_start INTEGER NOT NULL,
    epoch INTEGER NOT NULL,
    acquired_at_ms INTEGER NOT NULL
);
"#;

/// Live sessions of registered views, one row per [`ViewRegistration`] that a
/// process still holds, and the disk use each view had at the last
/// measurement. Disk-limit eviction never removes a view with a session row
/// whose owner process is alive, and admission of a new view decides from
/// the recorded disk use so that it never has to walk a directory.
/// `usage_record.complete` says whether the last measurement read every
/// view and the family store to the end; the rows of anything it could not
/// read keep their previous values.
///
/// The tables are created with `IF NOT EXISTS` by every writer that needs
/// them, without a schema version bump: releases that predate them never
/// read them, and their views stay protected while in use by their pins and
/// read markers.
pub(crate) const RUNTIME_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS view_sessions (
    session_id TEXT NOT NULL PRIMARY KEY,
    scope TEXT NOT NULL,
    owner_pid INTEGER NOT NULL,
    owner_start INTEGER NOT NULL,
    opened_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS view_sessions_scope ON view_sessions (scope);
CREATE TABLE IF NOT EXISTS view_usage (
    scope TEXT NOT NULL PRIMARY KEY,
    bytes INTEGER NOT NULL,
    held INTEGER NOT NULL,
    measured_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS family_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    file_bytes INTEGER NOT NULL,
    measured_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_record (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    complete INTEGER NOT NULL,
    measured_at_ms INTEGER NOT NULL
);
"#;

/// `members.state` of a view whose disk-limit eviction has begun. Eviction
/// commits this state before it deletes anything, so a process killed
/// halfway leaves a member that the next eviction pass or the view's next
/// registration finishes cleaning, instead of a half-deleted view that looks
/// intact.
pub(crate) const STATE_EVICTING: &str = "evicting";

#[derive(Debug)]
pub enum RegistryError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Store(StoreError),
    View(super::ViewError),
    InvalidScope(String),
    /// The registry file is from a newer release or is not a registry.
    Incompatible(String),
    /// The member is not registered (any more).
    NotRegistered(String),
    /// A new view could not be admitted under the family's disk limits. The
    /// text is the named reason, starting with `queued:`.
    AdmissionQueued(String),
    /// A reader could not pin a generation that stayed current long enough to
    /// verify it.
    PointerUnstable {
        scope: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "family registry I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "family registry SQLite error: {error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::View(error) => write!(f, "{error}"),
            Self::InvalidScope(scope) => write!(f, "invalid view scope `{scope}`"),
            Self::Incompatible(message) => write!(f, "incompatible family registry: {message}"),
            Self::NotRegistered(scope) => write!(f, "view `{scope}` is not registered"),
            Self::AdmissionQueued(reason) => write!(f, "{reason}"),
            Self::PointerUnstable { scope } => write!(
                f,
                "the current generation of `{scope}` kept changing while a reader pinned it"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<std::io::Error> for RegistryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<rusqlite::Error> for RegistryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<StoreError> for RegistryError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<super::ViewError> for RegistryError {
    fn from(error: super::ViewError) -> Self {
        Self::View(error)
    }
}

pub type RegistryResult<T> = Result<T, RegistryError>;

/// `<storage>/views/v2/<scope>`: the view directory of a registered member.
pub fn view_dir(storage: &Path, scope: &str) -> RegistryResult<PathBuf> {
    super::validate_scope_key(scope).map_err(|_| RegistryError::InvalidScope(scope.to_owned()))?;
    Ok(storage.join("views").join("v2").join(scope))
}

/// One registered view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberRecord {
    pub scope: String,
    /// `None` when the root could not be recorded exactly; such a member is
    /// never treated as having a missing root.
    pub root: Option<PathBuf>,
    pub view_dir: PathBuf,
    pub registered_at_ms: u64,
    pub last_bind_ms: u64,
    pub last_publish_ms: Option<u64>,
    pub missing_sweeps: u32,
    /// A disk-limit eviction of this view began and has not finished.
    pub evicting: bool,
}

/// One registry reader that holds no view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderRecord {
    pub reader_id: String,
    pub label: String,
    pub owner: PinOwner,
    pub registered_at_ms: u64,
}

struct RegistryInner {
    storage: PathBuf,
    durable_storage: Mutex<Option<PathBuf>>,
    family: String,
    path: PathBuf,
    connection: Mutex<TrackedConnection>,
    /// The disk limits this process applies to the family; see
    /// `views::eviction`. Shared by every handle on this registry file.
    budget: Mutex<super::eviction::DiskBudget>,
    /// Sizes of immutable view files measured by `views::eviction::measure`.
    /// Kept here so they live exactly as long as the registry.
    size_caches: Mutex<super::eviction::ViewSizeCaches>,
    /// Sessions that ended while the connection was busy; see
    /// `ViewSession::drop`. Never held while waiting for `connection`.
    ended_sessions: Mutex<Vec<EndedSession>>,
}

impl fmt::Debug for RegistryInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FamilyRegistry")
            .field("family", &self.family)
            .field("path", &self.path)
            .finish()
    }
}

fn open_registries() -> &'static Mutex<HashMap<PathBuf, Weak<RegistryInner>>> {
    static REGISTRIES: OnceLock<Mutex<HashMap<PathBuf, Weak<RegistryInner>>>> = OnceLock::new();
    REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A handle on one family's registry. Clones share one SQLite connection per
/// process, like the family stores.
#[derive(Clone, Debug)]
pub struct FamilyRegistry {
    inner: Arc<RegistryInner>,
}

/// A cleanup job keeps identity, not a publisher's writable connection alive.
#[derive(Clone, Debug)]
pub(crate) struct RegistryLocation {
    storage: PathBuf,
    family: String,
    durable: Option<PathBuf>,
}

impl RegistryLocation {
    pub(crate) fn open_for_sweep(&self) -> RegistryResult<Option<FamilyRegistry>> {
        let registry = FamilyRegistry::open_existing_isolated(&self.storage, &self.family, false)?;
        // The connection keeps the publisher's registry busy policy. Cleanup
        // takes the registry barrier only while it holds no other lock, so a
        // wait here can delay cleanup but never a publisher.
        if let (Some(registry), Some(durable)) = (&registry, &self.durable) {
            registry
                .bind_durable_storage(durable)
                .map_err(RegistryError::from)?;
        }
        Ok(registry)
    }
}

impl FamilyRegistry {
    /// Opens the registry, creating it when absent.
    pub fn open(storage: &Path, family: &str) -> RegistryResult<Self> {
        let dir = family_dir(storage, family)?;
        crate::private_storage::open_dir(storage, &dir)?;
        Self::open_path(storage, family, dir.join(REGISTRY_FILE), true)
    }

    /// Opens an existing registry without creating anything; `None` when the
    /// family has no v2 registry yet.
    pub fn open_existing(storage: &Path, family: &str) -> RegistryResult<Option<Self>> {
        let path = family_dir(storage, family)?.join(REGISTRY_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        Self::open_path(storage, family, path, false).map(Some)
    }

    pub(crate) fn location(&self) -> RegistryLocation {
        RegistryLocation {
            storage: self.storage().to_path_buf(),
            family: self.family().to_owned(),
            durable: self.durable_storage(),
        }
    }

    pub(crate) fn open_existing_read_only(
        storage: &Path,
        family: &str,
    ) -> RegistryResult<Option<Self>> {
        Self::open_existing_isolated(storage, family, true)
    }

    fn open_existing_isolated(
        storage: &Path,
        family: &str,
        read_only: bool,
    ) -> RegistryResult<Option<Self>> {
        let path = family_dir(storage, family)?.join(REGISTRY_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        crate::root_cache::check_cache_access(&path)?;
        let flags = if read_only {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let connection = if read_only {
            TrackedConnection::open_census(&path, SqliteStore::BlobStore)?
        } else {
            TrackedConnection::open_path_with_flags(
                &path,
                flags
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                SqliteStore::BlobStore,
            )?
        };
        connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        connection.busy_timeout(Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS))?;
        check_schema(&connection)?;
        // Isolated maintenance handles never flush a publisher's deferred
        // session updates, and closing them never checkpoints unrelated data.
        Ok(Some(Self {
            inner: Arc::new(RegistryInner {
                storage: storage.to_path_buf(),
                durable_storage: Mutex::new(None),
                family: family.to_owned(),
                path,
                connection: Mutex::new(connection),
                budget: Mutex::new(super::eviction::DiskBudget::default()),
                size_caches: Mutex::default(),
                ended_sessions: Mutex::new(Vec::new()),
            }),
        }))
    }

    fn open_path(
        storage: &Path,
        family: &str,
        path: PathBuf,
        create: bool,
    ) -> RegistryResult<Self> {
        crate::root_cache::check_cache_access(&path)?;
        let mut registries = lock(open_registries());
        registries.retain(|_, registry| registry.strong_count() > 0);
        if let Some(inner) = registries.get(&path).and_then(Weak::upgrade) {
            return Ok(Self { inner });
        }
        let connection = if create {
            TrackedConnection::open(&path, SqliteStore::BlobStore)?
        } else {
            TrackedConnection::open_path_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                SqliteStore::BlobStore,
            )?
        };
        connection.busy_timeout(Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS))?;
        let inner = Arc::new(RegistryInner {
            storage: storage.to_path_buf(),
            durable_storage: Mutex::new(None),
            family: family.to_owned(),
            path: path.clone(),
            connection: Mutex::new(connection),
            budget: Mutex::new(super::eviction::DiskBudget::default()),
            size_caches: Mutex::default(),
            ended_sessions: Mutex::new(Vec::new()),
        });
        {
            let mut connection = lock(&inner.connection);
            if create {
                crate::blob_store::retry_while_busy(
                    Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS),
                    || connection.pragma_update(None, "journal_mode", "WAL"),
                )?;
                connection.pragma_update(None, "synchronous", "FULL")?;
                let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute_batch(REGISTRY_SCHEMA)?;
                tx.execute_batch(RUNTIME_SCHEMA)?;
                tx.execute(
                    "INSERT OR IGNORE INTO registry_meta (singleton, schema_version, gc_epoch)
                     VALUES (1, ?1, 0)",
                    params![REGISTRY_SCHEMA_VERSION],
                )?;
                check_schema(&tx)?;
                tx.commit()?;
            } else {
                check_schema(&connection)?;
            }
        }
        registries.insert(path, Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    pub fn family(&self) -> &str {
        &self.inner.family
    }

    /// The registry connection, after writing any session ends queued while
    /// it was busy, so that nothing read through it sees a session that has
    /// already ended in this process.
    fn connection(&self) -> MutexGuard<'_, TrackedConnection> {
        #[cfg(test)]
        CONNECTION_BLOCKED.with(|hook| {
            if matches!(
                self.inner.connection.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ) {
                if let Some(blocked) = hook.borrow_mut().take() {
                    blocked.send(()).unwrap();
                }
            }
        });
        let connection = lock(&self.inner.connection);
        flush_ended_sessions(&self.inner, &connection);
        connection
    }

    pub fn storage(&self) -> &Path {
        &self.inner.storage
    }

    /// Treat the `anchor` path under this registry's storage as a mounted volume
    /// for a fixture. Dropping the guard restores real volume discovery.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn test_volume_uuid_guard(
        &self,
        anchor: &Path,
        uuid: &str,
    ) -> std::io::Result<TestVolumeUuidGuard> {
        crate::storage_retention::test_volume_uuid_guard(self.storage(), anchor, uuid)
    }

    pub(crate) fn bind_durable_storage(&self, storage: &Path) -> RegistryResult<()> {
        crate::root_cache::register_durable_owner(&crate::root_cache::StoragePaths::new(
            storage.to_path_buf(),
            self.inner.storage.clone(),
        ))?;
        let mut durable = lock(&self.inner.durable_storage);
        if durable.as_ref().is_some_and(|old| {
            old != storage
                && crate::config_live::dir_identity(old)
                    != crate::config_live::dir_identity(storage)
        }) {
            return Err(std::io::Error::other(
                "family registry attached to another durable storage root",
            )
            .into());
        }
        *durable = Some(storage.to_path_buf());
        Ok(())
    }

    /// Declares the durable storage root that owns this registry's cache
    /// folders, as a running AFT process does before it evicts anything:
    /// view-directory removal refuses a cache whose durable owner is unknown.
    /// Production code binds through [`Self::register_view_with_storage`] or
    /// storage retention; integration tests that evict from a separate child
    /// process call this instead of registering a view of their own.
    #[doc(hidden)]
    pub fn bind_durable_storage_for_tests(&self, storage: &Path) -> RegistryResult<()> {
        self.bind_durable_storage(storage)
    }
    pub(crate) fn durable_storage(&self) -> Option<PathBuf> {
        lock(&self.inner.durable_storage).clone()
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Registers (or re-binds) a view in one IMMEDIATE transaction. This must
    /// be the first durable step of any view work; the returned registration
    /// is the only way to obtain write access to the family's stores.
    ///
    /// The registration is also the view's session: while any clone of it is
    /// alive, a `view_sessions` row names this process, and disk-limit
    /// eviction leaves the view alone. A scope that is not registered yet is
    /// a new view whose first generation will add bytes, so it must first be
    /// admitted under the family's disk limits; see
    /// [`super::eviction::admit_new_view`]. Re-binding an existing member is
    /// never refused.
    pub fn register_view_with_storage(
        &self,
        durable_storage: &Path,
        scope: &str,
        root: &Path,
    ) -> RegistryResult<ViewRegistration> {
        self.bind_durable_storage(durable_storage)?;
        if matches!(root.try_exists(), Ok(true)) {
            let paths = crate::root_cache::StoragePaths::new(
                durable_storage.to_path_buf(),
                self.inner.storage.clone(),
            );
            crate::storage_retention::record_bind(&paths, root, self.family())?;
        }
        let view_dir = view_dir(&self.inner.storage, scope)?;
        if self.member(scope)?.is_none() {
            super::eviction::admit_new_view(self)?;
        }
        let now = now_ms();
        let owner = current_owner();
        let session_id = format!(
            "{}-{}-{}",
            owner.pid,
            owner.start_time,
            SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut connection = self.connection();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(RUNTIME_SCHEMA)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT state FROM members WHERE scope = ?1",
                params![scope],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(STATE_EVICTING) {
            // An eviction began deleting this view and was interrupted. It
            // removed the pointer first, so nothing here is published; clear
            // the rest before the view is used again, so a rebuilt generation
            // never meets a leftover file of the same name. Anything still
            // protected (or protection that cannot be read) is left alone.
            if matches!(crate::gc::family::view_has_protection(&view_dir), Ok(false)) {
                remove_view_dir(&view_dir)?;
            }
        }
        tx.execute(
            "INSERT INTO members
                 (scope, root, view_dir, registered_at_ms, last_bind_ms, state, missing_sweeps)
             VALUES (?1, ?2, ?3, ?4, ?4, 'active', 0)
             ON CONFLICT(scope) DO UPDATE SET
                 root = excluded.root,
                 view_dir = excluded.view_dir,
                 last_bind_ms = excluded.last_bind_ms,
                 state = 'active',
                 missing_sweeps = 0",
            params![scope, encode_root(root), path_text(&view_dir), now as i64],
        )?;
        tx.execute(
            "INSERT INTO view_sessions (session_id, scope, owner_pid, owner_start, opened_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                scope,
                i64::from(owner.pid),
                owner.start_time as i64,
                now as i64
            ],
        )?;
        tx.commit()?;
        drop(connection);
        crate::private_storage::open_dir(&self.inner.storage, &view_dir)?;
        Ok(ViewRegistration {
            registry: self.clone(),
            scope: scope.to_owned(),
            root: root.to_path_buf(),
            view_dir,
            session: Arc::new(ViewSession {
                registry: self.clone(),
                session_id,
                scope: scope.to_owned(),
            }),
        })
    }

    /// The disk limits this process applies to the family.
    pub fn disk_budget(&self) -> super::eviction::DiskBudget {
        *lock(&self.inner.budget)
    }

    /// Replaces the disk limits for every handle on this registry in this
    /// process. Production keeps the defaults; tests use small limits to
    /// exercise eviction and admission with real files.
    pub fn set_disk_budget(&self, budget: super::eviction::DiskBudget) {
        *lock(&self.inner.budget) = budget;
    }

    /// The view size caches of this family, shared by every handle on this
    /// registry in this process and dropped with the last one.
    pub(super) fn view_size_caches(&self) -> &Mutex<super::eviction::ViewSizeCaches> {
        &self.inner.size_caches
    }

    /// Every recorded view session, live or not.
    pub fn sessions(&self) -> RegistryResult<Vec<SessionRecord>> {
        let connection = self.connection();
        read_sessions(&connection, None)
    }

    /// Removes session rows whose owner process is gone. Their views count
    /// as having no session whether or not the rows are removed; this only
    /// keeps the table small.
    pub(crate) fn reclaim_dead_sessions(&self) -> RegistryResult<usize> {
        let dead = self
            .sessions()?
            .into_iter()
            .filter(|session| !crate::pins::owner_is_live(&session.owner))
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        let connection = self.connection();
        for session_id in &dead {
            connection.execute(
                "DELETE FROM view_sessions WHERE session_id = ?1",
                params![session_id],
            )?;
        }
        Ok(dead.len())
    }

    /// Registers a reader that holds no view. The row is removed when the
    /// registration is dropped; a crashed reader's row is reclaimed by the
    /// next sweep once its owner process is gone.
    pub fn register_reader(&self, label: &str) -> RegistryResult<ReaderRegistration> {
        let owner = current_owner();
        let reader_id = format!(
            "{}-{}-{}",
            owner.pid,
            owner.start_time,
            READER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.connection().execute(
            "INSERT INTO readers (reader_id, label, owner_pid, owner_start, registered_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                reader_id,
                label,
                i64::from(owner.pid),
                owner.start_time as i64,
                now_ms() as i64
            ],
        )?;
        Ok(ReaderRegistration {
            registry: self.clone(),
            reader_id,
            label: label.to_owned(),
        })
    }

    pub fn members(&self) -> RegistryResult<Vec<MemberRecord>> {
        read_members(&self.connection())
    }

    pub(crate) fn members_bounded(&self, limit: usize) -> RegistryResult<Vec<MemberRecord>> {
        let connection = self.connection();
        let members = read_members_limited(&connection, limit.saturating_add(1))?;
        if members.len() > limit {
            return Err(RegistryError::Incompatible(format!(
                "member walk exceeded {limit} rows"
            )));
        }
        Ok(members)
    }

    pub fn member(&self, scope: &str) -> RegistryResult<Option<MemberRecord>> {
        Ok(read_members(&self.connection())?
            .into_iter()
            .find(|member| member.scope == scope))
    }

    pub fn readers(&self) -> RegistryResult<Vec<ReaderRecord>> {
        let connection = self.connection();
        let mut statement = connection.prepare(
            "SELECT reader_id, label, owner_pid, owner_start, registered_at_ms
             FROM readers ORDER BY reader_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ReaderRecord {
                reader_id: row.get(0)?,
                label: row.get(1)?,
                owner: PinOwner {
                    pid: row.get::<_, i64>(2)? as u32,
                    start_time: row.get::<_, i64>(3)?.max(0) as u64,
                },
                registered_at_ms: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn gc_epoch(&self) -> RegistryResult<u64> {
        let epoch: i64 = self.connection().query_row(
            "SELECT gc_epoch FROM registry_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(epoch.max(0) as u64)
    }

    /// Records a successful publication of `scope`.
    pub fn note_publish(&self, scope: &str) -> RegistryResult<()> {
        self.connection().execute(
            "UPDATE members SET last_publish_ms = ?2 WHERE scope = ?1",
            params![scope, now_ms() as i64],
        )?;
        Ok(())
    }

    /// Claims a lease and starts an unconditional deleting pass.
    #[cfg(test)]
    pub(crate) fn begin_sweep(&self) -> RegistryResult<Option<SweepLease>> {
        let Some(mut lease) = self.acquire_sweep()? else {
            return Ok(None);
        };
        lease.advance_epoch()?;
        Ok(Some(lease))
    }

    /// Claims the family's one-sweeper lease without changing its epoch.
    /// A live owner keeps the lease; a dead owner's lease is taken over.
    pub(crate) fn acquire_sweep(&self) -> RegistryResult<Option<SweepLease>> {
        let owner = current_owner();
        let mut connection = self.connection();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holder: Option<(i64, i64)> = tx
            .query_row(
                "SELECT owner_pid, owner_start FROM sweep_lease WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((pid, start)) = holder {
            let holder = PinOwner {
                pid: pid as u32,
                start_time: start.max(0) as u64,
            };
            // A live holder, including another thread of this process, keeps
            // the lease; only a dead owner's lease is taken over.
            if crate::pins::owner_is_live(&holder) {
                return Ok(None);
            }
        }
        let epoch: i64 = tx.query_row(
            "SELECT gc_epoch FROM registry_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO sweep_lease (singleton, owner_pid, owner_start, epoch, acquired_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(singleton) DO UPDATE SET owner_pid = excluded.owner_pid,
                 owner_start = excluded.owner_start, epoch = excluded.epoch,
                 acquired_at_ms = excluded.acquired_at_ms",
            params![
                i64::from(owner.pid),
                owner.start_time as i64,
                epoch,
                now_ms() as i64
            ],
        )?;
        tx.commit()?;
        Ok(Some(SweepLease {
            registry: self.clone(),
            epoch: epoch.max(0) as u64,
            owner,
            released: false,
        }))
    }

    /// Runs `body` on the registry connection without opening a
    /// transaction, for reads that take no write lock.
    pub(crate) fn read<T>(
        &self,
        body: impl FnOnce(&rusqlite::Connection) -> RegistryResult<T>,
    ) -> RegistryResult<T> {
        body(&self.connection())
    }

    /// Runs `body` inside one IMMEDIATE transaction on the registry. This is
    /// the handoff barrier: registration takes the same write lock, so no view
    /// can (re)register between the body's checks and its effects.
    pub(crate) fn with_barrier<T>(
        &self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> RegistryResult<T>,
    ) -> RegistryResult<T> {
        let mut connection = self.connection();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = body(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Checks that `scope` is registered, inside the handoff barrier.
    pub(crate) fn confirm_member(&self, scope: &str) -> RegistryResult<bool> {
        self.with_barrier(|tx| {
            Ok(tx
                .query_row(
                    "SELECT 1 FROM members WHERE scope = ?1",
                    params![scope],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    /// Removes reader rows whose owner process is gone.
    pub(crate) fn reclaim_dead_readers(&self) -> RegistryResult<usize> {
        let dead = self
            .readers()?
            .into_iter()
            .filter(|reader| !crate::pins::owner_is_live(&reader.owner))
            .map(|reader| reader.reader_id)
            .collect::<Vec<_>>();
        let connection = self.connection();
        for reader_id in &dead {
            connection.execute(
                "DELETE FROM readers WHERE reader_id = ?1",
                params![reader_id],
            )?;
        }
        Ok(dead.len())
    }
}

/// The one-sweeper lease of a family, holding the sweep's epoch `S`.
#[derive(Debug)]
pub struct SweepLease {
    registry: FamilyRegistry,
    epoch: u64,
    owner: PinOwner,
    released: bool,
}

impl SweepLease {
    /// Production deleting passes call this after preflight finds possible
    /// work. The lease stays held through marking and deletion; the test-only
    /// begin_sweep helper advances unconditionally.
    pub(crate) fn advance_epoch(&mut self) -> RegistryResult<()> {
        self.registry.with_barrier(|tx| {
            tx.execute(
                "UPDATE registry_meta SET gc_epoch = gc_epoch + 1 WHERE singleton = 1",
                [],
            )?;
            self.epoch = tx
                .query_row(
                    "SELECT gc_epoch FROM registry_meta WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?
                .max(0) as u64;
            tx.execute(
                "UPDATE sweep_lease SET epoch = ?1 WHERE singleton = 1",
                params![self.epoch as i64],
            )?;
            Ok(())
        })
    }

    /// Both periodic callers consult this clock while holding the same lease.
    /// A forced admission pass ignores it. Failed or partial passes do not
    /// postpone a retry.
    pub(crate) fn due(&self, force: bool) -> RegistryResult<bool> {
        let connection = self.registry.connection();
        connection.execute_batch("CREATE TABLE IF NOT EXISTS sweep_schedule (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), completed_at_ms INTEGER NOT NULL)")?;
        let last: Option<i64> = connection
            .query_row(
                "SELECT completed_at_ms FROM sweep_schedule WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(force
            || last.is_none_or(|last| {
                now_ms().saturating_sub(last.max(0) as u64)
                    >= super::eviction::ENFORCEMENT_INTERVAL.as_millis() as u64
            }))
    }

    pub(crate) fn completed(&self) -> RegistryResult<()> {
        self.registry.connection().execute("INSERT INTO sweep_schedule VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET completed_at_ms = excluded.completed_at_ms", params![now_ms() as i64])?;
        Ok(())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = self.registry.connection().execute(
            "DELETE FROM sweep_lease WHERE singleton = 1 AND owner_pid = ?1 AND owner_start = ?2",
            params![i64::from(self.owner.pid), self.owner.start_time as i64],
        );
    }
}

impl Drop for SweepLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// A registered view: the capability to write the family's stores and to
/// publish into its own view directory.
#[derive(Clone, Debug)]
pub struct ViewRegistration {
    registry: FamilyRegistry,
    scope: String,
    root: PathBuf,
    view_dir: PathBuf,
    /// Shared by every clone, so the session ends when the last clone of
    /// this registration is dropped.
    session: Arc<ViewSession>,
}

/// One process's live use of a view. Dropping it removes its session row and
/// records the time as the view's most recent use (`last_bind_ms`), which is
/// what least-recently-used eviction orders by.
#[derive(Debug)]
struct ViewSession {
    registry: FamilyRegistry,
    session_id: String,
    scope: String,
}

/// A session that ended but whose row is not deleted yet.
#[derive(Debug)]
struct EndedSession {
    session_id: String,
    scope: String,
    ended_at_ms: u64,
}

impl Drop for ViewSession {
    /// Never waits for the registry connection. The last clone of a
    /// registration can be dropped anywhere, including inside a barrier body
    /// on this thread, which holds that connection's mutex; blocking on it
    /// here would deadlock. So the end is queued, and written now only if
    /// the connection is free. Otherwise a short-lived thread writes it as
    /// soon as the holder lets go, and every later use of the connection in
    /// this process writes queued ends first, before reading anything.
    fn drop(&mut self) {
        let ended = EndedSession {
            session_id: std::mem::take(&mut self.session_id),
            scope: std::mem::take(&mut self.scope),
            ended_at_ms: now_ms(),
        };
        lock(&self.registry.inner.ended_sessions).push(ended);
        match self.registry.inner.connection.try_lock() {
            Ok(connection) => flush_ended_sessions(&self.registry.inner, &connection),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                flush_ended_sessions(&self.registry.inner, &poisoned.into_inner())
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let registry = self.registry.clone();
                // If no thread can be started, the queued end is written by
                // this process's next use of the registry; until then the
                // row names a live process and keeps the view protected,
                // which errs on the safe side.
                let _ = std::thread::Builder::new()
                    .name("aft-view-session-end".to_string())
                    .spawn(move || drop(registry.connection()));
            }
        }
    }
}

/// Writes every queued session end: deletes its row and records its end as
/// the member's latest use. Called with the connection mutex held and no
/// transaction open on it.
fn flush_ended_sessions(inner: &RegistryInner, connection: &rusqlite::Connection) {
    let ended = std::mem::take(&mut *lock(&inner.ended_sessions));
    for session in ended {
        let _ = connection.execute(
            "DELETE FROM view_sessions WHERE session_id = ?1",
            params![session.session_id],
        );
        let _ = connection.execute(
            "UPDATE members SET last_bind_ms = MAX(last_bind_ms, ?2) WHERE scope = ?1",
            params![session.scope, session.ended_at_ms as i64],
        );
    }
}

/// One recorded view session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    pub scope: String,
    pub owner: PinOwner,
    pub opened_at_ms: u64,
}

impl ViewRegistration {
    pub fn registry(&self) -> &FamilyRegistry {
        &self.registry
    }

    pub fn family(&self) -> &str {
        self.registry.family()
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn view_dir(&self) -> &Path {
        &self.view_dir
    }

    /// The id of this registration's session row in `view_sessions`.
    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    pub fn write_access(&self) -> StoreWriteAccess {
        StoreWriteAccess::for_registered_view(self.registry.storage(), self.registry.family())
    }

    pub fn open_store(&self, plane: FamilyPlane) -> RegistryResult<FamilyStore> {
        Ok(FamilyStore::open(&self.write_access(), plane)?)
    }

    /// The view's own pointer and manifests in `views/v2/<scope>`.
    /// This checkout's member in the v3 namespace: the same family and scope
    /// under `blobs/v3/<family>/views/<scope>`.
    pub fn v3_route(&self) -> super::snapshot::V3Route {
        super::snapshot::V3Route {
            storage: self.registry().storage().to_path_buf(),
            family: self.family().to_owned(),
            scope: self.scope().to_owned(),
        }
    }

    pub fn view_store(&self) -> RegistryResult<super::ViewStore> {
        let view = super::ViewStore::open_dir(self.view_dir.clone())?
            .with_sweep_registry(self.registry.clone());
        view.recover_abandoned_derived(Some(self.registry()));
        Ok(view)
    }

    /// Runs `create` inside the registry's handoff barrier, after confirming
    /// that this view is still registered; assembly and live pins are created
    /// this way. A missing-root deregistration re-checks for pins under the
    /// same barrier immediately before it removes a view, so it either sees a
    /// pin created here or has already removed the member. In the second case
    /// `create` does not run, nothing re-creates the removed view directory,
    /// and the caller gets [`PinError::NotRegistered`]; it has to register
    /// again before doing more work.
    pub(crate) fn under_pin_barrier<T>(
        &self,
        create: impl FnOnce(&Path) -> Result<T, PinError>,
    ) -> Result<T, PinError> {
        let outcome = self.registry.with_barrier(|tx| {
            let registered = tx
                .query_row(
                    "SELECT 1 FROM members WHERE scope = ?1",
                    params![self.scope],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            Ok(registered.then(|| create(&self.view_dir)))
        });
        match outcome {
            Ok(Some(created)) => created,
            Ok(None) => Err(PinError::NotRegistered(self.scope.clone())),
            Err(error) => Err(PinError::Registry(error.to_string())),
        }
    }
}

#[cfg(test)]
thread_local! {
    // Only the pinning thread observes contention; unrelated registry users do
    // not consume its acknowledgement.
    static CONNECTION_BLOCKED: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

static READER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A registered reader that holds no view. It can list the family's members,
/// read their stores and pin their current generations, and nothing else.
#[derive(Debug)]
pub struct ReaderRegistration {
    registry: FamilyRegistry,
    reader_id: String,
    label: String,
}

impl ReaderRegistration {
    pub fn reader_id(&self) -> &str {
        &self.reader_id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn family(&self) -> &str {
        self.registry.family()
    }

    pub fn members(&self) -> RegistryResult<Vec<MemberRecord>> {
        self.registry.members()
    }

    /// A read-only handle on one family store, or `None` if it was never
    /// created. Readers never create stores.
    pub fn open_store(&self, plane: FamilyPlane) -> RegistryResult<Option<FamilyStoreReader>> {
        Ok(FamilyStoreReader::open_existing(
            self.registry.storage(),
            self.registry.family(),
            plane,
        )?)
    }

    /// Pins the member's current generation with a read marker, then verifies
    /// that the pointer still names it and the member is still registered. A
    /// generation the pointer moved away from in between is released and the
    /// pin retried. `Ok(None)` means the member has no view directory or no
    /// published generation; nothing is created in that case.
    pub fn protect_current(&self, scope: &str) -> RegistryResult<Option<ProtectedGeneration>> {
        let Some(member) = self.registry.member(scope)? else {
            return Err(RegistryError::NotRegistered(scope.to_owned()));
        };
        for _ in 0..PROTECT_ATTEMPTS {
            let Some(store) = super::ViewStore::existing_dir(member.view_dir.clone()) else {
                return Ok(None);
            };
            let Some(generation) = store.current_generation_read_only()? else {
                return Ok(None);
            };
            let marker = crate::root_cache::ReadMarker::create(&member.view_dir, &generation)?;
            let still_current =
                store.current_generation_read_only()?.as_deref() == Some(generation.as_str());
            // Confirm membership under the registry's write lock: a missing-root
            // deregistration holds that lock while it checks for markers, so
            // either it saw this marker, or this check sees the member gone.
            let still_registered = self.registry.confirm_member(scope)?;
            if still_current && still_registered {
                return Ok(Some(ProtectedGeneration {
                    scope: scope.to_owned(),
                    view_dir: member.view_dir.clone(),
                    generation,
                    marker,
                }));
            }
            drop(marker);
            if !still_registered {
                return Err(RegistryError::NotRegistered(scope.to_owned()));
            }
        }
        Err(RegistryError::PointerUnstable {
            scope: scope.to_owned(),
        })
    }
}

impl Drop for ReaderRegistration {
    fn drop(&mut self) {
        let _ = self.registry.connection().execute(
            "DELETE FROM readers WHERE reader_id = ?1",
            params![self.reader_id],
        );
    }
}

/// A generation protected by a read marker in its view directory. It stays
/// protected (from both the family sweep and the view's own generation sweep)
/// until this value is dropped.
#[derive(Debug)]
pub struct ProtectedGeneration {
    scope: String,
    view_dir: PathBuf,
    generation: String,
    marker: crate::root_cache::ReadMarker,
}

impl ProtectedGeneration {
    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn view_dir(&self) -> &Path {
        &self.view_dir
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn marker_path(&self) -> &Path {
        self.marker.path()
    }

    /// Keeps a long-lived (resident) marker fresh for cross-host readers.
    pub fn touch_if_due(&self) -> std::io::Result<()> {
        self.marker.touch_if_due()
    }
}

pub(crate) fn current_owner() -> PinOwner {
    let pid = std::process::id();
    PinOwner {
        pid,
        start_time: crate::root_cache::process_start_time_ms(pid).unwrap_or_else(now_ms),
    }
}

pub(crate) fn read_members(connection: &rusqlite::Connection) -> RegistryResult<Vec<MemberRecord>> {
    read_members_limited(connection, usize::MAX)
}

fn read_members_limited(
    connection: &rusqlite::Connection,
    limit: usize,
) -> RegistryResult<Vec<MemberRecord>> {
    let mut statement = connection.prepare(
        "SELECT scope, root, view_dir, registered_at_ms, last_bind_ms, last_publish_ms,
                missing_sweeps, state
         FROM members ORDER BY scope LIMIT ?1",
    )?;
    let rows = statement.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        Ok(MemberRecord {
            scope: row.get(0)?,
            root: row.get::<_, Option<Vec<u8>>>(1)?.and_then(decode_root),
            view_dir: PathBuf::from(row.get::<_, String>(2)?),
            registered_at_ms: row.get::<_, i64>(3)?.max(0) as u64,
            last_bind_ms: row.get::<_, i64>(4)?.max(0) as u64,
            last_publish_ms: row
                .get::<_, Option<i64>>(5)?
                .map(|value| value.max(0) as u64),
            missing_sweeps: row.get::<_, i64>(6)?.max(0) as u32,
            evicting: row.get::<_, String>(7)? == STATE_EVICTING,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Session rows, all of them or those of one scope. A registry that no
/// writer of this release has opened yet has no session table, which means
/// no sessions.
pub(crate) fn read_sessions(
    connection: &rusqlite::Connection,
    scope: Option<&str>,
) -> RegistryResult<Vec<SessionRecord>> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'view_sessions'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT session_id, scope, owner_pid, owner_start, opened_at_ms FROM view_sessions
         WHERE ?1 IS NULL OR scope = ?1 ORDER BY session_id",
    )?;
    let rows = statement.query_map(params![scope], |row| {
        Ok(SessionRecord {
            session_id: row.get(0)?,
            scope: row.get(1)?,
            owner: PinOwner {
                pid: row.get::<_, i64>(2)? as u32,
                start_time: row.get::<_, i64>(3)?.max(0) as u64,
            },
            opened_at_ms: row.get::<_, i64>(4)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// True when a process that is still alive holds a session on `scope`.
pub(crate) fn has_live_session(
    connection: &rusqlite::Connection,
    scope: &str,
) -> RegistryResult<bool> {
    Ok(read_sessions(connection, Some(scope))?
        .iter()
        .any(|session| crate::pins::owner_is_live(&session.owner)))
}

/// Removes a view directory, treating one that is already gone as removed.
pub(crate) fn remove_view_dir(view_dir: &Path) -> std::io::Result<()> {
    match crate::storage_retention::remove_directory_if_closed(view_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn remove_view_dir_with(
    view_dir: &Path,
    before_remove: impl FnOnce(&crate::bash_background::persistence::PinnedDir) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match crate::storage_retention::remove_directory_if_closed_with(view_dir, before_remove) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_schema(connection: &rusqlite::Connection) -> RegistryResult<()> {
    let version: Option<i64> = connection
        .query_row(
            "SELECT schema_version FROM registry_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version {
        Some(REGISTRY_SCHEMA_VERSION) => Ok(()),
        Some(other) => Err(RegistryError::Incompatible(format!(
            "schema {other}, this release reads schema {REGISTRY_SCHEMA_VERSION}"
        ))),
        None => Err(RegistryError::Incompatible(
            "no registry metadata row".to_string(),
        )),
    }
}

/// Roots are stored as exact OS bytes where the platform allows it. A root that
/// cannot be stored exactly is stored as NULL, which the sweep treats as
/// "never missing" rather than risk removing a live checkout's view.
fn encode_root(root: &Path) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(root.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        root.to_str().map(|text| text.as_bytes().to_vec())
    }
}

fn decode_root(bytes: Vec<u8>) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_ms() -> u64 {
    crate::pins::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_only_cleanup_does_not_retain_writers_or_change_closed_storage() {
        fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            let mut files = std::collections::BTreeMap::new();
            let mut pending = vec![root.to_path_buf()];
            while let Some(dir) = pending.pop() {
                for entry in fs::read_dir(dir).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        pending.push(entry.path());
                    } else {
                        files.insert(
                            entry.path().strip_prefix(root).unwrap().to_path_buf(),
                            fs::read(entry.path()).unwrap(),
                        );
                    }
                }
            }
            files
        }
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let registration = registry
            .register_view_with_storage(
                storage.path(),
                "quiescent",
                &storage.path().join("absent-root"),
            )
            .unwrap();
        let view = registration.view_store().unwrap();
        let view_dir = view.view_dir().to_path_buf();
        let pointer = view.pointer_path();
        let registry_path = registry.path().to_path_buf();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let _hook = crate::storage_retention::observe_test_hook(
            &view_dir,
            Arc::new(move |step| {
                if step == "post-publication-sweep" {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let manifest =
            crate::views::manifest_v2::ManifestV2::new(crate::views::manifest_v2::ManifestHeader {
                producers: crate::views::manifest_v2::Producers {
                    trigram: "t".into(),
                    semantic: None,
                    callgraph: "c".into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            });
        let name = crate::views::manifest_v2::GenerationName::for_manifest(&manifest).unwrap();
        view.commit_v2(view.prepare_v2(&name, None, &manifest, None).unwrap(), None)
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(view);
        drop(registration);
        drop(registry);
        let writers_closed = crate::db::file_identity::open_connections(&registry_path) == 0
            && crate::db::file_identity::open_connections(&pointer) == 0;
        // No raw descriptor is used until every writer has closed.
        let before = writers_closed.then(|| snapshot(storage.path()));
        release_tx.send(()).unwrap();
        super::super::generation::wait_for_generation_sweep_for_test(&view_dir);
        assert!(
            writers_closed,
            "background cleanup retained a publisher's writable registry"
        );
        let before = before.unwrap();
        let after = snapshot(storage.path());
        let changed: std::collections::BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .collect();
        assert!(
            changed.is_empty(),
            "current-only cleanup changed unrelated storage: {changed:?}"
        );
    }

    #[test]
    fn v2_generation_sweep_holds_registry_barrier_during_unlink() {
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let registry = FamilyRegistry::open(storage.path(), "0123456789abcdef").unwrap();
        let registration = registry
            .register_view_with_storage(storage.path(), "barrier", &root)
            .unwrap();
        let view = registration.view_store().unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"obsolete").unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = std::sync::atomic::AtomicBool::new(false);
        let _hook = crate::storage_retention::observe_test_hook(
            view.view_dir(),
            Arc::new(move |step| {
                if step == "generation-delete"
                    && !once.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        super::super::generation::schedule_generation_sweep(view.clone());
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let held = {
            let mut connection = registry.connection();
            connection.busy_timeout(Duration::ZERO).unwrap();
            let blocked = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .err()
                .is_some_and(|error| {
                    error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                });
            connection
                .busy_timeout(Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS))
                .unwrap();
            blocked
        };
        release_tx.send(()).unwrap();
        super::super::generation::wait_for_generation_sweep_for_test(view.view_dir());
        assert!(held, "V2 unlink must exclude registry pin admission");
        assert!(!old.exists());
    }

    #[test]
    fn registration_is_idempotent_and_readers_leave_on_drop() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let root = storage.path().join("checkout");
        registry
            .register_view_with_storage(registry.storage(), "scope-a", &root)
            .unwrap();
        registry
            .register_view_with_storage(registry.storage(), "scope-a", &root)
            .unwrap();
        let members = registry.members().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].root.as_deref(), Some(root.as_path()));

        let reader = registry.register_reader("parent-folder").unwrap();
        assert_eq!(registry.readers().unwrap().len(), 1);
        assert!(
            registry.members().unwrap().len() == 1,
            "a reader is not a member"
        );
        drop(reader);
        assert!(registry.readers().unwrap().is_empty());
    }

    #[test]
    fn a_reader_does_not_create_a_registry_or_a_view() {
        let storage = tempfile::tempdir().unwrap();
        assert!(FamilyRegistry::open_existing(storage.path(), "family")
            .unwrap()
            .is_none());
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        registry
            .register_view_with_storage(registry.storage(), "scope-a", &storage.path().join("root"))
            .unwrap();
        let reader = registry.register_reader("parent-folder").unwrap();
        let view = view_dir(storage.path(), "scope-a").unwrap();
        fs::remove_dir_all(&view).unwrap();
        assert!(reader.protect_current("scope-a").unwrap().is_none());
        assert!(
            !view.exists(),
            "protecting an absent view must not create it"
        );
    }

    #[test]
    fn only_one_sweeper_holds_the_lease_and_each_sweep_bumps_the_epoch() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let first = registry.begin_sweep().unwrap().unwrap();
        assert_eq!(first.epoch(), 1);
        first.release();
        let second = registry.begin_sweep().unwrap().unwrap();
        assert_eq!(second.epoch(), 2);
        assert_eq!(registry.gc_epoch().unwrap(), 2);
    }

    /// Dropping the last clone of a registration inside a barrier body, on
    /// the thread that holds the registry connection, returns instead of
    /// waiting for that connection; the session ends as soon as the barrier
    /// is released, and the end time becomes the view's last use.
    #[test]
    fn dropping_the_last_registration_inside_a_barrier_returns_and_ends_the_session() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), "scope-a", &storage.path().join("root"))
            .unwrap();
        assert_eq!(registry.sessions().unwrap().len(), 1);
        registry
            .with_barrier(|tx| {
                tx.execute("UPDATE members SET last_bind_ms = 1", [])?;
                Ok(())
            })
            .unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let registry = registry.clone();
            move || {
                registry
                    .with_barrier(move |_| {
                        drop(view);
                        Ok(())
                    })
                    .unwrap();
                done_tx.send(()).unwrap();
            }
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("dropping the last registration inside a barrier deadlocked");
        worker.join().unwrap();
        assert!(registry.sessions().unwrap().is_empty());
        assert!(registry.member("scope-a").unwrap().unwrap().last_bind_ms > 1);
    }

    /// A pin started while another holder has the barrier waits for it. When
    /// that holder deregisters the view before letting go, the pin is refused.
    /// The holder waits for an observed contended connection acquisition before
    /// deleting the member, rather than assuming the pinning thread ran first.
    #[test]
    fn a_pin_waits_for_the_barrier_and_is_refused_after_a_removal_inside_it() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), "scope-a", &storage.path().join("root"))
            .unwrap();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let pinning = registry
            .with_barrier(|tx| {
                let pinning = std::thread::spawn({
                    let view = view.clone();
                    move || {
                        CONNECTION_BLOCKED.with(|hook| *hook.borrow_mut() = Some(blocked_tx));
                        crate::pins::LivePin::create(&view).map(drop)
                    }
                });
                blocked_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("pin must encounter the held registry barrier");
                tx.execute("DELETE FROM members WHERE scope = ?1", params!["scope-a"])?;
                Ok(pinning)
            })
            .unwrap();
        let result = pinning.join().unwrap();
        assert!(
            matches!(result, Err(PinError::NotRegistered(_))),
            "{result:?}"
        );
    }
}
