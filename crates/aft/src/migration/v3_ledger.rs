//! The v3 import ledger (`blobs/v3/<family>/imports.sqlite`) and the driver
//! that runs one legacy key's import through it.
//!
//! One row per legacy key, artifact, producer and destination plane, so an
//! earlier completion of one artifact or producer never stands for another:
//! a semantic-only completion leaves the trigram and symbol rows to run, and a
//! changed producer starts a new row. Claims follow the v2 import ledger's
//! owner-token protocol (`per_checkout::ClaimOwner`): a live owner makes other
//! callers back off, a dead or released owner's row is taken over with
//! `attempt + 1` and resumes from its durable state.
//!
//! The file is created, and every row transition written, inside a
//! `v3::publish` call, so it exists only after the v3 reader floor is raised
//! and each write holds the storage root's reservation lock. Readers that
//! must not write under the storage root copy it first (see [`read_rows`]).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::per_checkout::{now_ms, validate_legacy_key, ActiveClaim, ClaimOwner, ImportState};
use super::v3_import::{
    check_staged, register_staged, source_identity, stage, staged_path, staging_dir,
    LegacyArtifact, ReadStats, RegisterOutcome, Registered, StageInput, V3ImportError, V3Result,
    DEFAULT_WORKING_SET, LEDGER_APPLICATION_ID, LEDGER_FORMAT_VERSION,
};
use crate::blob_store::v2::{FamilyPlane, TrigramPolicy};
use crate::blob_store::v3;
use crate::db::lifecycle::{SqliteStore, TrackedConnection};
use crate::views::semantic::SemanticProducer;

const LEDGER_FILE: &str = "imports.sqlite";
const LEDGER_SCHEMA_VERSION: i64 = 1;
/// Each ledger write is one v3 publication; this covers its WAL frames and
/// the registry transaction, independent of the import's size.
const LEDGER_RESERVATION: u64 = 64 * 1024;
const STORE: SqliteStore = SqliteStore::Unmapped("v3_import_ledger");

const SCHEMA: &str = "CREATE TABLE format_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, schema_text TEXT NOT NULL);
CREATE TABLE imports (legacy_key TEXT NOT NULL, artifact TEXT NOT NULL,
 producer TEXT NOT NULL, plane TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('claimed','staged','validated','registered','done','rejected')),
 attempt INTEGER NOT NULL CHECK(attempt>=1), owner_pid INTEGER, owner_start INTEGER, owner_token TEXT,
 source_hash BLOB, source_len INTEGER, staged_hash BLOB,
 imported INTEGER NOT NULL DEFAULT 0, skipped INTEGER NOT NULL DEFAULT 0,
 reason TEXT, updated_ms INTEGER NOT NULL, completed_ms INTEGER,
 PRIMARY KEY(legacy_key, artifact, producer, plane));
";

/// Identifies one ledger row.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RowKey {
    pub legacy_key: String,
    pub artifact: LegacyArtifact,
    /// The trigram policy fingerprint, semantic producer id or symbol
    /// producer id the row's payloads were converted for.
    pub producer: String,
    pub plane: FamilyPlane,
}

impl RowKey {
    fn describe(&self) -> String {
        format!(
            "{}/{}/{}",
            self.legacy_key,
            self.artifact.as_str(),
            self.plane.as_str()
        )
    }
}

/// One row of the v3 import ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V3ImportRow {
    pub key: RowKey,
    pub state: ImportState,
    pub attempt: u64,
    pub owner: Option<ClaimOwner>,
    pub source_hash: Option<[u8; 32]>,
    pub source_len: Option<u64>,
    pub staged_hash: Option<[u8; 32]>,
    /// Rows put into (or found in) the plane.
    pub imported: u64,
    /// Files of the artifact left for the plane to build.
    pub skipped: u64,
    /// Why the row was rejected, or the storage gap that stopped it.
    pub reason: Option<String>,
    pub updated_ms: u64,
    pub completed_ms: Option<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type SharedConnection = Arc<Mutex<TrackedConnection>>;

/// Every handle of a process shares one connection per ledger: a second
/// descriptor on the same database would drop the process's POSIX locks on
/// it when closed.
fn open_connections() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<TrackedConnection>>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<TrackedConnection>>>>> =
        OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn check_format(connection: &rusqlite::Connection) -> V3Result<()> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != LEDGER_APPLICATION_ID {
        return Err(V3ImportError::Corrupt(format!(
            "import ledger application_id {application_id:#x}"
        )));
    }
    let meta: Option<(i64, i64, String)> = connection
        .query_row(
            "SELECT format_version, schema_version, schema_text FROM format_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match meta {
        Some((format, _, _)) if format > LEDGER_FORMAT_VERSION => {
            Err(V3ImportError::NewerLedger(format))
        }
        Some((_, schema, text)) if schema == LEDGER_SCHEMA_VERSION && text == SCHEMA => Ok(()),
        Some(_) => Err(V3ImportError::Corrupt(
            "import ledger schema differs from this build's".to_owned(),
        )),
        None => Err(V3ImportError::Corrupt(
            "import ledger has no format row".to_owned(),
        )),
    }
}

/// The ledger of one v3 family.
#[derive(Clone, Debug)]
pub struct V3Ledger {
    storage: PathBuf,
    family: String,
    path: PathBuf,
}

enum Claim {
    Owned(V3ImportRow),
    Terminal(V3ImportRow),
    Busy(ClaimOwner),
}

#[derive(Default)]
struct Advance {
    source_hash: Option<[u8; 32]>,
    source_len: Option<u64>,
    staged_hash: Option<[u8; 32]>,
    imported: Option<u64>,
    skipped: Option<u64>,
    reason: Option<String>,
}

const ROW_COLUMNS: &str = "legacy_key, artifact, producer, plane, state, attempt, owner_pid,
    owner_start, owner_token, source_hash, source_len, staged_hash, imported, skipped, reason,
    updated_ms, completed_ms";

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<V3Result<V3ImportRow>> {
    let artifact: String = row.get(1)?;
    let plane: String = row.get(3)?;
    let state: String = row.get(4)?;
    let hash = |index: usize| -> rusqlite::Result<Option<[u8; 32]>> {
        let bytes: Option<Vec<u8>> = row.get(index)?;
        Ok(bytes.and_then(|bytes| bytes.try_into().ok()))
    };
    let owner = match (
        row.get::<_, Option<i64>>(6)?,
        row.get::<_, Option<i64>>(7)?,
        row.get::<_, Option<String>>(8)?,
    ) {
        (Some(pid), Some(start), Some(token)) => Some(ClaimOwner {
            pid: pid as u32,
            start_time: start as u64,
            token,
        }),
        _ => None,
    };
    let (Some(artifact), Some(plane), Some(state)) = (
        LegacyArtifact::parse(&artifact),
        FamilyPlane::V3_STORES
            .into_iter()
            .find(|candidate| candidate.as_str() == plane),
        ImportState::parse(&state),
    ) else {
        return Ok(Err(V3ImportError::Corrupt(format!(
            "unknown import row {artifact}/{plane}/{state}"
        ))));
    };
    Ok(Ok(V3ImportRow {
        key: RowKey {
            legacy_key: row.get(0)?,
            artifact,
            producer: row.get(2)?,
            plane,
        },
        state,
        attempt: row.get::<_, i64>(5)? as u64,
        owner,
        source_hash: hash(9)?,
        source_len: row.get::<_, Option<i64>>(10)?.map(|len| len as u64),
        staged_hash: hash(11)?,
        imported: row.get::<_, i64>(12)? as u64,
        skipped: row.get::<_, i64>(13)? as u64,
        reason: row.get(14)?,
        updated_ms: row.get::<_, i64>(15)? as u64,
        completed_ms: row.get::<_, Option<i64>>(16)?.map(|ms| ms as u64),
    }))
}

fn read_row(connection: &rusqlite::Connection, key: &RowKey) -> V3Result<Option<V3ImportRow>> {
    connection
        .query_row(
            &format!(
                "SELECT {ROW_COLUMNS} FROM imports
                 WHERE legacy_key=?1 AND artifact=?2 AND producer=?3 AND plane=?4"
            ),
            params![
                key.legacy_key,
                key.artifact.as_str(),
                key.producer,
                key.plane.as_str()
            ],
            decode_row,
        )
        .optional()?
        .transpose()
}

fn read_all(connection: &rusqlite::Connection) -> V3Result<Vec<V3ImportRow>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM imports ORDER BY legacy_key, artifact, producer, plane"
    ))?;
    let rows = statement
        .query_map([], decode_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().collect()
}

impl V3Ledger {
    pub fn new(storage: &Path, family: &str) -> V3Result<Self> {
        let dir = v3::family_dir(storage, family)
            .map_err(|_| V3ImportError::InvalidKey(family.to_owned()))?;
        Ok(Self {
            storage: storage.to_path_buf(),
            family: family.to_owned(),
            path: dir.join(LEDGER_FILE),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The process's connection to the ledger. Without `create`, a missing
    /// ledger is `None`; with it, the caller must be inside a v3 publication.
    ///
    /// The `open_connections` lock is held from before the open until the
    /// connection is registered, the same lock [`read_rows`] holds across a
    /// copy of the ledger's files, so an open never overlaps such a copy.
    fn connection(&self, create: bool) -> V3Result<Option<SharedConnection>> {
        let mut open = lock(open_connections());
        open.retain(|_, connection| connection.strong_count() > 0);
        if let Some(shared) = open.get(&self.path).and_then(Weak::upgrade) {
            return Ok(Some(shared));
        }
        if !create && !self.path.is_file() {
            return Ok(None);
        }
        let exists = self.path.is_file();
        let mut connection = TrackedConnection::open(&self.path, STORE)?;
        connection.busy_timeout(std::time::Duration::from_millis(
            crate::blob_store::BUSY_TIMEOUT_MS,
        ))?;
        if !exists {
            // A fresh file: select WAL, then create the schema and identity
            // in one transaction, so a crash leaves either nothing usable to
            // misread or the whole schema.
            connection.pragma_update(None, "journal_mode", "WAL")?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let empty: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
            if empty == 0 {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "application_id", LEDGER_APPLICATION_ID)?;
                tx.execute(
                    "INSERT INTO format_meta (singleton, format_version, schema_version, schema_text)
                     VALUES (1, ?1, ?2, ?3)",
                    params![LEDGER_FORMAT_VERSION, LEDGER_SCHEMA_VERSION, SCHEMA],
                )?;
            }
            tx.commit()?;
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        check_format(&connection)?;
        let shared = Arc::new(Mutex::new(connection));
        open.insert(self.path.clone(), Arc::downgrade(&shared));
        Ok(Some(shared))
    }

    /// Runs `body` in one IMMEDIATE ledger transaction inside a v3
    /// publication: the floor is raised before the ledger can be created and
    /// the write holds the reservation lock.
    fn write<T>(
        &self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> V3Result<T>,
    ) -> V3Result<T> {
        let mut outcome = None;
        v3::publish(
            &self.storage,
            &self.family,
            LEDGER_RESERVATION,
            None,
            |_publication| {
                outcome = Some((|| {
                    let shared = self.connection(true)?.expect("created ledger");
                    let mut connection = lock(&shared);
                    let tx =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let result = body(&tx)?;
                    tx.commit()?;
                    Ok(result)
                })());
                Ok(())
            },
        )?;
        outcome.expect("publication ran its body")
    }

    /// Every row, read through this process's connection when it has one,
    /// otherwise from a private copy (see [`read_rows`]).
    pub fn rows(&self) -> V3Result<Vec<V3ImportRow>> {
        Ok(read_rows(&self.storage, &self.family)?.unwrap_or_default())
    }

    pub fn row(&self, key: &RowKey) -> V3Result<Option<V3ImportRow>> {
        Ok(self.rows()?.into_iter().find(|row| &row.key == key))
    }

    fn claim(&self, key: &RowKey, owner: &ClaimOwner) -> V3Result<Claim> {
        self.write(|tx| {
            let now = now_ms() as i64;
            Ok(match read_row(tx, key)? {
                None => {
                    tx.execute(
                        "INSERT INTO imports (legacy_key, artifact, producer, plane, state, attempt,
                             owner_pid, owner_start, owner_token, updated_ms)
                         VALUES (?1, ?2, ?3, ?4, 'claimed', 1, ?5, ?6, ?7, ?8)",
                        params![
                            key.legacy_key,
                            key.artifact.as_str(),
                            key.producer,
                            key.plane.as_str(),
                            owner.pid,
                            owner.start_time as i64,
                            owner.token,
                            now
                        ],
                    )?;
                    Claim::Owned(read_row(tx, key)?.expect("inserted row"))
                }
                Some(row) if row.state.is_terminal() => Claim::Terminal(row),
                Some(V3ImportRow {
                    owner: Some(holder),
                    ..
                }) if holder.is_live() => Claim::Busy(holder),
                Some(_) => {
                    // The previous owner is gone or released the row: resume
                    // where it stopped. Its state is durable, so resuming
                    // repeats at most the one step that was in flight.
                    tx.execute(
                        "UPDATE imports SET attempt = attempt + 1, owner_pid = ?5,
                             owner_start = ?6, owner_token = ?7, updated_ms = ?8
                         WHERE legacy_key=?1 AND artifact=?2 AND producer=?3 AND plane=?4",
                        params![
                            key.legacy_key,
                            key.artifact.as_str(),
                            key.producer,
                            key.plane.as_str(),
                            owner.pid,
                            owner.start_time as i64,
                            owner.token,
                            now
                        ],
                    )?;
                    Claim::Owned(read_row(tx, key)?.expect("claimed row"))
                }
            })
        })
    }

    /// Moves `key` from `from` to `to` if `owner` still holds it there. A
    /// terminal state releases the owner and stamps completion.
    fn advance(
        &self,
        key: &RowKey,
        owner: &ClaimOwner,
        from: ImportState,
        to: ImportState,
        update: Advance,
    ) -> V3Result<V3ImportRow> {
        self.write(|tx| {
            let now = now_ms() as i64;
            let changed = tx.execute(
                "UPDATE imports SET state = ?7, updated_ms = ?8, completed_ms = ?9,
                     source_hash = COALESCE(?10, source_hash),
                     source_len = COALESCE(?11, source_len),
                     staged_hash = COALESCE(?12, staged_hash),
                     imported = COALESCE(?13, imported),
                     skipped = COALESCE(?14, skipped),
                     reason = COALESCE(?15, reason),
                     owner_pid = CASE WHEN ?16 THEN NULL ELSE owner_pid END,
                     owner_start = CASE WHEN ?16 THEN NULL ELSE owner_start END,
                     owner_token = CASE WHEN ?16 THEN NULL ELSE owner_token END
                 WHERE legacy_key=?1 AND artifact=?2 AND producer=?3 AND plane=?4
                     AND state = ?5 AND owner_token = ?6",
                params![
                    key.legacy_key,
                    key.artifact.as_str(),
                    key.producer,
                    key.plane.as_str(),
                    from.as_str(),
                    owner.token,
                    to.as_str(),
                    now,
                    to.is_terminal().then_some(now),
                    update.source_hash.as_ref().map(|hash| hash.to_vec()),
                    update.source_len.map(|len| len as i64),
                    update.staged_hash.as_ref().map(|hash| hash.to_vec()),
                    update.imported.map(|count| count as i64),
                    update.skipped.map(|count| count as i64),
                    update.reason,
                    to.is_terminal(),
                ],
            )?;
            if changed != 1 {
                return Err(V3ImportError::LostClaim(key.describe()));
            }
            Ok(read_row(tx, key)?.expect("advanced row"))
        })
    }

    /// Releases `owner`'s claim without changing the row's state, recording
    /// why it stopped. The next run takes the row over and resumes it.
    fn release(&self, key: &RowKey, owner: &ClaimOwner, reason: &str) -> V3Result<V3ImportRow> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE imports SET owner_pid = NULL, owner_start = NULL, owner_token = NULL,
                     reason = ?6, updated_ms = ?7
                 WHERE legacy_key=?1 AND artifact=?2 AND producer=?3 AND plane=?4
                     AND owner_token = ?5",
                params![
                    key.legacy_key,
                    key.artifact.as_str(),
                    key.producer,
                    key.plane.as_str(),
                    owner.token,
                    reason,
                    now_ms() as i64
                ],
            )?;
            if changed != 1 {
                return Err(V3ImportError::LostClaim(key.describe()));
            }
            Ok(read_row(tx, key)?.expect("released row"))
        })
    }
}

/// Reads every row of a family's ledger without writing a byte under the
/// storage root. A connection this process already holds is used as is.
/// Otherwise the database and its WAL are copied to a private temporary
/// directory and read there: an ordinary open would recover and checkpoint
/// a WAL a killed importer left behind. A copy taken while another process
/// writes can only be older or unreadable, and rows only move towards
/// completion, so neither makes an import look more complete than it is.
///
/// The copy opens and closes descriptors on the ledger's files, and closing
/// any descriptor on a SQLite file drops every POSIX lock this process holds
/// on it. So the lookup and the whole copy run under the `open_connections`
/// lock, which every ledger open in this process takes before it opens and
/// keeps until it has registered its connection: an open and a copy can
/// never overlap, and a copy only runs while this process has no connection
/// to the ledger.
pub fn read_rows(storage: &Path, family: &str) -> V3Result<Option<Vec<V3ImportRow>>> {
    let ledger = V3Ledger::new(storage, family)?;
    let mut open = lock(open_connections());
    open.retain(|_, connection| connection.strong_count() > 0);
    if let Some(shared) = open.get(&ledger.path).and_then(Weak::upgrade) {
        drop(open);
        return read_all(&lock(&shared)).map(Some);
    }
    if !ledger.path.is_file() {
        return Ok(None);
    }
    let rows = read_sqlite_copy(&ledger.path, |connection| {
        check_format(connection)?;
        read_all(connection).map(Some)
    });
    drop(open);
    rows
}

#[cfg(test)]
thread_local! {
    /// Test hook: called on the copying thread at named points of
    /// [`read_sqlite_copy`], so a test can hold a copy part-way through.
    static COPY_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&'static str)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn copy_hook(stage: &'static str) {
    COPY_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(stage);
        }
    });
}

/// Runs `body` on a private copy of the SQLite database at `path` (and its
/// WAL, if any), taken in a temporary directory outside the storage root.
/// Even a read-only open of a WAL database can create or recover sidecar
/// files beside it; reading a copy writes nothing under the storage root.
/// Copying opens and closes a descriptor on the file, which drops POSIX locks
/// this process holds on it, so callers use it only for databases this
/// process has no SQLite connection to and cannot open one during the copy
/// (as [`read_rows`] ensures for the ledger by holding `open_connections`,
/// and as holds for the `aft cache prune-legacy` command, which opens no v3
/// registry connection).
pub(crate) fn read_sqlite_copy<T>(
    path: &Path,
    body: impl FnOnce(&rusqlite::Connection) -> V3Result<T>,
) -> V3Result<T> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| V3ImportError::Corrupt(format!("{} names no file", path.display())))?;
    let copy_dir = std::env::temp_dir().join(format!(
        "aft-v3-sqlite-read-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        crate::private_storage::create_dir_all(&copy_dir)?;
        let copy = copy_dir.join(name);
        crate::private_storage::copy(path, &copy)?;
        #[cfg(test)]
        copy_hook("database-copied");
        let mut wal_name = name.to_os_string();
        wal_name.push("-wal");
        match crate::private_storage::copy(path.with_file_name(&wal_name), copy_dir.join(&wal_name))
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let connection = TrackedConnection::open(&copy, STORE)?;
        let result = body(&connection);
        drop(connection);
        result
    })();
    let _ = fs::remove_dir_all(&copy_dir);
    #[cfg(test)]
    copy_hook("finished");
    result
}

// ---------------------------------------------------------------------------
// Legacy-key status for retention and pruning

/// How far a legacy key's v3 import got, read without writing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LegacyImportStatus {
    pub rows: Vec<V3ImportRow>,
    /// Some row has a claim whose owner is running: that process may be
    /// reading the legacy artifact right now.
    pub live_claim: bool,
    /// Some row is not terminal yet.
    pub incomplete: bool,
    /// When the last row became terminal, once every row is.
    pub completed_ms: Option<u64>,
}

/// The v3 import status of `legacy_key`. The previous release named a legacy
/// set with the same key as its family, so the family ledger of that name is
/// the one that can hold the key's rows. `None` when no v3 import was ever
/// recorded for it.
pub fn legacy_key_status(storage: &Path, legacy_key: &str) -> V3Result<Option<LegacyImportStatus>> {
    if validate_legacy_key(legacy_key).is_err()
        || crate::blob_store::v2::validate_family(legacy_key).is_err()
    {
        return Ok(None);
    }
    let Some(rows) = read_rows(storage, legacy_key)? else {
        return Ok(None);
    };
    let rows = rows
        .into_iter()
        .filter(|row| row.key.legacy_key == legacy_key)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(None);
    }
    let live_claim = rows
        .iter()
        .any(|row| row.owner.as_ref().is_some_and(ClaimOwner::is_live));
    let incomplete = rows.iter().any(|row| !row.state.is_terminal());
    let completed_ms = (!incomplete)
        .then(|| rows.iter().filter_map(|row| row.completed_ms).max())
        .flatten();
    Ok(Some(LegacyImportStatus {
        rows,
        live_claim,
        incomplete,
        completed_ms,
    }))
}

// ---------------------------------------------------------------------------
// The driver

/// What one caller asks the v3 import to produce. The trigram policy is not
/// optional: every request imports the search postings, so no request can
/// complete only the semantic plane and leave the trigram plane unasked.
#[derive(Clone, Debug)]
pub struct V3ImportRequest {
    /// The storage root holding both the legacy artifacts and `blobs/v3`.
    pub storage: PathBuf,
    pub family: String,
    /// The key of the legacy set (`index/<key>`, `semantic/<key>`, …).
    pub legacy_key: String,
    /// The checkout the semantic import verifies its files against.
    pub root: PathBuf,
    pub trigram: TrigramPolicy,
    /// The semantic producer, when semantic indexing is configured.
    pub semantic: Option<SemanticProducer>,
    pub symbols: bool,
    /// Working-set budget of the streaming trigram inversion, in bytes.
    pub working_set: usize,
}

impl V3ImportRequest {
    /// The production request: every plane the family has.
    pub fn full_plane(
        storage: PathBuf,
        family: String,
        legacy_key: String,
        root: PathBuf,
        trigram: TrigramPolicy,
        semantic: Option<SemanticProducer>,
    ) -> Self {
        Self {
            storage,
            family,
            legacy_key,
            root,
            trigram,
            semantic,
            symbols: true,
            working_set: DEFAULT_WORKING_SET,
        }
    }

    /// The ledger rows this request drives, one per artifact it imports.
    pub fn rows(&self) -> Vec<RowKey> {
        let mut rows = vec![self.row(LegacyArtifact::Trigram, self.trigram.fingerprint_hex())];
        if let Some(producer) = &self.semantic {
            rows.push(self.row(LegacyArtifact::Semantic, producer.id()));
        }
        if self.symbols {
            rows.push(self.row(
                LegacyArtifact::Symbols,
                crate::views::symbol_store::SymbolProducer::current().id(),
            ));
        }
        rows
    }

    fn row(&self, artifact: LegacyArtifact, producer: String) -> RowKey {
        RowKey {
            legacy_key: self.legacy_key.clone(),
            artifact,
            producer,
            plane: artifact.plane(),
        }
    }

    fn stage_input<'a>(&'a self, dir: &'a Path) -> StageInput<'a> {
        StageInput {
            storage: &self.storage,
            legacy_key: &self.legacy_key,
            root: &self.root,
            dir,
            trigram: &self.trigram,
            semantic: self.semantic.as_ref(),
            working_set: self.working_set,
        }
    }
}

/// A durable boundary of one artifact's import, reported after it commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum V3Step {
    Claimed,
    Staged,
    Validated,
    /// The `n`th registration batch is committed in the plane.
    BatchRegistered(u64),
    Registered,
    Done,
    Rejected,
    /// The claim was released after a storage gap.
    Released,
}

/// Observes durable boundaries. Tests park or kill the process here.
pub trait V3ImportObserver: Sync {
    fn reached(&self, artifact: LegacyArtifact, step: V3Step);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactOutcome {
    /// The legacy set has no such artifact; no row was written.
    Absent,
    /// The row was already terminal; nothing was done.
    AlreadyComplete(ImportState),
    /// This call finished the import, possibly resuming an earlier attempt.
    Done {
        registered: Registered,
        skipped: u64,
    },
    Rejected(String),
    /// Another live owner holds the row.
    Busy {
        owner_pid: u32,
    },
    /// A reservation was refused; the plane reports this named gap and
    /// serves what v3 already holds. A later run resumes the row.
    StorageGap(String),
}

#[derive(Clone, Debug, Default)]
pub struct V3ImportReport {
    pub outcomes: BTreeMap<LegacyArtifact, ArtifactOutcome>,
    /// Durable boundaries committed by this call, in order.
    pub steps: Vec<(LegacyArtifact, V3Step)>,
    /// Read statistics of the artifacts this call staged.
    pub stats: BTreeMap<LegacyArtifact, ReadStats>,
}

struct Run<'a> {
    request: &'a V3ImportRequest,
    ledger: V3Ledger,
    owner: ClaimOwner,
    dir: PathBuf,
    observer: Option<&'a dyn V3ImportObserver>,
    report: V3ImportReport,
}

impl Run<'_> {
    fn observe(&mut self, artifact: LegacyArtifact, step: V3Step) {
        self.report.steps.push((artifact, step));
        if let Some(observer) = self.observer {
            observer.reached(artifact, step);
        }
    }

    fn staged_file(&self, row: &V3ImportRow) -> Option<PathBuf> {
        row.staged_hash
            .map(|hash| staged_path(&self.dir, row.key.artifact, &hash))
    }

    fn reject(&mut self, row: &V3ImportRow, reason: String) -> V3Result<ArtifactOutcome> {
        crate::slog_info!(
            "v3 legacy import {} rejected: {}",
            row.key.describe(),
            reason
        );
        self.ledger.advance(
            &row.key,
            &self.owner,
            row.state,
            ImportState::Rejected,
            Advance {
                reason: Some(reason.clone()),
                ..Advance::default()
            },
        )?;
        if let Some(path) = self.staged_file(row) {
            let _ = fs::remove_file(path);
        }
        self.observe(row.key.artifact, V3Step::Rejected);
        Ok(ArtifactOutcome::Rejected(reason))
    }

    fn drive(&mut self, mut row: V3ImportRow) -> V3Result<ArtifactOutcome> {
        let artifact = row.key.artifact;
        let mut registered = Registered::default();
        loop {
            match row.state {
                ImportState::Claimed => {
                    let staged = stage(artifact, &self.request.stage_input(&self.dir))?;
                    let staged = match staged {
                        Ok(staged) => staged,
                        Err(reason) => return self.reject(&row, reason),
                    };
                    self.report.stats.insert(artifact, staged.stats);
                    row = self.ledger.advance(
                        &row.key,
                        &self.owner,
                        ImportState::Claimed,
                        ImportState::Staged,
                        Advance {
                            source_hash: Some(staged.source_hash),
                            source_len: Some(staged.source_len),
                            staged_hash: Some(staged.hash),
                            skipped: Some(staged.skipped),
                            ..Advance::default()
                        },
                    )?;
                    self.observe(artifact, V3Step::Staged);
                }
                ImportState::Staged => {
                    let Some(path) = self.staged_file(&row) else {
                        return self.reject(&row, "the row records no staged bundle".to_owned());
                    };
                    let hash = row.staged_hash.expect("staged file implies hash");
                    if let Err(reason) =
                        check_staged(artifact, &path, &hash, &self.request.stage_input(&self.dir))
                    {
                        return self.reject(&row, reason);
                    }
                    // An older daemon may have replaced its artifact since it
                    // was converted; the bundle then describes bytes that no
                    // longer exist and the plane is built from the checkout.
                    let current =
                        source_identity(artifact, &self.request.storage, &self.request.legacy_key);
                    if current.ok() != row.source_hash.zip(row.source_len) {
                        return self.reject(
                            &row,
                            "the legacy artifact changed during the import".to_owned(),
                        );
                    }
                    row = self.ledger.advance(
                        &row.key,
                        &self.owner,
                        ImportState::Staged,
                        ImportState::Validated,
                        Advance::default(),
                    )?;
                    self.observe(artifact, V3Step::Validated);
                }
                ImportState::Validated => {
                    let Some(path) = self.staged_file(&row) else {
                        return self.reject(&row, "the row records no staged bundle".to_owned());
                    };
                    let hash = row.staged_hash.expect("staged file implies hash");
                    let mut batches = Vec::new();
                    let outcome = register_staged(
                        &self.request.storage,
                        &self.request.family,
                        artifact,
                        &path,
                        &hash,
                        &mut |batch| {
                            batches.push(batch);
                            if let Some(observer) = self.observer {
                                observer.reached(artifact, V3Step::BatchRegistered(batch));
                            }
                        },
                    )?;
                    for batch in batches {
                        self.report
                            .steps
                            .push((artifact, V3Step::BatchRegistered(batch)));
                    }
                    match outcome {
                        Err(reason) => return self.reject(&row, reason),
                        Ok(RegisterOutcome::StorageGap(gap)) => {
                            self.ledger.release(&row.key, &self.owner, &gap)?;
                            self.observe(artifact, V3Step::Released);
                            return Ok(ArtifactOutcome::StorageGap(gap));
                        }
                        Ok(RegisterOutcome::Registered(done)) => {
                            registered = done.clone();
                            row = self.ledger.advance(
                                &row.key,
                                &self.owner,
                                ImportState::Validated,
                                ImportState::Registered,
                                Advance {
                                    imported: Some(
                                        done.inserted + done.reused + done.kept_existing,
                                    ),
                                    ..Advance::default()
                                },
                            )?;
                            self.observe(artifact, V3Step::Registered);
                        }
                    }
                }
                ImportState::Registered => {
                    if let Some(path) = self.staged_file(&row) {
                        match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    row = self.ledger.advance(
                        &row.key,
                        &self.owner,
                        ImportState::Registered,
                        ImportState::Done,
                        Advance::default(),
                    )?;
                    self.observe(artifact, V3Step::Done);
                }
                ImportState::Done => {
                    return Ok(ArtifactOutcome::Done {
                        registered,
                        skipped: row.skipped,
                    })
                }
                ImportState::Rejected => {
                    return Ok(ArtifactOutcome::Rejected(row.reason.unwrap_or_default()))
                }
            }
        }
    }
}

/// Runs, resumes or observes the v3 import of `request`'s legacy key.
///
/// Call it from a background thread, never on a bind path: a first import
/// reads every legacy artifact. Artifacts the legacy set lacks write no row,
/// so a key without legacy data never creates a ledger.
pub fn run_v3_import(
    request: &V3ImportRequest,
    observer: Option<&dyn V3ImportObserver>,
) -> V3Result<V3ImportReport> {
    validate_legacy_key(&request.legacy_key)
        .map_err(|_| V3ImportError::InvalidKey(request.legacy_key.clone()))?;
    let owner = ClaimOwner::new();
    let _active = ActiveClaim::register(&owner);
    let mut run = Run {
        request,
        ledger: V3Ledger::new(&request.storage, &request.family)?,
        owner,
        dir: staging_dir(&request.storage, &request.family, &request.legacy_key),
        observer,
        report: V3ImportReport::default(),
    };
    for key in request.rows() {
        let artifact = key.artifact;
        let existing = run.ledger.row(&key)?;
        if existing.is_none()
            && !artifact
                .source_path(&request.storage, &request.legacy_key)
                .is_file()
        {
            run.report
                .outcomes
                .insert(artifact, ArtifactOutcome::Absent);
            continue;
        }
        if let Some(row) = existing.filter(|row| row.state.is_terminal()) {
            run.report
                .outcomes
                .insert(artifact, ArtifactOutcome::AlreadyComplete(row.state));
            continue;
        }
        let outcome = match run.ledger.claim(&key, &run.owner)? {
            Claim::Terminal(row) => ArtifactOutcome::AlreadyComplete(row.state),
            Claim::Busy(holder) => ArtifactOutcome::Busy {
                owner_pid: holder.pid,
            },
            Claim::Owned(row) => {
                if row.attempt == 1 && row.state == ImportState::Claimed {
                    run.observe(artifact, V3Step::Claimed);
                }
                run.drive(row)?
            }
        };
        run.report.outcomes.insert(artifact, outcome);
    }
    Ok(run.report)
}

#[cfg(test)]
#[path = "v3_ledger_tests.rs"]
mod tests;

/// Keeps a test's claim live until it is dropped.
#[cfg(test)]
pub(crate) struct HeldClaim(#[allow(dead_code)] ActiveClaim);

/// Test seam: claims a row of `legacy_key` in `family`'s ledger for an owner
/// this process keeps live until the returned guard drops, as a running
/// import would.
#[cfg(test)]
pub(crate) fn hold_claim_for_test(storage: &Path, family: &str, legacy_key: &str) -> HeldClaim {
    let owner = ClaimOwner::new();
    let active = ActiveClaim::register(&owner);
    let key = RowKey {
        legacy_key: legacy_key.to_owned(),
        artifact: LegacyArtifact::Trigram,
        producer: "held-for-test".to_owned(),
        plane: FamilyPlane::Trigram,
    };
    let claimed = V3Ledger::new(storage, family)
        .unwrap()
        .claim(&key, &owner)
        .unwrap();
    assert!(matches!(claimed, Claim::Owned(_)));
    HeldClaim(active)
}
