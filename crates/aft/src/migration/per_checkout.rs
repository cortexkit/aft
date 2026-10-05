//! Durable, versioned import of pre-view index artifacts into a per-checkout
//! (v2) view.
//!
//! A legacy set is the group of directories an older AFT keeps for one
//! artifact key: `index/<key>/cache.bin` (trigram), `semantic/<key>/semantic.bin`
//! (vectors), `callgraph/<key>/` (SQLite stores) and `artifact-owners/<key>/`.
//! The import converts what can be proven equal to a cold build into the
//! family's v2 stores and publishes it as the binding view's first
//! generation, which the first load then strictly reconciles like any other
//! persisted generation. It never writes, moves or deletes the legacy set:
//! the previous binary can still use it after an offline rollback, and only
//! the explicit `aft cache prune-legacy` command removes it later.
//!
//! The import is a per-key state machine with one row per legacy artifact in
//! `<storage>/migration/v2/<key>/imports.sqlite`:
//!
//! ```text
//! (absent) → claimed{owner, attempt} → staged → validated → registered → done
//!                                    ↘ rejected{reason}
//! ```
//!
//! - **claimed**: an owner (pid, process start, per-claim token) holds the row.
//!   A live owner makes every other caller back off (single flight); a dead
//!   owner's row is taken over with `attempt + 1` and resumes from its state.
//! - **staged**: the canonical conversion is written to a staging file and
//!   fsynced; the row records the hashes of the legacy source and the staged
//!   bytes.
//! - **validated**: the staged bytes were re-read and decoded, their keys
//!   re-derived, and the legacy source is still the bytes that were converted.
//! - **registered**: every payload is in the family store and the trigram
//!   segment row is committed under its content-derived name.
//! - **done**: the view generation referencing them is published.
//! - **rejected**: the artifact is absent, incompatible, changed during the
//!   import, or belongs to a plane the binder does not register. Its plane is
//!   built from the checkout instead; nothing is relabelled.
//!
//! Every transition is one `BEGIN IMMEDIATE` compare-and-set on the row's
//! state and owner token, so a crash at any boundary leaves a row that the
//! next attempt resumes by repeating at most one idempotent step.
//!
//! The legacy callgraph is never imported: its row goes straight to
//! `rejected` because the callgraph plane re-extracts every file.
//!
//! Imports run on background threads. A binder that finds another live owner
//! reports the plane as `migrating` and does not wait inside a bind.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::blob_store::v2::{
    to_hex, ContentHash, FamilyKey, FamilyPlane, PutOrTouch, StoreError, TrigramKey, TrigramPolicy,
};
use crate::db::lifecycle::{SqliteStore, TrackedConnection};
use crate::pins::{AssemblyPin, LivePin, PinError, PinOwner};
use crate::views::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
};
use crate::views::readiness::PlaneState;
use crate::views::registry::{FamilyRegistry, RegistryError, ViewRegistration};
use crate::views::segment_store::{
    self, SegmentError, SegmentMember, TrigramFlag, TrigramPayload, TrigramRecord,
};
use crate::views::semantic::SemanticProducer;
use crate::views::{RelPath, ViewError};

/// Format of the import ledger this build writes. A ledger whose format is
/// newer than this is reported as an error naming its version rather than
/// read with this build's schema.
pub const LEDGER_FORMAT_VERSION: i64 = 1;

/// How long a legacy set stays on disk after its import completed, so an
/// offline rollback to the previous binary still finds its caches.
pub const LEGACY_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Legacy files larger than this are not read into memory; their plane is
/// built from the checkout instead.
const MAX_LEGACY_SOURCE_BYTES: u64 = 1 << 30;
/// Upper bound on files listed by one legacy artifact.
const MAX_LEGACY_FILES: usize = 2_000_000;
/// Semantic sources larger than this are never chunked by the semantic plane
/// either, so the importer does not read them.
const MAX_SEMANTIC_SOURCE_BYTES: u64 = 4 * 1024 * 1024;

const LEDGER_FILE: &str = "imports.sqlite";
const STAGED_DIR: &str = "staged";
const STAGED_MAGIC: &[u8; 8] = b"AFTIMP01";

// Layout constants of the legacy trigram `cache.bin`, as written by the
// search index of the previous release (`search_index::write_cache_file_from_sources`).
const LEGACY_CACHE_MAGIC: u32 = 0x3144_4958;
const LEGACY_INDEX_MAGIC: &[u8; 8] = b"AFTIDX01";
const LEGACY_LOOKUP_MAGIC: &[u8; 8] = b"AFTLKP01";
const LEGACY_POSTING_BYTES: usize = 6;
const LEGACY_LOOKUP_ENTRY_BYTES: usize = 16;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ledger_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS imports (
    artifact TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    owner_pid INTEGER,
    owner_start INTEGER,
    owner_token TEXT,
    source_hash BLOB,
    source_len INTEGER,
    staged_hash BLOB,
    result TEXT,
    reason TEXT,
    updated_ms INTEGER NOT NULL,
    completed_ms INTEGER
);
";

/// One legacy artifact of a legacy set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Artifact {
    Trigram,
    Semantic,
    Callgraph,
}

impl Artifact {
    pub const ALL: [Self; 3] = [Self::Trigram, Self::Semantic, Self::Callgraph];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigram => "trigram",
            Self::Semantic => "semantic",
            Self::Callgraph => "callgraph",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|artifact| artifact.as_str() == value)
    }
}

/// The durable state of one import row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportState {
    Claimed,
    Staged,
    Validated,
    Registered,
    Done,
    Rejected,
}

impl ImportState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Staged => "staged",
            Self::Validated => "validated",
            Self::Registered => "registered",
            Self::Done => "done",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::Claimed,
            Self::Staged,
            Self::Validated,
            Self::Registered,
            Self::Done,
            Self::Rejected,
        ]
        .into_iter()
        .find(|state| state.as_str() == value)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Rejected)
    }
}

/// The process holding a claim. The token tells apart two claims of the same
/// process, so a claim left behind by a failed attempt in a still-running
/// process is recognised as abandoned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimOwner {
    pub pid: u32,
    pub start_time: u64,
    pub token: String,
}

impl ClaimOwner {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let me = this_process();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            pid: me.pid,
            start_time: me.start_time,
            token: format!("{}-{}-{}-{}", me.pid, me.start_time, now_ms(), seq),
        }
    }

    /// Whether this claim still has a running owner.
    pub fn is_live(&self) -> bool {
        let me = this_process();
        if self.pid == me.pid && self.start_time == me.start_time {
            return lock(active_claims()).contains(&self.token);
        }
        crate::pins::owner_is_live(&PinOwner {
            pid: self.pid,
            start_time: self.start_time,
        })
    }
}

/// This process's identity, computed once so every claim of the process
/// carries the same start time even where the OS cannot report one.
fn this_process() -> &'static PinOwner {
    static OWNER: OnceLock<PinOwner> = OnceLock::new();
    OWNER.get_or_init(crate::views::registry::current_owner)
}

fn active_claims() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Marks a claim token live for as long as the import that owns it runs.
struct ActiveClaim(String);

impl ActiveClaim {
    fn register(owner: &ClaimOwner) -> Self {
        lock(active_claims()).insert(owner.token.clone());
        Self(owner.token.clone())
    }
}

impl Drop for ActiveClaim {
    fn drop(&mut self) {
        lock(active_claims()).remove(&self.0);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// One row of the import ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportRow {
    pub artifact: Artifact,
    pub state: ImportState,
    pub attempt: u64,
    pub owner: Option<ClaimOwner>,
    pub source_hash: Option<[u8; 32]>,
    pub source_len: Option<u64>,
    pub staged_hash: Option<[u8; 32]>,
    /// For a registered or done trigram row, the imported segment id (hex).
    pub result: Option<String>,
    pub reason: Option<String>,
    pub updated_ms: u64,
    pub completed_ms: Option<u64>,
}

/// Whether a legacy key's import has finished, and when.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportStatus {
    pub rows: Vec<ImportRow>,
    /// True when every artifact has a terminal row.
    pub complete: bool,
    /// When the last row became terminal, once `complete`.
    pub completed_ms: Option<u64>,
}

#[derive(Debug)]
pub enum ImportError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Store(StoreError),
    Registry(RegistryError),
    Pin(PinError),
    View(ViewError),
    Segment(SegmentError),
    /// The ledger was written by a newer AFT.
    NewerLedger(i64),
    /// The legacy key cannot name a directory.
    InvalidKey(String),
    /// Another owner took the row over between two steps of this attempt.
    LostClaim(Artifact),
    Corrupt(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "legacy import I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "legacy import ledger error: {error}"),
            Self::Store(error) => write!(f, "legacy import store error: {error}"),
            Self::Registry(error) => write!(f, "legacy import registry error: {error}"),
            Self::Pin(error) => write!(f, "legacy import pin error: {error}"),
            Self::View(error) => write!(f, "legacy import view error: {error}"),
            Self::Segment(error) => write!(f, "legacy import segment error: {error}"),
            Self::NewerLedger(version) => write!(
                f,
                "legacy import ledger format {version} is newer than this build reads ({LEDGER_FORMAT_VERSION})"
            ),
            Self::InvalidKey(key) => write!(f, "invalid legacy artifact key `{key}`"),
            Self::LostClaim(artifact) => {
                write!(f, "legacy {} import claim was taken over", artifact.as_str())
            }
            Self::Corrupt(reason) => write!(f, "legacy import state is corrupt: {reason}"),
        }
    }
}

impl std::error::Error for ImportError {}

macro_rules! from_error {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        $(impl From<$ty> for ImportError {
            fn from(error: $ty) -> Self {
                Self::$variant(error)
            }
        })*
    };
}

from_error!(
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Store(StoreError),
    Registry(RegistryError),
    Pin(PinError),
    View(ViewError),
    Segment(SegmentError),
);

pub type ImportResult<T> = Result<T, ImportError>;

/// A legacy artifact key names one directory under each legacy root, so it
/// must be a single plain path component.
pub fn validate_legacy_key(key: &str) -> ImportResult<()> {
    let valid = !key.is_empty()
        && key.len() <= 255
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ImportError::InvalidKey(key.to_owned()))
    }
}

/// `<storage>/migration/v2/<key>`: the import ledger and staging files. Old
/// binaries know nothing under `migration/`, so none of their sweeps reach it.
pub fn ledger_dir(storage: &Path, legacy_key: &str) -> PathBuf {
    storage.join("migration").join("v2").join(legacy_key)
}

pub fn legacy_trigram_path(storage: &Path, legacy_key: &str) -> PathBuf {
    storage.join("index").join(legacy_key).join("cache.bin")
}

pub fn legacy_semantic_path(storage: &Path, legacy_key: &str) -> PathBuf {
    storage
        .join("semantic")
        .join(legacy_key)
        .join("semantic.bin")
}

pub fn legacy_callgraph_dir(storage: &Path, legacy_key: &str) -> PathBuf {
    storage.join("callgraph").join(legacy_key)
}

// ---------------------------------------------------------------------------
// Ledger

struct LedgerInner {
    dir: PathBuf,
    connection: Mutex<TrackedConnection>,
}

/// The import ledger of one legacy key. Every handle of a process shares one
/// SQLite connection: a second descriptor on the same database would drop the
/// process's POSIX locks on it when closed.
#[derive(Clone)]
pub struct ImportLedger {
    inner: Arc<LedgerInner>,
}

impl fmt::Debug for ImportLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportLedger")
            .field("dir", &self.inner.dir)
            .finish()
    }
}

fn open_ledgers() -> &'static Mutex<HashMap<PathBuf, Weak<LedgerInner>>> {
    static LEDGERS: OnceLock<Mutex<HashMap<PathBuf, Weak<LedgerInner>>>> = OnceLock::new();
    LEDGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

enum Claim {
    Owned(ImportRow),
    Terminal,
    Busy(ClaimOwner),
}

impl ImportLedger {
    /// Opens the ledger, creating it when absent.
    pub fn open(storage: &Path, legacy_key: &str) -> ImportResult<Self> {
        validate_legacy_key(legacy_key)?;
        let dir = ledger_dir(storage, legacy_key);
        crate::private_storage::create_dir_all(&dir)?;
        Self::open_path(dir, true).map(|ledger| ledger.expect("created ledger"))
    }

    /// Opens an existing ledger without creating anything.
    pub fn open_existing(storage: &Path, legacy_key: &str) -> ImportResult<Option<Self>> {
        validate_legacy_key(legacy_key)?;
        Self::open_path(ledger_dir(storage, legacy_key), false)
    }

    fn open_path(dir: PathBuf, create: bool) -> ImportResult<Option<Self>> {
        let path = dir.join(LEDGER_FILE);
        let mut ledgers = lock(open_ledgers());
        ledgers.retain(|_, ledger| ledger.strong_count() > 0);
        if let Some(inner) = ledgers.get(&path).and_then(Weak::upgrade) {
            return Ok(Some(Self { inner }));
        }
        if !create && !path.is_file() {
            return Ok(None);
        }
        let store = SqliteStore::Unmapped("legacy_import_ledger");
        let mut connection = if create {
            TrackedConnection::open(&path, store)?
        } else {
            TrackedConnection::open_path_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                store,
            )?
        };
        connection.busy_timeout(Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS))?;
        if create {
            crate::blob_store::retry_while_busy(
                Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS),
                || connection.pragma_update(None, "journal_mode", "WAL"),
            )?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT OR IGNORE INTO ledger_meta (singleton, format_version) VALUES (1, ?1)",
                params![LEDGER_FORMAT_VERSION],
            )?;
            check_format(&tx)?;
            tx.commit()?;
        } else {
            check_format(&connection)?;
        }
        let inner = Arc::new(LedgerInner {
            dir,
            connection: Mutex::new(connection),
        });
        ledgers.insert(path, Arc::downgrade(&inner));
        Ok(Some(Self { inner }))
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Reads a ledger's status without writing a byte under the storage root:
    /// an ordinary open would recover and checkpoint a journal a killed
    /// importer left behind. A handle this process already holds is used as
    /// is, because a second descriptor on the same database would drop this
    /// process's locks on it when closed. Otherwise the database and its
    /// journal are copied to a private temporary directory and read there. A
    /// copy taken while another process writes can only be older or
    /// unreadable; rows only ever move towards completion, so neither makes
    /// an import look more complete than it is.
    pub fn read_status(storage: &Path, legacy_key: &str) -> ImportResult<Option<ImportStatus>> {
        validate_legacy_key(legacy_key)?;
        let path = ledger_dir(storage, legacy_key).join(LEDGER_FILE);
        if let Some(inner) = lock(open_ledgers()).get(&path).and_then(Weak::upgrade) {
            return Ok(Some(status_of(read_rows(&lock(&inner.connection))?)));
        }
        if !path.is_file() {
            return Ok(None);
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let copy_dir = std::env::temp_dir().join(format!(
            "aft-ledger-read-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let result = (|| {
            crate::private_storage::create_dir_all(&copy_dir)?;
            let copy = copy_dir.join(LEDGER_FILE);
            crate::private_storage::copy(&path, &copy)?;
            let journal = path.with_file_name(format!("{LEDGER_FILE}-wal"));
            match crate::private_storage::copy(
                &journal,
                copy.with_file_name(format!("{LEDGER_FILE}-wal")),
            ) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let connection =
                TrackedConnection::open(&copy, SqliteStore::Unmapped("legacy_import_ledger"))?;
            check_format(&connection)?;
            let rows = read_rows(&connection)?;
            drop(connection);
            Ok(Some(status_of(rows)))
        })();
        let _ = fs::remove_dir_all(&copy_dir);
        result
    }

    pub fn rows(&self) -> ImportResult<Vec<ImportRow>> {
        let connection = lock(&self.inner.connection);
        read_rows(&connection)
    }

    pub fn status(&self) -> ImportResult<ImportStatus> {
        Ok(status_of(self.rows()?))
    }

    fn row(&self, artifact: Artifact) -> ImportResult<Option<ImportRow>> {
        let connection = lock(&self.inner.connection);
        read_row(&connection, artifact)
    }

    fn claim(&self, artifact: Artifact, owner: &ClaimOwner) -> ImportResult<Claim> {
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_row(&tx, artifact)?;
        let now = now_ms();
        let claim = match current {
            None => {
                tx.execute(
                    "INSERT INTO imports (artifact, state, attempt, owner_pid, owner_start,
                         owner_token, updated_ms)
                     VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6)",
                    params![
                        artifact.as_str(),
                        ImportState::Claimed.as_str(),
                        owner.pid,
                        owner.start_time as i64,
                        owner.token,
                        now as i64
                    ],
                )?;
                Claim::Owned(read_row(&tx, artifact)?.expect("inserted row"))
            }
            Some(row) if row.state.is_terminal() => Claim::Terminal,
            Some(ImportRow {
                owner: Some(holder),
                ..
            }) if holder.is_live() => Claim::Busy(holder),
            Some(_) => {
                // The previous owner is gone: take the row over where it
                // stopped. Its state is durable, so resuming repeats at most
                // the one step that was in flight.
                tx.execute(
                    "UPDATE imports SET attempt = attempt + 1, owner_pid = ?2, owner_start = ?3,
                         owner_token = ?4, updated_ms = ?5
                     WHERE artifact = ?1",
                    params![
                        artifact.as_str(),
                        owner.pid,
                        owner.start_time as i64,
                        owner.token,
                        now as i64
                    ],
                )?;
                Claim::Owned(read_row(&tx, artifact)?.expect("claimed row"))
            }
        };
        tx.commit()?;
        Ok(claim)
    }

    /// Moves `artifact` from `from` to `to` if `owner` still holds it in
    /// `from`. A terminal state releases the owner and stamps completion.
    fn advance(
        &self,
        artifact: Artifact,
        owner: &ClaimOwner,
        from: ImportState,
        to: ImportState,
        update: Advance,
    ) -> ImportResult<ImportRow> {
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms() as i64;
        let completed = to.is_terminal().then_some(now);
        let changed = tx.execute(
            "UPDATE imports SET state = ?4, updated_ms = ?5, completed_ms = ?6,
                 source_hash = COALESCE(?7, source_hash),
                 source_len = COALESCE(?8, source_len),
                 staged_hash = COALESCE(?9, staged_hash),
                 result = COALESCE(?10, result),
                 reason = COALESCE(?11, reason),
                 owner_pid = CASE WHEN ?12 THEN NULL ELSE owner_pid END,
                 owner_start = CASE WHEN ?12 THEN NULL ELSE owner_start END,
                 owner_token = CASE WHEN ?12 THEN NULL ELSE owner_token END
             WHERE artifact = ?1 AND state = ?2 AND owner_token = ?3",
            params![
                artifact.as_str(),
                from.as_str(),
                owner.token,
                to.as_str(),
                now,
                completed,
                update.source_hash.as_ref().map(|hash| hash.to_vec()),
                update.source_len.map(|len| len as i64),
                update.staged_hash.as_ref().map(|hash| hash.to_vec()),
                update.result,
                update.reason,
                to.is_terminal(),
            ],
        )?;
        if changed != 1 {
            return Err(ImportError::LostClaim(artifact));
        }
        let row = read_row(&tx, artifact)?.expect("advanced row");
        tx.commit()?;
        Ok(row)
    }
}

#[derive(Default)]
struct Advance {
    source_hash: Option<[u8; 32]>,
    source_len: Option<u64>,
    staged_hash: Option<[u8; 32]>,
    result: Option<String>,
    reason: Option<String>,
}

impl Advance {
    fn reason(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }
}

fn check_format(connection: &rusqlite::Connection) -> ImportResult<()> {
    let version: Option<i64> = connection
        .query_row(
            "SELECT format_version FROM ledger_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version {
        Some(version) if version > LEDGER_FORMAT_VERSION => Err(ImportError::NewerLedger(version)),
        Some(_) => Ok(()),
        None => Err(ImportError::Corrupt("ledger has no format row".to_owned())),
    }
}

const ROW_COLUMNS: &str = "artifact, state, attempt, owner_pid, owner_start, owner_token,
    source_hash, source_len, staged_hash, result, reason, updated_ms, completed_ms";

fn read_row(
    connection: &rusqlite::Connection,
    artifact: Artifact,
) -> ImportResult<Option<ImportRow>> {
    connection
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM imports WHERE artifact = ?1"),
            params![artifact.as_str()],
            decode_row,
        )
        .optional()?
        .transpose()
}

fn read_rows(connection: &rusqlite::Connection) -> ImportResult<Vec<ImportRow>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM imports ORDER BY artifact"
    ))?;
    let rows = statement
        .query_map([], decode_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().collect()
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportResult<ImportRow>> {
    let artifact: String = row.get(0)?;
    let state: String = row.get(1)?;
    let hash = |index: usize| -> rusqlite::Result<Option<[u8; 32]>> {
        let bytes: Option<Vec<u8>> = row.get(index)?;
        Ok(bytes.and_then(|bytes| bytes.try_into().ok()))
    };
    let owner = match (
        row.get::<_, Option<i64>>(3)?,
        row.get::<_, Option<i64>>(4)?,
        row.get::<_, Option<String>>(5)?,
    ) {
        (Some(pid), Some(start), Some(token)) => Some(ClaimOwner {
            pid: pid as u32,
            start_time: start as u64,
            token,
        }),
        _ => None,
    };
    let (Some(artifact), Some(state)) = (Artifact::parse(&artifact), ImportState::parse(&state))
    else {
        return Ok(Err(ImportError::Corrupt(format!(
            "unknown import row {artifact}/{state}"
        ))));
    };
    Ok(Ok(ImportRow {
        artifact,
        state,
        attempt: row.get::<_, i64>(2)? as u64,
        owner,
        source_hash: hash(6)?,
        source_len: row.get::<_, Option<i64>>(7)?.map(|len| len as u64),
        staged_hash: hash(8)?,
        result: row.get(9)?,
        reason: row.get(10)?,
        updated_ms: row.get::<_, i64>(11)? as u64,
        completed_ms: row.get::<_, Option<i64>>(12)?.map(|ms| ms as u64),
    }))
}

fn status_of(rows: Vec<ImportRow>) -> ImportStatus {
    let terminal = rows
        .iter()
        .filter(|row| row.state.is_terminal())
        .map(|row| row.artifact)
        .collect::<BTreeSet<_>>();
    let complete = Artifact::ALL
        .iter()
        .all(|artifact| terminal.contains(artifact));
    let completed_ms = complete
        .then(|| rows.iter().filter_map(|row| row.completed_ms).max())
        .flatten();
    ImportStatus {
        rows,
        complete,
        completed_ms,
    }
}

// ---------------------------------------------------------------------------
// Staged bundles

/// One converted file: the manifest facts and the payload stored under `key`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BundleEntry {
    rel_path: RelPath,
    content: ContentHash,
    size: u64,
    key: [u8; 32],
    payload: Vec<u8>,
}

/// The staged, canonical conversion of one legacy artifact.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Bundle {
    entries: Vec<BundleEntry>,
}

impl Bundle {
    fn encode(&self, artifact: Artifact) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(STAGED_MAGIC);
        bytes.push(artifact as u8);
        bytes.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            let path = entry.rel_path.as_bytes();
            bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
            bytes.extend_from_slice(path);
            bytes.extend_from_slice(entry.content.as_bytes());
            bytes.extend_from_slice(&entry.size.to_le_bytes());
            bytes.extend_from_slice(&entry.key);
            bytes.extend_from_slice(&(entry.payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&entry.payload);
        }
        let footer = blake3::hash(&bytes);
        bytes.extend_from_slice(footer.as_bytes());
        bytes
    }

    fn decode(bytes: &[u8], artifact: Artifact) -> Result<Self, String> {
        let body_len = bytes
            .len()
            .checked_sub(32)
            .ok_or("staged bundle is truncated")?;
        let (body, footer) = bytes.split_at(body_len);
        if blake3::hash(body).as_bytes() != footer {
            return Err("staged bundle footer does not match its bytes".to_owned());
        }
        let mut reader = LeReader::new(body);
        if reader.take(8)? != STAGED_MAGIC || reader.u8()? != artifact as u8 {
            return Err("staged bundle has another format or artifact".to_owned());
        }
        let count = reader.u32()? as usize;
        if count > MAX_LEGACY_FILES {
            return Err(format!("staged bundle lists {count} files"));
        }
        let mut entries = Vec::with_capacity(count.min(body.len() / 80));
        for _ in 0..count {
            let path_len = reader.u32()? as usize;
            let rel_path =
                RelPath::new(reader.take(path_len)?.to_vec()).map_err(|error| error.to_string())?;
            let content = ContentHash::from_bytes(reader.array32()?);
            let size = reader.u64()?;
            let key = reader.array32()?;
            let payload_len = reader.u32()? as usize;
            let payload = reader.take(payload_len)?.to_vec();
            entries.push(BundleEntry {
                rel_path,
                content,
                size,
                key,
                payload,
            });
        }
        if !reader.is_exhausted() {
            return Err("staged bundle has trailing bytes".to_owned());
        }
        Ok(Self { entries })
    }
}

fn staged_path(ledger: &ImportLedger, artifact: Artifact, staged_hash: &[u8; 32]) -> PathBuf {
    ledger.dir().join(STAGED_DIR).join(format!(
        "{}-{}.bundle",
        artifact.as_str(),
        &to_hex(staged_hash)[..32]
    ))
}

/// Writes `bytes` to `path` through temp + fsync + rename, then syncs the
/// directory, so `staged` is only recorded for bytes that reached the disk.
fn write_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::private_storage::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp.{}.{}", std::process::id(), now_ms()));
    let result = (|| {
        let mut file = crate::private_storage::options()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        crate::fs_lock::rename_over(&temporary, path)?;
        crate::fs_lock::sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Little-endian cursor shared by the legacy trigram parser and the bundle
/// decoder.
struct LeReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> LeReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn at(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }

    fn is_exhausted(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("unexpected end of data")?;
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    fn array32(&mut self) -> Result<[u8; 32], String> {
        Ok(self.take(32)?.try_into().expect("32"))
    }
}

// ---------------------------------------------------------------------------
// Canonical conversion: trigram

/// Converts a legacy `cache.bin` into canonical per-file trigram payloads.
///
/// The legacy postings are the search index's own posting fold of each file,
/// so inverting them gives exactly the payload [`TrigramPayload::extract`]
/// computes from the same bytes. Anything that cannot be proven equal is a
/// rejection: another index or tokenizer version, another size limit, a
/// failed checksum, a path that is not relative, or a malformed table.
/// Files without a recorded content hash are left out; the first load's
/// strict reconciliation adds them from the checkout.
fn convert_trigram(bytes: &[u8], policy: &TrigramPolicy) -> Result<Bundle, String> {
    let mut reader = LeReader::new(bytes);
    if reader.u32()? != LEGACY_CACHE_MAGIC {
        return Err("not a trigram cache".to_owned());
    }
    let version = reader.u32()?;
    if version != crate::search_index::INDEX_FORMAT_VERSION {
        return Err(format!(
            "trigram cache version {version} differs from this build's {}",
            crate::search_index::INDEX_FORMAT_VERSION
        ));
    }
    let postings_len_total = reader.u64()? as usize;
    let postings_start = reader.offset;
    let postings_end = postings_start
        .checked_add(postings_len_total)
        .filter(|end| *end <= bytes.len() && postings_len_total >= 4)
        .ok_or("trigram cache postings section overruns the file")?;
    let crc_at = postings_end - 4;
    let stored = u32::from_le_bytes(bytes[crc_at..postings_end].try_into().expect("4"));
    if crc32fast::hash(&bytes[postings_start..crc_at]) != stored {
        return Err("trigram cache postings checksum mismatch".to_owned());
    }
    let section = &bytes[..crc_at];
    let mut reader = LeReader::at(section, postings_start);
    if reader.take(8)? != LEGACY_INDEX_MAGIC || reader.u32()? != version {
        return Err("trigram cache postings header mismatch".to_owned());
    }
    let head_len = reader.u32()? as usize;
    let root_len = reader.u32()? as usize;
    let ignore_len = reader.u32()? as usize;
    let max_file_size = reader.u64()?;
    if max_file_size != policy.max_file_size {
        return Err(format!(
            "trigram cache max_file_size {max_file_size} differs from the family policy {}",
            policy.max_file_size
        ));
    }
    let file_count = reader.u32()? as usize;
    if file_count > MAX_LEGACY_FILES {
        return Err(format!("trigram cache lists {file_count} files"));
    }
    reader.take(head_len)?;
    reader.take(root_len)?;
    reader.take(ignore_len)?;

    struct LegacyFile {
        rel_path: RelPath,
        size: u64,
        content: [u8; 32],
        unindexed: bool,
    }
    let mut files = Vec::with_capacity(file_count.min(section.len() / 57));
    let mut seen = HashSet::new();
    for _ in 0..file_count {
        let unindexed = match reader.u8()? {
            0 => false,
            1 => true,
            other => return Err(format!("trigram cache file flag {other}")),
        };
        let path_len = reader.u32()? as usize;
        let size = reader.u64()?;
        let _secs = reader.u64()?;
        let _nanos = reader.u32()?;
        let content = reader.array32()?;
        let path = std::str::from_utf8(reader.take(path_len)?)
            .map_err(|_| "trigram cache path is not UTF-8")?;
        let path = Path::new(path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "trigram cache path {} is not relative",
                path.display()
            ));
        }
        let rel_path = RelPath::from_os_path(path).map_err(|error| error.to_string())?;
        if !seen.insert(rel_path.clone()) {
            return Err("trigram cache lists a path twice".to_owned());
        }
        files.push(LegacyFile {
            rel_path,
            size,
            content,
            unindexed,
        });
    }
    let blob_len = reader.u64()? as usize;
    let blob = reader.take(blob_len)?;
    if blob_len % LEGACY_POSTING_BYTES != 0 {
        return Err("trigram cache posting blob has a partial record".to_owned());
    }

    let lookup = &bytes[postings_end..];
    let lookup_body_len = lookup
        .len()
        .checked_sub(4)
        .ok_or("trigram cache lookup section is truncated")?;
    let stored = u32::from_le_bytes(lookup[lookup_body_len..].try_into().expect("4"));
    if crc32fast::hash(&lookup[..lookup_body_len]) != stored {
        return Err("trigram cache lookup checksum mismatch".to_owned());
    }
    let mut reader = LeReader::new(&lookup[..lookup_body_len]);
    if reader.take(8)? != LEGACY_LOOKUP_MAGIC || reader.u32()? != version {
        return Err("trigram cache lookup header mismatch".to_owned());
    }
    let entry_count = reader.u32()? as usize;
    if entry_count
        .checked_mul(LEGACY_LOOKUP_ENTRY_BYTES)
        .is_none_or(|needed| needed > lookup_body_len)
    {
        return Err("trigram cache lookup table overruns its section".to_owned());
    }
    let mut records: Vec<Vec<TrigramRecord>> = (0..files.len()).map(|_| Vec::new()).collect();
    let mut previous = None;
    for _ in 0..entry_count {
        let trigram = reader.u32()?;
        let offset = reader.u64()? as usize;
        let count = reader.u32()? as usize;
        if previous.is_some_and(|previous| previous >= trigram) {
            return Err("trigram cache lookup is not strictly sorted".to_owned());
        }
        previous = Some(trigram);
        let end = count
            .checked_mul(LEGACY_POSTING_BYTES)
            .and_then(|len| offset.checked_add(len))
            .filter(|end| *end <= blob.len())
            .ok_or("trigram cache posting list overruns the blob")?;
        for posting in blob[offset..end].chunks_exact(LEGACY_POSTING_BYTES) {
            let file_id = u32::from_le_bytes(posting[..4].try_into().expect("4")) as usize;
            let file_records = records
                .get_mut(file_id)
                .ok_or("trigram cache posting names an unknown file")?;
            if file_records
                .last()
                .is_some_and(|last| last.trigram == trigram)
            {
                return Err("trigram cache lists one file twice under a trigram".to_owned());
            }
            file_records.push(TrigramRecord {
                trigram,
                next_mask: posting[4],
                loc_mask: posting[5],
            });
        }
    }
    if !reader.is_exhausted() {
        return Err("trigram cache lookup has trailing bytes".to_owned());
    }

    let mut entries = Vec::with_capacity(files.len());
    for (file, records) in files.into_iter().zip(records) {
        if file.content == [0; 32] {
            continue;
        }
        let flag = if !file.unindexed {
            TrigramFlag::Indexed
        } else if file.size > policy.max_file_size {
            TrigramFlag::UnindexedOversize
        } else {
            TrigramFlag::UnindexedBinary
        };
        if flag != TrigramFlag::Indexed && !records.is_empty() {
            return Err("trigram cache holds postings for an unindexed file".to_owned());
        }
        let content = ContentHash::from_bytes(file.content);
        let payload = TrigramPayload { flag, records }.encode();
        entries.push(BundleEntry {
            rel_path: file.rel_path,
            content,
            size: file.size,
            key: *TrigramKey {
                content,
                policy: *policy,
            }
            .family_key()
            .as_bytes(),
            payload,
        });
    }
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(Bundle { entries })
}

fn trigram_segment(
    bundle: &Bundle,
    policy: &TrigramPolicy,
) -> Result<segment_store::SegmentBytes, String> {
    let members = bundle
        .entries
        .iter()
        .map(|entry| {
            TrigramPayload::decode(&entry.payload)
                .map(|payload| {
                    (
                        SegmentMember {
                            rel_path: entry.rel_path.clone(),
                            content: entry.content,
                            size: entry.size,
                        },
                        payload,
                    )
                })
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    segment_store::assemble(policy, members).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Canonical conversion: semantic

/// The semantic chunk fields that decide a payload, with the kind as stored
/// in `semantic.bin` and in view payloads.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ChunkShape {
    name: String,
    qualified_name: String,
    kind: u8,
    start_line: u32,
    end_line: u32,
    exported: bool,
    snippet: String,
    embed_text: String,
}

/// The byte the semantic index stores for each symbol kind. A kind this
/// table does not know never matches a stored row, so a drift between the
/// two tables can only stop an import, never relabel a vector.
fn stored_kind(kind: &crate::symbols::SymbolKind) -> u8 {
    use crate::symbols::SymbolKind;
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Class => 1,
        SymbolKind::Method => 2,
        SymbolKind::Struct => 3,
        SymbolKind::Interface => 4,
        SymbolKind::Enum => 5,
        SymbolKind::TypeAlias => 6,
        SymbolKind::Variable => 7,
        SymbolKind::Heading => 8,
        SymbolKind::FileSummary => 9,
        SymbolKind::Kernel => 10,
    }
}

fn push_field(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

/// The view payload a cold fill would store for these rows, in chunk order:
/// the same encoding as `SemanticVectors::encode_view_payload`.
fn semantic_payload(producer: &SemanticProducer, rows: &[(ChunkShape, Vec<u8>)]) -> Vec<u8> {
    let mut payload = vec![1_u8];
    push_field(&mut payload, producer.chunker_version.as_bytes());
    push_field(&mut payload, producer.template_version.as_bytes());
    push_field(&mut payload, producer.model_fingerprint.as_bytes());
    payload.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (shape, vector) in rows {
        push_field(&mut payload, shape.name.as_bytes());
        push_field(&mut payload, shape.qualified_name.as_bytes());
        payload.push(shape.kind);
        payload.extend_from_slice(&shape.start_line.to_le_bytes());
        payload.extend_from_slice(&shape.end_line.to_le_bytes());
        payload.push(u8::from(shape.exported));
        push_field(&mut payload, shape.snippet.as_bytes());
        push_field(&mut payload, shape.embed_text.as_bytes());
        push_field(&mut payload, vector);
    }
    payload
}

/// Converts a legacy `semantic.bin` into view payloads under `producer`.
///
/// The snapshot must carry exactly the producer's model fingerprint and the
/// current chunking version. A file is imported only when its bytes in the
/// checkout still hash to the snapshot's hash and this release chunks those
/// bytes into exactly the snapshot's rows (names, lines, snippets and
/// embedded texts). Its vectors belong to the same producer and inputs as a
/// cold fill, though a model invocation need not reproduce their float bytes.
/// Every other file is left for the plane to embed.
fn convert_semantic(
    bytes: &[u8],
    root: &Path,
    producer: &SemanticProducer,
) -> Result<(Bundle, usize), String> {
    let parsed = super::parse_legacy_snapshot(bytes, &producer.model_fingerprint)?;
    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    let mut skipped = 0usize;
    for file in parsed.files {
        let Some(rel_path) = super::legacy_path_to_rel_path(&file.path, &root) else {
            skipped += 1;
            continue;
        };
        let Ok(relative) = segment_store::rel_path_to_os(&rel_path) else {
            skipped += 1;
            continue;
        };
        let absolute = root.join(&relative);
        let fits = fs::symlink_metadata(&absolute)
            .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_SEMANTIC_SOURCE_BYTES);
        let Some(source) = fits.then(|| fs::read(&absolute).ok()).flatten() else {
            skipped += 1;
            continue;
        };
        if file.content_hash == [0; 32] || blake3::hash(&source).as_bytes() != &file.content_hash {
            skipped += 1;
            continue;
        }
        let chunks = match crate::semantic_index::chunk_view_file(
            &root,
            &relative,
            &source,
            producer.caps,
        ) {
            Ok(Some(chunks)) => chunks,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let mut stored: BTreeMap<ChunkShape, Vec<Vec<u8>>> = BTreeMap::new();
        for entry in file.entries {
            stored
                .entry(ChunkShape {
                    name: entry.name,
                    qualified_name: entry.qualified_name.unwrap_or_default(),
                    kind: entry.kind,
                    start_line: entry.start_line,
                    end_line: entry.end_line,
                    exported: entry.exported,
                    snippet: entry.snippet,
                    embed_text: entry.embed_text,
                })
                .or_default()
                .push(entry.vector);
        }
        let chunk_count = chunks.len();
        let mut rows = Vec::with_capacity(chunk_count);
        for chunk in chunks {
            let shape = ChunkShape {
                name: chunk.name,
                qualified_name: chunk.qualified_name.unwrap_or_default(),
                kind: stored_kind(&chunk.kind),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                exported: chunk.exported,
                snippet: chunk.snippet,
                embed_text: chunk.embed_text,
            };
            let Some(vector) = stored.get_mut(&shape).and_then(Vec::pop) else {
                break;
            };
            rows.push((shape, vector));
        }
        let all_matched = rows.len() == chunk_count && stored.values().all(Vec::is_empty);
        if !all_matched {
            skipped += 1;
            continue;
        }
        entries.push(BundleEntry {
            content: ContentHash::from_bytes(file.content_hash),
            size: source.len() as u64,
            key: *producer.key(&source, &rel_path).as_bytes(),
            payload: semantic_payload(producer, &rows),
            rel_path,
        });
    }
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok((Bundle { entries }, skipped))
}

// ---------------------------------------------------------------------------
// The import

/// What one binder asks the import to produce.
#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub storage: PathBuf,
    /// The v2 family whose stores receive the imported payloads.
    pub family: String,
    /// The key of the legacy set (`index/<key>`, `semantic/<key>`, …). The
    /// previous release named it with the same artifact key as the family.
    pub legacy_key: String,
    /// The binding view.
    pub scope: String,
    pub root: PathBuf,
    /// The binder's manifest producers; the published generation carries
    /// exactly these, so the binder's first load accepts it as its seed.
    pub producers: Producers,
    /// The trigram policy, when the binder registers the trigram plane.
    pub trigram_policy: Option<TrigramPolicy>,
    /// The semantic producer, when the binder registers the semantic plane.
    pub semantic: Option<SemanticProducer>,
}

impl ImportRequest {
    fn trigram(&self) -> Option<TrigramPolicy> {
        self.trigram_policy
            .filter(|policy| policy.fingerprint_hex() == self.producers.trigram)
    }

    fn semantic(&self) -> Option<&SemanticProducer> {
        self.semantic
            .as_ref()
            .filter(|producer| self.producers.semantic.as_deref() == Some(producer.id().as_str()))
    }
}

/// A durable boundary of the import, reported after it is committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportStep {
    Claimed(Artifact),
    Staged(Artifact),
    Validated(Artifact),
    Registered(Artifact),
    Rejected(Artifact),
    /// The generation that references the imported payloads is published.
    Published,
    /// Every row is terminal.
    Done,
}

/// Observes durable boundaries. Tests park or kill the process here.
pub trait ImportObserver: Sync {
    fn reached(&self, step: ImportStep);
}

fn observe(observer: Option<&dyn ImportObserver>, steps: &mut Vec<ImportStep>, step: ImportStep) {
    steps.push(step);
    if let Some(observer) = observer {
        observer.reached(step);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportOutcome {
    /// This call finished the import, possibly resuming an earlier attempt.
    Completed,
    /// Every row was already terminal; nothing was done.
    AlreadyComplete,
    /// Another live owner holds a claim. The caller reports the planes as
    /// migrating and tries again later.
    Busy { owner_pid: u32 },
}

#[derive(Clone, Debug)]
pub struct ImportReport {
    pub outcome: ImportOutcome,
    /// The durable boundaries this call committed, in order.
    pub steps: Vec<ImportStep>,
    /// The generation this call published, if any.
    pub published: Option<String>,
    /// Files converted per plane by this call.
    pub trigram_files: usize,
    pub semantic_files: usize,
    pub rows: Vec<ImportRow>,
}

/// Runs, resumes or observes the import for `request`'s legacy key.
///
/// Call it from a background thread, never on a bind path: a first import
/// reads the legacy artifacts and the checkout files they list.
pub fn run_import(
    request: &ImportRequest,
    observer: Option<&dyn ImportObserver>,
) -> ImportResult<ImportReport> {
    let ledger = ImportLedger::open(&request.storage, &request.legacy_key)?;
    let mut report = ImportReport {
        outcome: ImportOutcome::AlreadyComplete,
        steps: Vec::new(),
        published: None,
        trigram_files: 0,
        semantic_files: 0,
        rows: Vec::new(),
    };
    if ledger.status()?.complete {
        report.rows = ledger.rows()?;
        return Ok(report);
    }
    let me = ClaimOwner::new();
    let _active = ActiveClaim::register(&me);
    let mut owned = Vec::new();
    for artifact in Artifact::ALL {
        match ledger.claim(artifact, &me)? {
            Claim::Owned(row) => {
                if row.attempt == 1 && row.state == ImportState::Claimed {
                    observe(observer, &mut report.steps, ImportStep::Claimed(artifact));
                }
                owned.push(row);
            }
            Claim::Terminal => {}
            Claim::Busy(holder) => {
                report.outcome = ImportOutcome::Busy {
                    owner_pid: holder.pid,
                };
                report.rows = ledger.rows()?;
                return Ok(report);
            }
        }
    }
    report.outcome = ImportOutcome::Completed;
    let registry = FamilyRegistry::open(&request.storage, &request.family)?;
    let registration = registry.register_view(&request.scope, &request.root)?;
    let mut run = Run {
        request,
        ledger: &ledger,
        owner: &me,
        registration: &registration,
        observer,
        report: &mut report,
        pins: Vec::new(),
        stored: BTreeSet::new(),
        rejected_semantic: BTreeSet::new(),
    };
    for row in owned {
        run.drive(row)?;
    }
    run.publish()?;
    report.rows = ledger.rows()?;
    if status_of(report.rows.clone()).complete {
        observe(observer, &mut report.steps, ImportStep::Done);
    }
    Ok(report)
}

struct Run<'a> {
    request: &'a ImportRequest,
    ledger: &'a ImportLedger,
    owner: &'a ClaimOwner,
    registration: &'a ViewRegistration,
    observer: Option<&'a dyn ImportObserver>,
    report: &'a mut ImportReport,
    /// Live pins that protect registered payloads until publication.
    pins: Vec<LivePin>,
    /// Artifacts whose payloads this run stored under one of `pins`.
    stored: BTreeSet<Artifact>,
    /// Keys the import could not use; their files remain pending rather than
    /// preventing unrelated files and planes from being published.
    rejected_semantic: BTreeSet<[u8; 32]>,
}

impl Run<'_> {
    fn observe(&mut self, step: ImportStep) {
        observe(self.observer, &mut self.report.steps, step);
    }

    fn drive(&mut self, mut row: ImportRow) -> ImportResult<()> {
        loop {
            let artifact = row.artifact;
            row = match row.state {
                ImportState::Claimed => self.stage(&row)?,
                ImportState::Staged => self.validate(&row)?,
                ImportState::Validated => self.register(&row)?,
                ImportState::Registered | ImportState::Done | ImportState::Rejected => {
                    return Ok(())
                }
            };
            let step = match row.state {
                ImportState::Staged => ImportStep::Staged(artifact),
                ImportState::Validated => ImportStep::Validated(artifact),
                ImportState::Registered => ImportStep::Registered(artifact),
                ImportState::Rejected => ImportStep::Rejected(artifact),
                ImportState::Claimed | ImportState::Done => continue,
            };
            self.observe(step);
        }
    }

    fn reject(&mut self, row: &ImportRow, reason: impl Into<String>) -> ImportResult<ImportRow> {
        let reason = reason.into();
        crate::slog_info!(
            "legacy import key={} artifact={} rejected: {}",
            self.request.legacy_key,
            row.artifact.as_str(),
            reason
        );
        let rejected = self.ledger.advance(
            row.artifact,
            self.owner,
            row.state,
            ImportState::Rejected,
            Advance::reason(reason),
        )?;
        if let Some(hash) = row.staged_hash {
            let _ = fs::remove_file(staged_path(self.ledger, row.artifact, &hash));
        }
        Ok(rejected)
    }

    fn source_path(&self, artifact: Artifact) -> Option<PathBuf> {
        match artifact {
            Artifact::Trigram => Some(legacy_trigram_path(
                &self.request.storage,
                &self.request.legacy_key,
            )),
            Artifact::Semantic => Some(legacy_semantic_path(
                &self.request.storage,
                &self.request.legacy_key,
            )),
            Artifact::Callgraph => None,
        }
    }

    /// Reads a legacy source whole, or says why it cannot be imported.
    fn read_source(&self, artifact: Artifact) -> ImportResult<Result<Vec<u8>, String>> {
        let Some(path) = self.source_path(artifact) else {
            return Ok(Err("no importable source".to_owned()));
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(Err("legacy artifact is not a file".to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Err("no legacy artifact".to_owned()))
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_LEGACY_SOURCE_BYTES {
            return Ok(Err(format!(
                "legacy artifact is {} bytes, above the import limit",
                metadata.len()
            )));
        }
        match fs::read(&path) {
            Ok(bytes) => Ok(Ok(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Err("no legacy artifact".to_owned()))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn stage(&mut self, row: &ImportRow) -> ImportResult<ImportRow> {
        let artifact = row.artifact;
        let converted = match artifact {
            Artifact::Callgraph => {
                return self.reject(
                    row,
                    "the callgraph is re-extracted from the checkout; legacy callgraph stores are never imported",
                )
            }
            Artifact::Trigram if self.request.trigram().is_none() => {
                return self.reject(row, "the binder does not register the trigram plane")
            }
            Artifact::Semantic if self.request.semantic().is_none() => {
                return self.reject(row, "the binder does not register the semantic plane")
            }
            _ => match self.read_source(artifact)? {
                Err(reason) => return self.reject(row, reason),
                Ok(source) => {
                    let source_hash = *blake3::hash(&source).as_bytes();
                    let source_len = source.len() as u64;
                    let bundle = match artifact {
                        Artifact::Trigram => {
                            convert_trigram(&source, &self.request.trigram().expect("checked"))
                        }
                        _ => convert_semantic(
                            &source,
                            &self.request.root,
                            self.request.semantic().expect("checked"),
                        )
                        .map(|(bundle, _)| bundle),
                    };
                    (bundle, source_hash, source_len)
                }
            },
        };
        let (bundle, source_hash, source_len) = converted;
        let bundle = match bundle {
            Ok(bundle) => bundle,
            Err(reason) => return self.reject(row, reason),
        };
        let encoded = bundle.encode(artifact);
        let staged_hash = *blake3::hash(&encoded).as_bytes();
        write_durable(&staged_path(self.ledger, artifact, &staged_hash), &encoded)?;
        self.ledger.advance(
            artifact,
            self.owner,
            ImportState::Claimed,
            ImportState::Staged,
            Advance {
                source_hash: Some(source_hash),
                source_len: Some(source_len),
                staged_hash: Some(staged_hash),
                ..Advance::default()
            },
        )
    }

    /// Reads back the staged bundle of `row`, verified against its recorded hash.
    fn staged_bundle(&self, row: &ImportRow) -> Result<Bundle, String> {
        let hash = row.staged_hash.ok_or("the row records no staged bundle")?;
        let bytes = fs::read(staged_path(self.ledger, row.artifact, &hash))
            .map_err(|error| format!("staged bundle unreadable: {error}"))?;
        if blake3::hash(&bytes).as_bytes() != &hash {
            return Err("staged bundle does not match its recorded hash".to_owned());
        }
        Bundle::decode(&bytes, row.artifact)
    }

    fn validate(&mut self, row: &ImportRow) -> ImportResult<ImportRow> {
        let bundle = match self.staged_bundle(row) {
            Ok(bundle) => bundle,
            Err(reason) => return self.reject(row, reason),
        };
        let checked = match row.artifact {
            Artifact::Trigram => match self.request.trigram() {
                None => Err("the binder no longer registers the trigram plane".to_owned()),
                Some(policy) => bundle
                    .entries
                    .iter()
                    .try_for_each(|entry| {
                        TrigramPayload::decode(&entry.payload)
                            .map_err(|error| error.to_string())?;
                        let key = TrigramKey {
                            content: entry.content,
                            policy,
                        }
                        .family_key();
                        (key.as_bytes() == &entry.key)
                            .then_some(())
                            .ok_or_else(|| "staged trigram key differs from its content".to_owned())
                    })
                    .and_then(|()| trigram_segment(&bundle, &policy).map(|_| ())),
            },
            Artifact::Semantic => match self.request.semantic() {
                None => Err("the binder no longer registers the semantic plane".to_owned()),
                Some(producer) => bundle.entries.iter().try_for_each(|entry| {
                    let relative = segment_store::rel_path_to_os(&entry.rel_path)
                        .map_err(|error| error.to_string())?;
                    crate::semantic_index::SemanticVectors::decode_view_payload(
                        &entry.payload,
                        &relative,
                        &crate::semantic_index::ViewPayloadProducer {
                            chunker_version: &producer.chunker_version,
                            template_version: &producer.template_version,
                            model_fingerprint: &producer.model_fingerprint,
                        },
                    )
                    .map(|_| ())
                }),
            },
            Artifact::Callgraph => Err("the callgraph is never imported".to_owned()),
        };
        if let Err(reason) = checked {
            return self.reject(row, reason);
        }
        // An older daemon may have rewritten its artifact since it was
        // converted; the conversion then describes bytes that no longer
        // exist, so it is dropped and the plane is built from the checkout.
        let unchanged = match self.read_source(row.artifact)? {
            Ok(source) => Some(*blake3::hash(&source).as_bytes()) == row.source_hash,
            Err(_) => false,
        };
        if !unchanged {
            return self.reject(row, "the legacy artifact changed during the import");
        }
        self.ledger.advance(
            row.artifact,
            self.owner,
            ImportState::Staged,
            ImportState::Validated,
            Advance::default(),
        )
    }

    /// Puts every payload of `bundle` into its family store under a live pin
    /// that protects it until publication; for trigram, also writes the
    /// segment. Every write is idempotent, so a resumed attempt repeats it.
    fn store_bundle(
        &mut self,
        artifact: Artifact,
        bundle: &Bundle,
    ) -> ImportResult<Option<String>> {
        let plane = match artifact {
            Artifact::Trigram => FamilyPlane::Trigram,
            _ => FamilyPlane::Semantic,
        };
        let keys = bundle
            .entries
            .iter()
            .map(|entry| FamilyKey::new(plane, entry.key))
            .collect::<Vec<_>>();
        let mut live = LivePin::create(self.registration)?;
        live.protect(&keys)?;
        let segment = match artifact {
            Artifact::Trigram => {
                let policy = self
                    .request
                    .trigram()
                    .ok_or_else(|| ImportError::Corrupt("trigram plane vanished".to_owned()))?;
                let segment = trigram_segment(bundle, &policy).map_err(ImportError::Corrupt)?;
                live.protect_segment(&segment.id)?;
                Some(segment)
            }
            _ => None,
        };
        let store = self.registration.open_store(plane)?;
        for (entry, key) in bundle.entries.iter().zip(&keys) {
            match store.put_or_touch(key, &entry.payload) {
                Ok(PutOrTouch::Inserted { .. } | PutOrTouch::Reused { .. }) => {}
                Err(StoreError::ConflictingPayload(_)) if artifact == Artifact::Semantic => {
                    // Semantic keys address the input and producer, not the
                    // model's output floats. Another invocation of the same
                    // model can differ in its low bits. Keep the first valid
                    // payload immutable instead of aborting the whole import.
                    let producer = self.request.semantic().ok_or_else(|| {
                        ImportError::Corrupt("semantic plane vanished".to_owned())
                    })?;
                    let relative = segment_store::rel_path_to_os(&entry.rel_path)?;
                    let usable = store.get(key)?.is_some_and(|payload| {
                        crate::semantic_index::SemanticVectors::decode_view_payload(
                            &payload,
                            &relative,
                            &crate::semantic_index::ViewPayloadProducer {
                                chunker_version: &producer.chunker_version,
                                template_version: &producer.template_version,
                                model_fingerprint: &producer.model_fingerprint,
                            },
                        )
                        .is_ok()
                    });
                    if usable {
                        store.touch(&[*key])?;
                        crate::slog_info!(
                            "legacy semantic import keeps existing payload key={key}"
                        );
                    } else {
                        self.rejected_semantic.insert(entry.key);
                        crate::slog_warn!("legacy semantic import skipped unusable key={key}");
                    }
                }
                Ok(PutOrTouch::Quarantined) if artifact == Artifact::Semantic => {
                    self.rejected_semantic.insert(entry.key);
                    crate::slog_warn!("legacy semantic import skipped quarantined key={key}");
                }
                Ok(PutOrTouch::Quarantined) => {
                    return Err(ImportError::Corrupt(format!(
                        "family store quarantined imported key {key}"
                    )))
                }
                Err(error) => return Err(error.into()),
            }
        }
        let segment_hex = match segment {
            Some(segment) => {
                segment_store::write_segment(&store, &self.request.storage, &segment, None)?;
                Some(to_hex(&segment.id))
            }
            None => None,
        };
        self.pins.push(live);
        self.stored.insert(artifact);
        Ok(segment_hex)
    }

    fn register(&mut self, row: &ImportRow) -> ImportResult<ImportRow> {
        let bundle = match self.staged_bundle(row) {
            Ok(bundle) => bundle,
            Err(reason) => return self.reject(row, reason),
        };
        let segment = self.store_bundle(row.artifact, &bundle)?;
        self.ledger.advance(
            row.artifact,
            self.owner,
            ImportState::Validated,
            ImportState::Registered,
            Advance {
                result: segment,
                ..Advance::default()
            },
        )
    }

    /// Publishes the generation that references every registered artifact,
    /// then marks them done. A view that already has a generation keeps it;
    /// the imported payloads are then reused by key while the store holds
    /// them.
    fn publish(&mut self) -> ImportResult<()> {
        let mut registered = Vec::new();
        for artifact in Artifact::ALL {
            let Some(row) = self.ledger.row(artifact)? else {
                continue;
            };
            if row.state != ImportState::Registered || row.owner.as_ref() != Some(self.owner) {
                continue;
            }
            match self.staged_bundle(&row) {
                Ok(bundle) => registered.push((row, bundle)),
                Err(reason) => {
                    // The bundle that was registered is gone; its payloads may
                    // be in the store, but without it no manifest can name
                    // them, so the plane is built from the checkout.
                    self.reject(&row, reason)?;
                }
            }
        }
        if registered.is_empty() {
            return Ok(());
        }
        // An artifact registered by an earlier, interrupted attempt has no
        // pin of this run: put its payloads again (a no-op for those still
        // stored) under this run's pins before naming them in a manifest.
        for (row, bundle) in &registered {
            if !self.stored.contains(&row.artifact) {
                self.store_bundle(row.artifact, bundle)?;
            }
        }
        let store = self.registration.view_store()?;
        let mut trigram_files = 0;
        let mut semantic_files = 0;
        if store.current_generation()?.is_none() {
            let manifest = self.manifest(&registered)?;
            trigram_files = registered
                .iter()
                .filter(|(row, _)| row.artifact == Artifact::Trigram)
                .map(|(_, bundle)| bundle.entries.len())
                .sum();
            semantic_files = registered
                .iter()
                .filter(|(row, _)| row.artifact == Artifact::Semantic)
                .map(|(_, bundle)| {
                    bundle
                        .entries
                        .iter()
                        .filter(|entry| !self.rejected_semantic.contains(&entry.key))
                        .count()
                })
                .sum();
            let name = GenerationName::for_manifest(&manifest)?;
            let keys = manifest.ready_keys().collect::<Vec<_>>();
            let _assembly = AssemblyPin::create_v2(self.registration, name.to_string(), &keys)?;
            let prepared = store.prepare_v2(&name, None, &manifest, None)?;
            match store.commit_v2(prepared, None)? {
                PublishV2::Published => {
                    self.registration
                        .registry()
                        .note_publish(self.registration.scope())?;
                    self.report.published = Some(name.to_string());
                    self.observe(ImportStep::Published);
                }
                PublishV2::EquivalentWinner { .. } | PublishV2::Conflict { .. } => {}
            }
        }
        self.report.trigram_files += trigram_files;
        self.report.semantic_files += semantic_files;
        for (row, _) in &registered {
            self.ledger.advance(
                row.artifact,
                self.owner,
                ImportState::Registered,
                ImportState::Done,
                Advance::default(),
            )?;
            if let Some(hash) = row.staged_hash {
                let _ = fs::remove_file(staged_path(self.ledger, row.artifact, &hash));
            }
        }
        self.pins.clear();
        self.stored.clear();
        Ok(())
    }

    fn manifest(&self, registered: &[(ImportRow, Bundle)]) -> ImportResult<ManifestV2> {
        let trigram = registered
            .iter()
            .find(|(row, _)| row.artifact == Artifact::Trigram)
            .map(|(row, bundle)| (row, bundle));
        let semantic = registered
            .iter()
            .find(|(row, _)| row.artifact == Artifact::Semantic)
            .map(|(_, bundle)| bundle);
        let trigram_registered = self.request.trigram().is_some();
        let semantic_registered = self.request.semantic().is_some();
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: self.request.producers.clone(),
            head_tree: None,
            ignore_fingerprint: None,
            segment: trigram.and_then(|(row, _)| row.result.clone()),
        });
        let mut entries: BTreeMap<RelPath, (ContentHash, u64, EntryPlanes)> = BTreeMap::new();
        if let Some((_, bundle)) = trigram {
            for entry in &bundle.entries {
                entries.insert(
                    entry.rel_path.clone(),
                    (
                        entry.content,
                        entry.size,
                        EntryPlanes {
                            trigram: Some(PlaneState::ready(&FamilyKey::new(
                                FamilyPlane::Trigram,
                                entry.key,
                            ))),
                            ..EntryPlanes::default()
                        },
                    ),
                );
            }
        }
        if let Some(bundle) = semantic {
            for entry in &bundle.entries {
                let ready = if self.rejected_semantic.contains(&entry.key) {
                    PlaneState::pending(
                        "the legacy semantic payload could not be imported for this key",
                    )
                } else {
                    PlaneState::ready(&FamilyKey::new(FamilyPlane::Semantic, entry.key))
                };
                match entries.get_mut(&entry.rel_path) {
                    Some((content, _, planes)) if *content == entry.content => {
                        planes.semantic = Some(ready);
                    }
                    // The two legacy artifacts describe different bytes; the
                    // entry keeps the trigram's and the semantic plane waits
                    // for the first load to reconcile the file.
                    Some(_) => {}
                    None => {
                        let planes = EntryPlanes {
                            trigram: trigram_registered.then(|| {
                                PlaneState::pending(
                                    "the legacy trigram index did not cover this file",
                                )
                            }),
                            semantic: Some(ready),
                            callgraph: None,
                        };
                        entries.insert(entry.rel_path.clone(), (entry.content, entry.size, planes));
                    }
                }
            }
        }
        for (rel_path, (content, size, mut planes)) in entries {
            if semantic_registered
                && planes.semantic.is_none()
                && crate::views::semantic::applies_to(&rel_path)
            {
                planes.semantic = Some(PlaneState::pending(
                    "the legacy semantic index did not cover this content",
                ));
            }
            manifest.insert(rel_path, EntryV2::regular(content, size, planes))?;
        }
        Ok(manifest)
    }
}

// ---------------------------------------------------------------------------
// Background driver

/// Runs the import, backing off while another live owner holds it, until it
/// is complete, `deadline` passes, or `keep_going` turns false. Meant for the
/// background thread that loads a checkout's view; while it runs the caller
/// reports its planes as migrating.
pub fn import_until_settled(
    request: &ImportRequest,
    deadline: Duration,
    poll: Duration,
    keep_going: &dyn Fn() -> bool,
) -> ImportResult<ImportReport> {
    let started = Instant::now();
    loop {
        let report = run_import(request, None)?;
        if !matches!(report.outcome, ImportOutcome::Busy { .. })
            || started.elapsed() >= deadline
            || !keep_going()
        {
            return Ok(report);
        }
        std::thread::sleep(poll);
    }
}

/// Whether `key` has any legacy artifact the import reads. Roots without one
/// never create a ledger.
pub fn has_importable_legacy_artifact(storage: &Path, legacy_key: &str) -> bool {
    validate_legacy_key(legacy_key).is_ok()
        && (legacy_trigram_path(storage, legacy_key).is_file()
            || legacy_semantic_path(storage, legacy_key).is_file())
}

/// The semantic producer a legacy snapshot was built with, under `config`.
///
/// The lane derives its producer from the loaded model's fingerprint:
/// the configured backend, model and text caps at the model's vector
/// dimension, plus, for a Synapse backend, the served vector space. The
/// model is not loaded here, so the producer is rebuilt from the snapshot's
/// header instead: its stored fingerprint when that agrees with `config` at
/// the snapshot's dimension (which carries any Synapse identity unchanged),
/// otherwise the configured one, which the import then rejects as
/// incompatible. Reads the header only.
pub(crate) fn legacy_semantic_producer(
    storage: &Path,
    legacy_key: &str,
    config: &crate::config::SemanticBackendConfig,
) -> Option<SemanticProducer> {
    use std::io::Read as _;
    const MAX_FINGERPRINT_BYTES: usize = 64 * 1024;
    let mut file = fs::File::open(legacy_semantic_path(storage, legacy_key)).ok()?;
    let mut header = [0_u8; 13];
    file.read_exact(&mut header).ok()?;
    let dimension = u32::from_le_bytes(header[1..5].try_into().expect("4")) as usize;
    let stored_len = u32::from_le_bytes(header[9..13].try_into().expect("4")) as usize;
    let configured =
        crate::semantic_index::SemanticIndexFingerprint::for_config_dimension(config, dimension);
    let mut stored = vec![0_u8; stored_len.min(MAX_FINGERPRINT_BYTES)];
    let stored = (stored_len <= MAX_FINGERPRINT_BYTES)
        .then(|| file.read_exact(&mut stored).ok().map(|()| stored))
        .flatten()
        .and_then(|bytes| {
            serde_json::from_slice::<crate::semantic_index::SemanticIndexFingerprint>(&bytes).ok()
        })
        .filter(|stored| {
            stored.backend == configured.backend
                && stored.model == configured.model
                && stored.base_url == configured.base_url
                && stored.dimension == configured.dimension
                && stored.chunking_version == configured.chunking_version
                && stored.embed_text_caps == configured.embed_text_caps
        });
    let fingerprint = stored.unwrap_or(configured);
    Some(SemanticProducer::current(
        fingerprint.as_string(),
        fingerprint.embed_text_caps,
    ))
}

/// Imports a root's legacy set before its views-on semantic lane loads.
///
/// Runs on the lane's own worker thread, after the lane was started and
/// before it registers its view, so the lane's first load seeds from the
/// imported generation instead of embedding every file again. While it runs
/// the root's semantic status names the stage `migrating_legacy_index`. A
/// root with no legacy artifact, or whose import already finished, returns
/// at once. Another process importing the same set is waited for with a
/// bound; past it the lane loads without the import.
pub(crate) fn import_for_semantic_lane(
    config: &crate::views::semantic_runtime::WorkerConfig,
    keep_going: &dyn Fn() -> bool,
) {
    const SETTLE_DEADLINE: Duration = Duration::from_secs(10 * 60);
    const SETTLE_POLL: Duration = Duration::from_secs(1);
    let (storage, key) = (&config.storage, &config.family);
    if !has_importable_legacy_artifact(storage, key) {
        return;
    }
    if let Ok(Some(ledger)) = ImportLedger::open_existing(storage, key) {
        if ledger.status().is_ok_and(|status| status.complete) {
            return;
        }
    }
    if !keep_going() {
        return;
    }
    *config
        .status
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        crate::context::SemanticIndexStatus::Building {
            stage: "migrating_legacy_index".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
    let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
        &config.schedule.limiter,
        "legacy_import",
        &config.root,
        keep_going,
    ) else {
        return;
    };
    let semantic = legacy_semantic_producer(storage, key, &config.semantic);
    let unregistered = crate::views::semantic_runtime::UNREGISTERED_PRODUCER.to_owned();
    let request = ImportRequest {
        storage: storage.clone(),
        family: config.family.clone(),
        legacy_key: key.clone(),
        scope: config.scope.clone(),
        root: config.root.clone(),
        producers: Producers {
            trigram: unregistered.clone(),
            semantic: semantic.as_ref().map(SemanticProducer::id),
            callgraph: unregistered,
        },
        trigram_policy: None,
        semantic,
    };
    let started = Instant::now();
    match import_until_settled(&request, SETTLE_DEADLINE, SETTLE_POLL, keep_going) {
        Ok(report) => crate::slog_info!(
            "legacy import root={} key={} outcome={:?} semantic_files={} published={} elapsed_ms={}",
            config.root.display(),
            key,
            report.outcome,
            report.semantic_files,
            report.published.as_deref().unwrap_or("none"),
            started.elapsed().as_millis()
        ),
        Err(error) => crate::slog_warn!(
            "legacy import root={} key={} failed: {}",
            config.root.display(),
            key,
            error
        ),
    }
}

#[cfg(test)]
#[path = "per_checkout_tests.rs"]
mod tests;
