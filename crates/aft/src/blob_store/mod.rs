//! Immutable, content-addressed payload storage shared by a repository family.
//!
//! Each [`BlobStore`] owns one SQLite connection for exactly one plane.  The
//! caller supplies the family (`artifact_key`); this module maps it to the
//! fixed on-disk layout and never derives a family from a checkout path.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};

use crate::db::lifecycle::{SqliteStore, TrackedConnection};

// Keep startup diagnostics in libtest output without changing production errors
// or adding timing work to production opens. SQLite may bypass a busy handler,
// so the failing statement and its elapsed time are needed to distinguish an
// immediate BUSY from an exhausted busy timeout.
#[cfg(test)]
macro_rules! trace_first_open_step {
    ($step:literal, $operation:expr) => {{
        let started = std::time::Instant::now();
        let result = $operation;
        if let Err(error) = &result {
            eprintln!(
                "blob-store first-open thread={:?} step={} elapsed={:?} busy_timeout_ms={}: {:?}",
                std::thread::current().id(),
                $step,
                started.elapsed(),
                BUSY_TIMEOUT_MS,
                error
            );
        }
        result
    }};
}

#[cfg(not(test))]
macro_rules! trace_first_open_step {
    ($step:literal, $operation:expr) => {
        $operation
    };
}

#[cfg(test)]
mod clone_accounting_tests;
pub mod reclaim;
pub mod v2;
pub mod v3;
#[doc(hidden)]
pub use v3::set_free_space_for_test;
pub use v3::{v3_format_report, V3FormatReport};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PayloadReadCounts {
    pub bytes: usize,
    pub blake3_calls: usize,
}

#[cfg(test)]
thread_local! {
    // Publication preparation and its blob readers run on the same thread.
    // Thread-local counters exclude reads from unrelated parallel tests.
    pub(crate) static PAYLOAD_READ_COUNTS: std::cell::Cell<PayloadReadCounts> = const {
        std::cell::Cell::new(PayloadReadCounts { bytes: 0, blake3_calls: 0 })
    };
    /// Membership queries [`BlobStore::missing_hex_keys`] ran on this thread,
    /// and how long they took in total.
    pub(crate) static MEMBERSHIP_PROBE_STATS: std::cell::Cell<ProbeStats> = const {
        std::cell::Cell::new(ProbeStats { queries: 0, nanos: 0 })
    };
}

/// Counts and total time of the membership queries one probe path ran.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProbeStats {
    pub queries: usize,
    pub nanos: u128,
}

#[cfg(test)]
impl ProbeStats {
    pub(crate) fn add(cell: &std::cell::Cell<Self>, queries: usize, started: std::time::Instant) {
        let current = cell.get();
        cell.set(Self {
            queries: current.queries + queries,
            nanos: current.nanos + started.elapsed().as_nanos(),
        });
    }
}

/// The SQLite busy wait used by every blob-store connection.
pub const BUSY_TIMEOUT_MS: u64 = 5_000;
/// SQLite's documented connection default.  The blob store reads this value
/// after opening instead of overriding it so future SQLite defaults are caught.
pub const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;

#[derive(Clone, Copy)]
struct BlobDurabilityState {
    dirty: bool,
}

static BLOB_DURABILITY: OnceLock<Mutex<HashMap<PathBuf, BlobDurabilityState>>> = OnceLock::new();
static BLOB_DURABILITY_BARRIER: Mutex<()> = Mutex::new(());

fn durability_states() -> &'static Mutex<HashMap<PathBuf, BlobDurabilityState>> {
    BLOB_DURABILITY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_blob_database(path: &Path) {
    durability_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path.to_path_buf())
        .or_insert(BlobDurabilityState { dirty: true });
}

fn mark_blob_database_dirty(path: &Path) {
    durability_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path.to_path_buf())
        .and_modify(|state| state.dirty = true)
        .or_insert(BlobDurabilityState { dirty: true });
}

pub(crate) fn blob_database_needs_durability(path: &Path) -> bool {
    let mut states = durability_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    states
        .entry(path.to_path_buf())
        .or_insert(BlobDurabilityState { dirty: true })
        .dirty
}

pub(crate) fn mark_blob_database_durable(path: &Path) {
    durability_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path.to_path_buf())
        .and_modify(|state| state.dirty = false)
        .or_insert(BlobDurabilityState { dirty: false });
}

pub(crate) fn publication_durability_barrier() -> MutexGuard<'static, ()> {
    BLOB_DURABILITY_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Payload encodings and the key producers that name them are pinned together.
/// A payload format change must update the corresponding producer string in the
/// same edit, because producer versions are components of content-address keys.
pub const SEMANTIC_PAYLOAD_SCHEMA: u32 = 1;
pub const SEMANTIC_PRODUCER_VERSION: &str = "semantic-v1";
pub const CALLGRAPH_PAYLOAD_SCHEMA: u32 = 1;
pub const CALLGRAPH_PRODUCER_VERSION: &str = "callgraph-v9-python-import-bindings";

const BLOB_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS blob_payloads (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32),
    payload BLOB NOT NULL,
    payload_digest BLOB NOT NULL CHECK(length(payload_digest) = 32),
    payload_schema INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
-- WITHOUT ROWID stores payload bytes in the primary-key tree. Membership probes
-- need a separate, narrow tree to avoid reading payload overflow pages.
CREATE INDEX IF NOT EXISTS blob_membership ON blob_payloads(full_key);
CREATE TABLE IF NOT EXISTS blob_quarantine (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32)
) WITHOUT ROWID;
-- A new identity distinguishes a recreated store from its old, empty counter.
CREATE TABLE IF NOT EXISTS blob_removals (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    identity BLOB NOT NULL,
    removed INTEGER NOT NULL
);
INSERT OR IGNORE INTO blob_removals VALUES(1, randomblob(16), 0);
CREATE TRIGGER IF NOT EXISTS count_blob_removals AFTER DELETE ON blob_payloads
BEGIN
    UPDATE blob_removals SET removed = removed + 1 WHERE singleton = 1;
END;
"#;

const CONTAINS_SQL: &str = "SELECT 1 FROM blob_payloads INDEXED BY blob_membership
                           WHERE full_key = ?1";

/// Keys asked about in one membership query by [`BlobStore::missing_hex_keys`];
/// well under SQLite's bound-parameter limit.
const MEMBERSHIP_BATCH: usize = 500;

/// What [`BlobStore::missing_hex_keys`] found.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeyPresence {
    /// Keys, spelled as the caller gave them, that have no payload row. A key
    /// that is not 64 hex digits can never have a row and is reported here too.
    pub missing: std::collections::BTreeSet<String>,
    /// Distinct keys checked before the check finished or ran out of time.
    pub checked: usize,
    /// Distinct keys the caller asked about.
    pub total: usize,
}

impl KeyPresence {
    /// False when the deadline stopped the check early: keys beyond `checked`
    /// are unknown, not present.
    pub fn complete(&self) -> bool {
        self.checked == self.total
    }
}

/// The two repository-family blob planes.  Trigram data is per-view derived
/// state and deliberately is not represented here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BlobPlane {
    Semantic,
    Callgraph,
}

impl BlobPlane {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Callgraph => "callgraph",
        }
    }

    const fn payload_schema(self) -> u32 {
        match self {
            Self::Semantic => SEMANTIC_PAYLOAD_SCHEMA,
            Self::Callgraph => CALLGRAPH_PAYLOAD_SCHEMA,
        }
    }
}

/// A complete, fixed-size database key.  Its bytes are the BLAKE3 digest of a
/// length-delimited canonical encoding of the semantic or callgraph key tuple.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct FullKey {
    bytes: [u8; 32],
    plane: BlobPlane,
}

impl FullKey {
    /// Returns the bytes stored in SQLite's `full_key` BLOB column.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// The only plane where this key can be stored.
    pub const fn plane(&self) -> BlobPlane {
        self.plane
    }

    /// Stable lower-case hexadecimal form suitable for logs and telemetry.
    pub fn to_hex(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.bytes {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

impl fmt::Display for FullKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A semantic blob identity.  `rel_path` remains a key component even when the
/// source bytes are identical, so path-specific embedding text cannot be reused
/// under a different path by accident.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SemanticKey {
    source_digest: [u8; 32],
    rel_path: Vec<u8>,
    chunker_version: String,
    embed_template_version: String,
    model_fingerprint: String,
    header_language: Option<&'static str>,
}

fn semantic_header_language(language: crate::parser::LangId) -> Option<&'static str> {
    match language {
        crate::parser::LangId::Cpp => Some("cpp"),
        crate::parser::LangId::ObjC => Some("objc"),
        _ => None,
    }
}

impl SemanticKey {
    /// Uses the same encoding as `from_bytes`; an unresolved header is a miss,
    /// since a source hash alone cannot select its parsing language.
    pub(crate) fn from_source_hash(
        source_digest: [u8; 32],
        rel_path: &[u8],
        chunker_version: impl Into<String>,
        embed_template_version: impl Into<String>,
        model_fingerprint: impl Into<String>,
    ) -> Option<Self> {
        let header_language = if rel_path.ends_with(b".h") {
            semantic_header_language(crate::parser::language_for_content_hash(
                std::path::Path::new("header.h"),
                blake3::Hash::from(source_digest),
            )?)
        } else {
            None
        };
        Some(Self {
            source_digest,
            rel_path: rel_path.to_vec(),
            chunker_version: chunker_version.into(),
            embed_template_version: embed_template_version.into(),
            model_fingerprint: model_fingerprint.into(),
            header_language,
        })
    }

    pub fn from_bytes(
        bytes: &[u8],
        rel_path: &[u8],
        chunker_version: impl Into<String>,
        embed_template_version: impl Into<String>,
        model_fingerprint: impl Into<String>,
    ) -> Self {
        let header_language = if rel_path.ends_with(b".h") {
            crate::parser::detect_language_from_source(
                std::path::Path::new("header.h"),
                std::str::from_utf8(bytes).unwrap_or(""),
            )
            .and_then(semantic_header_language)
        } else {
            None
        };
        Self {
            source_digest: *crate::views::io::source_hash(bytes).as_bytes(),
            rel_path: rel_path.to_vec(),
            chunker_version: chunker_version.into(),
            embed_template_version: embed_template_version.into(),
            model_fingerprint: model_fingerprint.into(),
            header_language,
        }
    }

    /// Builds a key using this release's semantic producer version for both
    /// producer components.  Callers that independently version the chunker
    /// and template should use [`Self::from_bytes`] instead.
    pub fn for_current(
        bytes: &[u8],
        rel_path: &[u8],
        model_fingerprint: impl Into<String>,
    ) -> Self {
        Self::from_bytes(
            bytes,
            rel_path,
            SEMANTIC_PRODUCER_VERSION,
            SEMANTIC_PRODUCER_VERSION,
            model_fingerprint,
        )
    }

    pub fn full_key(&self) -> FullKey {
        let fields: [&[u8]; 7] = [
            &self.source_digest,
            &self.rel_path,
            self.chunker_version.as_bytes(),
            self.embed_template_version.as_bytes(),
            self.model_fingerprint.as_bytes(),
            b"header-language",
            self.header_language.unwrap_or("").as_bytes(),
        ];
        // Preserve existing keys for non-headers and headers still parsed as C.
        // Only headers whose extracted symbols can differ get a language field.
        let field_count = if self.header_language.is_some() { 7 } else { 5 };
        full_key(
            BlobPlane::Semantic,
            b"aft/blob-store/semantic/v1",
            &fields[..field_count],
        )
    }

    /// A file-run format revision never changes its model or chunk-vector keys.
    pub(crate) fn file_key_for_summary_format(&self, format: u32) -> FullKey {
        let legacy = self.full_key();
        if format == 0 {
            return legacy;
        }
        full_key(
            BlobPlane::Semantic,
            b"aft/blob-store/semantic-summary-budget/v1",
            &[legacy.as_bytes(), &format.to_le_bytes()],
        )
    }

    pub fn source_digest(&self) -> &[u8; 32] {
        &self.source_digest
    }
}

/// A callgraph blob identity.  Unlike semantic keys, the relative path is not a
/// component: equal source bytes with the same language and extractor version
/// share one parse extraction across all paths in the family.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CallgraphKey {
    source_digest: [u8; 32],
    language: String,
    extractor_version: String,
}

impl CallgraphKey {
    pub fn from_bytes(
        bytes: &[u8],
        language: impl Into<String>,
        extractor_version: impl Into<String>,
    ) -> Self {
        Self::from_source_digest(
            *crate::views::io::source_hash(bytes).as_bytes(),
            language,
            extractor_version,
        )
    }

    /// Reconstruct a key from a proven Git-object alias without rereading source.
    pub(crate) fn from_source_digest(
        source_digest: [u8; 32],
        language: impl Into<String>,
        extractor_version: impl Into<String>,
    ) -> Self {
        Self {
            source_digest,
            language: language.into(),
            extractor_version: extractor_version.into(),
        }
    }

    /// Uses this release's extractor producer version.  `language = "config"`
    /// is intentionally accepted because configuration inputs are callgraph
    /// blobs too.
    pub fn for_current(bytes: &[u8], language: impl Into<String>) -> Self {
        Self::from_bytes(bytes, language, CALLGRAPH_PRODUCER_VERSION)
    }

    pub fn full_key(&self) -> FullKey {
        full_key(
            BlobPlane::Callgraph,
            b"aft/blob-store/callgraph/v1",
            &[
                &self.source_digest,
                self.language.as_bytes(),
                self.extractor_version.as_bytes(),
            ],
        )
    }

    pub fn source_digest(&self) -> &[u8; 32] {
        &self.source_digest
    }
}

fn full_key(plane: BlobPlane, domain: &[u8], fields: &[&[u8]]) -> FullKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    for field in fields {
        hasher.update(&(field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    FullKey {
        bytes: *hasher.finalize().as_bytes(),
        plane,
    }
}

/// The only public put outcomes.  A durable report is impossible for the last
/// three outcomes because [`PutReport::durable`] derives from this enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PutOutcome {
    Inserted,
    Reused,
    Quarantined,
    Failed,
    QuotaExceeded,
}

/// The result of a successful put attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PutReport {
    pub outcome: PutOutcome,
    pub durable: bool,
}

impl PutReport {
    fn new(outcome: PutOutcome) -> Self {
        Self {
            durable: matches!(outcome, PutOutcome::Inserted | PutOutcome::Reused),
            outcome,
        }
    }
}

/// The pragma values read back from the open connection after configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobStorePragmas {
    pub journal_mode: String,
    pub synchronous: i64,
    pub busy_timeout_ms: i64,
    pub foreign_keys: i64,
    pub wal_autocheckpoint_pages: i64,
}

/// Space and row usage for one immutable family/plane store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlobUsage {
    pub rows: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
}

/// A breaker implementation is notified exactly once after a corrupt database
/// has been moved aside and a clean replacement has opened successfully; the
/// notification records that recovery event.
pub trait BlobStoreBreaker {
    fn record_corruption_death(&self, artifact_key: &str, plane: BlobPlane);
}

#[derive(Debug)]
pub enum BlobStoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidArtifactKey(String),
    PragmaMismatch {
        name: &'static str,
        expected: String,
        actual: String,
    },
    PlaneKeyMismatch {
        store_plane: BlobPlane,
        key_plane: BlobPlane,
    },
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "blob-store I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "blob-store SQLite error: {error}"),
            Self::InvalidArtifactKey(key) => write!(f, "invalid artifact key `{key}`"),
            Self::PlaneKeyMismatch {
                store_plane,
                key_plane,
            } => write!(
                f,
                "a {} key cannot be stored in the {} plane",
                key_plane.as_str(),
                store_plane.as_str()
            ),
            Self::PragmaMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "blob-store PRAGMA {name} was `{actual}`, expected `{expected}`"
            ),
        }
    }
}

impl Error for BlobStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidArtifactKey(_)
            | Self::PragmaMismatch { .. }
            | Self::PlaneKeyMismatch { .. } => None,
        }
    }
}

impl From<std::io::Error> for BlobStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for BlobStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// One SQLite-backed immutable payload plane for one repository family.
#[derive(Debug)]
pub struct BlobStore {
    artifact_key: String,
    plane: BlobPlane,
    path: PathBuf,
    pragmas: BlobStorePragmas,
    connection: TrackedConnection,
}

impl BlobStore {
    /// Opens `<storage>/blobs/<artifact_key>/<plane>.sqlite` and creates its
    /// schema under `BEGIN IMMEDIATE`.  Existing corrupt SQLite headers are
    /// preserved beside the database before a clean empty store is created.
    pub fn open(
        storage: &Path,
        artifact_key: impl Into<String>,
        plane: BlobPlane,
    ) -> Result<Self, BlobStoreError> {
        Self::open_with_optional_breaker(storage, artifact_key.into(), plane, None)
    }

    /// Like [`Self::open`], but invokes the supplied breaker exactly once after
    /// corruption recovery has completed successfully.
    pub fn open_with_breaker(
        storage: &Path,
        artifact_key: impl Into<String>,
        plane: BlobPlane,
        breaker: &dyn BlobStoreBreaker,
    ) -> Result<Self, BlobStoreError> {
        Self::open_with_optional_breaker(storage, artifact_key.into(), plane, Some(breaker))
    }

    fn open_with_optional_breaker(
        storage: &Path,
        artifact_key: String,
        plane: BlobPlane,
        breaker: Option<&dyn BlobStoreBreaker>,
    ) -> Result<Self, BlobStoreError> {
        validate_artifact_key(&artifact_key)?;
        let path = storage
            .join("blobs")
            .join(&artifact_key)
            .join(format!("{}.sqlite", plane.as_str()));
        if let Some(parent) = path.parent() {
            crate::private_storage::open_dir(storage, parent)?;
        }

        match Self::open_at(&artifact_key, plane, path.clone()) {
            Ok(store) => Ok(store),
            Err(error) if is_corrupt_database_error(&error) && path.exists() => {
                let corrupt_path = move_corrupt_database_aside(&path)?;
                log::warn!(
                    "blob store database at {} was corrupt; moved it to {}",
                    path.display(),
                    corrupt_path.display()
                );
                let store = Self::open_at(&artifact_key, plane, path)?;
                if let Some(breaker) = breaker {
                    breaker.record_corruption_death(&artifact_key, plane);
                }
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    fn open_at(
        artifact_key: &str,
        plane: BlobPlane,
        path: PathBuf,
    ) -> Result<Self, BlobStoreError> {
        let mut connection = trace_first_open_step!(
            "TrackedConnection::open (before blob busy_timeout)",
            TrackedConnection::open(&path, SqliteStore::BlobStore)
        )?;
        configure_connection(&connection)?;
        ensure_schema(&mut connection)?;
        let pragmas = read_and_assert_pragmas(&connection)?;
        register_blob_database(&path);
        Ok(Self {
            artifact_key: artifact_key.to_owned(),
            plane,
            path,
            pragmas,
            connection,
        })
    }

    pub fn artifact_key(&self) -> &str {
        &self.artifact_key
    }

    pub const fn plane(&self) -> BlobPlane {
        self.plane
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read through the store's connection: opening another descriptor can
    /// release process-wide SQLite locks when that descriptor is closed.
    pub(crate) fn removal_stamp(&self) -> Result<(Vec<u8>, i64), BlobStoreError> {
        Ok(self.connection.query_row(
            "SELECT identity, removed FROM blob_removals WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    pub fn pragmas(&self) -> &BlobStorePragmas {
        &self.pragmas
    }

    /// Returns current payload rows plus the SQLite file allocation.  Quota
    /// admission uses this before an insert so a rejected payload never enters
    /// the immutable store.
    pub fn usage(&self) -> Result<BlobUsage, BlobStoreError> {
        crate::views::io::WORK
            .logical_aggregates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (rows, payload_bytes): (u64, u64) = self.connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let page_count: u64 = self
            .connection
            .pragma_query_value(None, "page_count", |row| row.get(0))?;
        let page_size: u64 = self
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))?;
        Ok(BlobUsage {
            rows,
            payload_bytes,
            file_bytes: page_count.saturating_mul(page_size),
        })
    }

    /// Inserts a payload once.  An existing row is never updated, even if its
    /// bytes fail a later integrity check; callers must use a new producer key.
    pub fn put(&mut self, full_key: &FullKey, payload: &[u8]) -> Result<PutReport, BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        let _durability = publication_durability_barrier();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let quarantined = tx
            .query_row(
                "SELECT 1 FROM blob_quarantine WHERE full_key = ?1",
                params![full_key.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if quarantined {
            tx.commit()?;
            return Ok(PutReport::new(PutOutcome::Quarantined));
        }

        let payload_digest = blake3::hash(payload);
        let inserted = tx.execute(
            "INSERT INTO blob_payloads
             (full_key, payload, payload_digest, payload_schema, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(full_key) DO NOTHING",
            params![
                full_key.as_bytes().as_slice(),
                payload,
                payload_digest.as_bytes().as_slice(),
                i64::from(self.plane.payload_schema()),
                unix_millis_now(),
            ],
        )?;
        tx.commit()?;
        if inserted == 1 {
            mark_blob_database_dirty(&self.path);
        }
        Ok(PutReport::new(if inserted == 1 {
            PutOutcome::Inserted
        } else {
            PutOutcome::Reused
        }))
    }

    /// Probes membership through the existing payload-free key index. Payload
    /// format changes require new producer keys; consumers still must use `get`
    /// (or the verified materialization reader) to check schema and digest before
    /// using bytes. A corrupt immutable row can be present but is not consumable.
    pub fn contains(&self, full_key: &FullKey) -> Result<bool, BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        Ok(self
            .connection
            .prepare_cached(CONTAINS_SQL)?
            .query_row(params![full_key.as_bytes().as_slice()], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Finds which of `keys` (hex full keys, as manifests spell them) have no
    /// payload row. It reads only the payload-free key index, in sorted batches
    /// of [`MEMBERSHIP_BATCH`] keys with one query each, never payload bytes.
    /// With a `deadline`, no new batch starts once it has passed, and the
    /// result says how many keys were checked so the caller can tell an
    /// incomplete check from a clean one.
    pub fn missing_hex_keys<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k str>,
        deadline: Option<std::time::Instant>,
    ) -> Result<KeyPresence, BlobStoreError> {
        // Sorted hex sorts like the key bytes, so batches follow the index.
        let keys = keys
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut presence = KeyPresence {
            total: keys.len(),
            ..KeyPresence::default()
        };
        for batch in keys.chunks(MEMBERSHIP_BATCH) {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                break;
            }
            let decoded = batch
                .iter()
                .filter_map(|key| v2::parse_hex32(key))
                .collect::<Vec<_>>();
            let mut present = std::collections::BTreeSet::new();
            if !decoded.is_empty() {
                #[cfg(test)]
                let started = std::time::Instant::now();
                let sql = format!(
                    "SELECT full_key FROM blob_payloads INDEXED BY blob_membership
                     WHERE full_key IN ({})",
                    vec!["?"; decoded.len()].join(",")
                );
                let mut statement = self.connection.prepare_cached(&sql)?;
                let rows = statement.query_map(
                    rusqlite::params_from_iter(decoded.iter().map(|key| key.as_slice())),
                    |row| row.get::<_, Vec<u8>>(0),
                )?;
                for key in rows {
                    present.insert(key?);
                }
                #[cfg(test)]
                MEMBERSHIP_PROBE_STATS.with(|stats| ProbeStats::add(stats, 1, started));
            }
            for key in batch {
                let stored = v2::parse_hex32(key).is_some_and(|bytes| present.contains(&bytes[..]));
                if !stored {
                    presence.missing.insert((*key).to_owned());
                }
            }
            presence.checked += batch.len();
        }
        Ok(presence)
    }

    /// Reads a payload only after verifying its stored digest and schema. A
    /// malformed row is indistinguishable from a miss to consumers, but remains
    /// on disk for quarantine/forensics rather than being rewritten in place.
    pub fn get(&self, full_key: &FullKey) -> Result<Option<Vec<u8>>, BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        Ok(read_verified_payload(
            &self.connection,
            full_key.as_bytes(),
            self.plane,
        )?)
    }

    /// Records a deterministic failure without modifying the immutable payload
    /// table.  Subsequent puts report `quarantined` until a changed key is used.
    pub fn quarantine(&mut self, full_key: &FullKey) -> Result<(), BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        let _durability = publication_durability_barrier();
        let inserted = self.connection.execute(
            "INSERT INTO blob_quarantine (full_key) VALUES (?1)
             ON CONFLICT(full_key) DO NOTHING",
            params![full_key.as_bytes().as_slice()],
        )?;
        if inserted == 1 {
            mark_blob_database_dirty(&self.path);
        }
        Ok(())
    }

    fn ensure_key_plane(&self, full_key: &FullKey) -> Result<(), BlobStoreError> {
        if full_key.plane() == self.plane {
            Ok(())
        } else {
            Err(BlobStoreError::PlaneKeyMismatch {
                store_plane: self.plane,
                key_plane: full_key.plane(),
            })
        }
    }
}

/// Shared by direct blob reads and the manifest joiner, which owns a read-only
/// connection rather than a BlobStore. Membership alone must never authorize
/// using payload bytes in a derived graph.
pub(crate) fn read_verified_payload(
    connection: &Connection,
    full_key: &[u8],
    plane: BlobPlane,
) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    let row = connection
        .query_row(
            "SELECT payload, payload_digest, payload_schema
         FROM blob_payloads WHERE full_key = ?1",
            params![full_key],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((payload, payload_digest, payload_schema)) = row else {
        return Ok(None);
    };
    #[cfg(test)]
    PAYLOAD_READ_COUNTS.with(|counts| {
        let old = counts.get();
        counts.set(PayloadReadCounts {
            bytes: old.bytes + payload.len(),
            blake3_calls: old.blake3_calls + 1,
        });
    });
    let digest_matches = payload_digest.as_slice() == blake3::hash(&payload).as_bytes();
    let schema_matches = payload_schema == i64::from(plane.payload_schema());
    if digest_matches && schema_matches {
        return Ok(Some(payload));
    }
    let reason = match (digest_matches, schema_matches) {
        (false, false) => "payload digest and schema mismatch",
        (false, true) => "payload digest mismatch",
        (true, false) => "payload schema mismatch",
        (true, true) => unreachable!("matching payload was returned above"),
    };
    log::warn!(
        "blob store rejected committed payload for key {:02x?} in {}: {}",
        full_key,
        plane.as_str(),
        reason
    );
    Ok(None)
}

fn validate_artifact_key(artifact_key: &str) -> Result<(), BlobStoreError> {
    if artifact_key.is_empty()
        || artifact_key == "."
        || artifact_key == ".."
        || artifact_key.contains(['/', '\\', '\0'])
    {
        return Err(BlobStoreError::InvalidArtifactKey(artifact_key.to_owned()));
    }
    Ok(())
}

fn configure_connection(connection: &TrackedConnection) -> Result<(), BlobStoreError> {
    // Set the wait policy before WAL attempts to acquire the journal lock so
    // concurrent first-open callers wait instead of failing immediately.
    trace_first_open_step!(
        "busy_timeout",
        connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
    )?;
    trace_first_open_step!(
        "PRAGMA foreign_keys=OFF",
        connection.pragma_update(None, "foreign_keys", "OFF")
    )?;
    // Pointer maps must precede the first table. Existing NONE stores stay
    // readable without an upgrade-time full rewrite; rebuilt files opt in.
    let pages: u64 = trace_first_open_step!(
        "PRAGMA page_count",
        connection.pragma_query_value(None, "page_count", |row| row.get(0))
    )?;
    if pages == 0 {
        trace_first_open_step!(
            "PRAGMA auto_vacuum=INCREMENTAL",
            connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        )?;
    }
    // Switching a rollback-journal file to WAL needs an exclusive lock, and
    // SQLite skips the busy handler on that upgrade when another connection
    // is mid-switch (it would risk a deadlock), so two first-openers racing on
    // a fresh file see SQLITE_BUSY straight away. Wait it out ourselves within
    // the same budget the busy handler would have used.
    trace_first_open_step!(
        "PRAGMA journal_mode=WAL (including BUSY retries)",
        retry_while_busy(Duration::from_millis(BUSY_TIMEOUT_MS), || {
            connection.pragma_update(None, "journal_mode", "WAL")
        })
    )?;
    trace_first_open_step!(
        "PRAGMA synchronous=NORMAL",
        connection.pragma_update(None, "synchronous", "NORMAL")
    )?;
    trace_first_open_step!(
        "set_wal_autocheckpoint",
        connection.set_wal_autocheckpoint(DEFAULT_WAL_AUTOCHECKPOINT_PAGES)
    )?;
    Ok(())
}

/// Run `operation` until it stops failing with SQLITE_BUSY/SQLITE_LOCKED or
/// `budget` elapses; the last error is returned when the budget runs out.
/// Shared with the view store, whose pointer database has the same first-open
/// WAL-switch race.
pub(crate) fn retry_while_busy<T>(
    budget: Duration,
    mut operation: impl FnMut() -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + budget;
    let mut backoff = Duration::from_millis(1);
    loop {
        match operation() {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

fn ensure_schema(connection: &mut Connection) -> Result<(), BlobStoreError> {
    let tx = trace_first_open_step!(
        "schema BEGIN IMMEDIATE",
        connection.transaction_with_behavior(TransactionBehavior::Immediate)
    )?;
    trace_first_open_step!("schema CREATE/INSERT batch", tx.execute_batch(BLOB_SCHEMA))?;
    let has_created_at = trace_first_open_step!(
        "schema PRAGMA table_info(blob_payloads)",
        (|| -> rusqlite::Result<Vec<String>> {
            tx.prepare("PRAGMA table_info(blob_payloads)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect()
        })()
    )?
    .iter()
    .any(|column| column == "created_at_ms");
    if !has_created_at {
        // Existing immutable rows predate per-row timestamps and are eligible for
        // the normal age-floor policy immediately after this migration.
        trace_first_open_step!(
            "schema ALTER TABLE created_at_ms",
            tx.execute(
                "ALTER TABLE blob_payloads ADD COLUMN created_at_ms INTEGER NOT NULL DEFAULT 0",
                [],
            )
        )?;
    }
    trace_first_open_step!("schema COMMIT", tx.commit())?;
    Ok(())
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_and_assert_pragmas(
    connection: &TrackedConnection,
) -> Result<BlobStorePragmas, BlobStoreError> {
    let pragmas = BlobStorePragmas {
        journal_mode: trace_first_open_step!(
            "read PRAGMA journal_mode",
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))
        )?,
        synchronous: trace_first_open_step!(
            "read PRAGMA synchronous",
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))
        )?,
        busy_timeout_ms: trace_first_open_step!(
            "read PRAGMA busy_timeout",
            connection.pragma_query_value(None, "busy_timeout", |row| row.get(0))
        )?,
        foreign_keys: trace_first_open_step!(
            "read PRAGMA foreign_keys",
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))
        )?,
        // TrackedConnection owns SQLite's default threshold while replacing
        // the built-in hook with an accounting-equivalent PASSIVE checkpoint.
        wal_autocheckpoint_pages: connection.wal_autocheckpoint_pages(),
    };
    assert_pragma("journal_mode", "wal", &pragmas.journal_mode)?;
    assert_pragma("synchronous", "1", &pragmas.synchronous.to_string())?;
    assert_pragma(
        "busy_timeout",
        &BUSY_TIMEOUT_MS.to_string(),
        &pragmas.busy_timeout_ms.to_string(),
    )?;
    assert_pragma("foreign_keys", "0", &pragmas.foreign_keys.to_string())?;
    assert_pragma(
        "wal_autocheckpoint",
        &DEFAULT_WAL_AUTOCHECKPOINT_PAGES.to_string(),
        &pragmas.wal_autocheckpoint_pages.to_string(),
    )?;
    Ok(pragmas)
}

fn assert_pragma(name: &'static str, expected: &str, actual: &str) -> Result<(), BlobStoreError> {
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(BlobStoreError::PragmaMismatch {
            name,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn is_corrupt_database_error(error: &BlobStoreError) -> bool {
    matches!(
        error,
        BlobStoreError::Sqlite(rusqlite::Error::SqliteFailure(sqlite_error, _))
            if matches!(sqlite_error.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn move_corrupt_database_aside(path: &Path) -> Result<PathBuf, BlobStoreError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| BlobStoreError::InvalidArtifactKey(path.display().to_string()))?;
    let destination = path.with_file_name(format!("{file_name}.corrupt-{timestamp}"));
    crate::db::file_identity::guard_replacement(path, "blob store corrupt-database quarantine");
    fs::rename(path, &destination)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            fs::rename(
                &sidecar,
                PathBuf::from(format!("{}{suffix}", destination.display())),
            )?;
        }
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    fn main_semantic_key(bytes: &[u8], path: &[u8]) -> FullKey {
        full_key(
            BlobPlane::Semantic,
            b"aft/blob-store/semantic/v1",
            &[
                crate::views::io::source_hash(bytes).as_bytes(),
                path,
                b"semantic-v1",
                b"semantic-v1",
                b"fixture-model",
            ],
        )
    }

    #[test]
    fn h_header_invalidation_preserves_main_keys_except_changed_language() {
        assert_eq!(SEMANTIC_PRODUCER_VERSION, "semantic-v1");
        assert_eq!(
            CALLGRAPH_PRODUCER_VERSION,
            "callgraph-v9-python-import-bindings"
        );
        assert_eq!(
            crate::views::callgraph::PRODUCER,
            "ruled-callgraph-v13-python-import-bindings"
        );
        let c = b"int plain(void);\n";
        let cpp = b"template <typename T> class Widget { public: T value(T x) { return x; } };\n";
        let objc = b"@interface Greeter\n@property int value;\n@end\n";
        for (bytes, path, changed) in [
            (c.as_slice(), b"src/plain.h".as_slice(), false),
            (cpp.as_slice(), b"src/widget.h".as_slice(), true),
            (objc.as_slice(), b"src/greeter.h".as_slice(), true),
            (
                b"pub fn untouched() {}\n".as_slice(),
                b"src/lib.rs".as_slice(),
                false,
            ),
            (cpp.as_slice(), b"src/widget.hpp".as_slice(), false),
            (objc.as_slice(), b"src/greeter.m".as_slice(), false),
        ] {
            let current = SemanticKey::for_current(bytes, path, "fixture-model");
            let main = main_semantic_key(bytes, path);
            assert_eq!(current.full_key() != main, changed, "{path:?}");
            let from_hash = SemanticKey::from_source_hash(
                *crate::views::io::source_hash(bytes).as_bytes(),
                path,
                "semantic-v1",
                "semantic-v1",
                "fixture-model",
            )
            .unwrap();
            assert_eq!(current, from_hash);
            if !changed {
                assert_eq!(
                    current.file_key_for_summary_format(1),
                    full_key(
                        BlobPlane::Semantic,
                        b"aft/blob-store/semantic-summary-budget/v1",
                        &[main.as_bytes(), &1_u32.to_le_bytes()]
                    )
                );
            }
        }
        for (bytes, changed) in [
            (c.as_slice(), false),
            (cpp.as_slice(), true),
            (objc.as_slice(), true),
        ] {
            let language = format!(
                "{:?}",
                crate::parser::detect_language_from_source(
                    std::path::Path::new("header.h"),
                    std::str::from_utf8(bytes).unwrap()
                )
                .unwrap()
            )
            .to_lowercase();
            let current = CallgraphKey::for_current(bytes, &language).full_key();
            let main = CallgraphKey::from_bytes(bytes, "c", "callgraph-v9-python-import-bindings")
                .full_key();
            assert_eq!(current != main, changed);
            let mut entry = crate::views::snapshot::LiveEntry::new(
                crate::views::snapshot::DiskState::of_bytes(bytes),
                0,
            );
            let attachment = crate::views::callgraph::attach(&mut entry, bytes, &language).unwrap();
            let main =
                CallgraphKey::from_bytes(bytes, "c", "ruled-callgraph-v13-python-import-bindings")
                    .full_key();
            assert_eq!(
                attachment.key != crate::blob_store::v2::FamilyKey::from(&main),
                changed
            );
        }
    }

    #[test]
    fn h_header_invalidation_hash_only_key_misses_until_language_is_known() {
        let bytes = b"template <typename UnknownHeaderType> class UniqueHeaderCacheFixture {};\n";
        let hash = *crate::views::io::source_hash(bytes).as_bytes();
        assert!(SemanticKey::from_source_hash(
            hash,
            b"unknown.h",
            "semantic-v1",
            "semantic-v1",
            "fixture-model"
        )
        .is_none());
        let from_bytes = SemanticKey::for_current(bytes, b"unknown.h", "fixture-model");
        assert_eq!(
            SemanticKey::from_source_hash(
                hash,
                b"unknown.h",
                "semantic-v1",
                "semantic-v1",
                "fixture-model"
            )
            .unwrap(),
            from_bytes
        );
    }

    use super::*;

    #[test]
    fn semantic_source_hash_constructor_preserves_canonical_keys() {
        for bytes in [
            Vec::new(),
            b"pub fn example() {}\n".to_vec(),
            vec![0xff, 0, 0x80],
            vec![b'x'; 1024 * 1024],
        ] {
            let from_hash = SemanticKey::from_source_hash(
                *crate::views::io::source_hash(&bytes).as_bytes(),
                b"src/example.rs",
                "chunker",
                "template",
                "model",
            )
            .unwrap();
            let from_bytes =
                SemanticKey::from_bytes(&bytes, b"src/example.rs", "chunker", "template", "model");
            assert_eq!(from_hash, from_bytes);
            assert_eq!(from_hash.full_key(), from_bytes.full_key());
            assert_eq!(
                from_hash.file_key_for_summary_format(0),
                from_bytes.full_key()
            );
            assert_eq!(
                from_hash.file_key_for_summary_format(1),
                from_bytes.file_key_for_summary_format(1)
            );
            assert_ne!(
                from_hash.file_key_for_summary_format(1),
                from_hash.full_key()
            );
        }
    }

    #[test]
    fn fresh_blob_stores_enable_incremental_vacuum_before_schema_creation() {
        let dir = tempfile::tempdir().unwrap();
        for plane in [BlobPlane::Callgraph, BlobPlane::Semantic] {
            let store = BlobStore::open(dir.path(), "fresh", plane).unwrap();
            assert_eq!(
                store
                    .connection
                    .pragma_query_value(None, "auto_vacuum", |r| r.get::<_, u32>(0))
                    .unwrap(),
                2
            );
        }
    }

    #[test]
    fn opening_populated_pre_presence_store_adds_no_payload_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blobs/existing/callgraph.sqlite");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut connection = Connection::open(&path).unwrap();
        // Describe the previously deployed schema independently of BLOB_SCHEMA:
        // copying the current schema would hide an accidental index migration.
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             CREATE TABLE blob_payloads (
                 full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32),
                 payload BLOB NOT NULL,
                 payload_digest BLOB NOT NULL CHECK(length(payload_digest) = 32),
                 payload_schema INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL DEFAULT 0
             ) WITHOUT ROWID;
             CREATE INDEX blob_membership ON blob_payloads(full_key);
             CREATE TABLE blob_quarantine (
                 full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32)
             ) WITHOUT ROWID;",
            )
            .unwrap();
        let tx = connection.transaction().unwrap();
        for index in 0..64u32 {
            let key = CallgraphKey::for_current(&index.to_le_bytes(), "typescript").full_key();
            let payload = vec![42; (4 + (index as usize % 5) * 24) * 1024];
            tx.execute(
                "INSERT INTO blob_payloads VALUES (?1, ?2, ?3, 1, 0)",
                params![
                    key.as_bytes().as_slice(),
                    &payload,
                    blake3::hash(&payload).as_bytes().as_slice()
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let indexes = |connection: &Connection| {
            connection
                .prepare(
                    "SELECT name, sql FROM sqlite_schema
                 WHERE type = 'index' AND tbl_name = 'blob_payloads' ORDER BY name",
                )
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let before = indexes(&connection);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, "blob_membership");
        let schema_version: i64 = connection
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .unwrap();
        drop(connection);

        let store = BlobStore::open(dir.path(), "existing", BlobPlane::Callgraph).unwrap();
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "auto_vacuum", |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            indexes(&store.connection),
            before,
            "opening a populated store must not build a new payload index"
        );
        assert_eq!(
            store
                .connection
                .pragma_query_value::<i64, _>(None, "schema_version", |row| row.get(0))
                .unwrap(),
            // The removal counter and its trigger do not rebuild payload rows
            // or indexes, but each legitimately advances SQLite's schema cookie.
            schema_version + 2
        );
        let (identity, removed) = store.removal_stamp().unwrap();
        assert_eq!(identity.len(), 16);
        assert_eq!(removed, 0);
    }

    /// Race eight first-openers on fresh files. Failed startup statements print
    /// their own timing so a failure can distinguish a skipped busy handler from
    /// a timeout, rather than assuming every BUSY comes from the WAL switch.
    #[test]
    fn concurrent_first_opens_never_see_busy() {
        let mut failures = Vec::new();
        for round in 0..150 {
            let dir = tempfile::tempdir().expect("tempdir");
            let handles: Vec<_> = (0..8)
                .map(|opener| {
                    let storage = dir.path().to_path_buf();
                    std::thread::spawn(move || {
                        let started = std::time::Instant::now();
                        BlobStore::open(&storage, "fam", BlobPlane::Semantic)
                            .map(|_| ())
                            .map_err(|error| {
                                format!(
                                    "opener {opener}, thread={:?}, total elapsed={:?}: {error}",
                                    std::thread::current().id(),
                                    started.elapsed()
                                )
                            })
                    })
                })
                .collect();
            for handle in handles {
                if let Err(error) = handle.join().expect("opener thread") {
                    failures.push(format!("round {round}: {error}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "concurrent first-open must wait out SQLite contention: {failures:?}"
        );
    }

    #[test]
    fn payload_schema_and_producer_version_pairs_are_pinned() {
        assert_eq!(
            [
                (SEMANTIC_PAYLOAD_SCHEMA, SEMANTIC_PRODUCER_VERSION),
                (CALLGRAPH_PAYLOAD_SCHEMA, CALLGRAPH_PRODUCER_VERSION),
            ],
            [
                (1, "semantic-v1"),
                (1, "callgraph-v9-python-import-bindings")
            ],
            "a payload encoding change must bump its producer key version in the same edit"
        );
    }

    #[test]
    fn semantic_paths_are_distinct_while_callgraph_content_reuses() {
        let bytes = b"same source";
        let semantic_a = SemanticKey::for_current(bytes, b"src/a.rs", "model-a").full_key();
        let semantic_b = SemanticKey::for_current(bytes, b"src/b.rs", "model-a").full_key();
        let callgraph_a = CallgraphKey::for_current(bytes, "rust").full_key();
        let callgraph_b = CallgraphKey::for_current(bytes, "rust").full_key();

        assert_ne!(semantic_a, semantic_b);
        assert_eq!(callgraph_a, callgraph_b);
    }

    #[test]
    fn config_is_a_valid_callgraph_language() {
        let key = CallgraphKey::for_current(b"[package]", "config");
        assert_ne!(key.source_digest(), &[0; 32]);
    }

    #[test]
    fn abandoned_insert_transaction_leaves_no_partial_payload_row() {
        let directory = tempfile::tempdir().expect("create temporary storage");
        let mut store = BlobStore::open(directory.path(), "family-a", BlobPlane::Semantic)
            .expect("open blob store");
        let key = SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key();
        let payload = b"payload";
        let payload_digest = blake3::hash(payload);

        {
            let tx = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("start payload transaction");
            tx.execute(
                "INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    key.as_bytes().as_slice(),
                    payload,
                    payload_digest.as_bytes().as_slice(),
                    i64::from(SEMANTIC_PAYLOAD_SCHEMA),
                ],
            )
            .expect("stage payload row");
            // Dropping an uncommitted SQLite transaction models a process that
            // dies after staging a row but before the put transaction commits.
        }

        assert_eq!(store.get(&key).expect("read after aborted put"), None);
    }

    #[test]
    fn usage_counts_rows_payloads_and_sqlite_pages() {
        let directory = tempfile::tempdir().expect("create temporary storage");
        let mut store = BlobStore::open(directory.path(), "family-a", BlobPlane::Semantic)
            .expect("open blob store");
        let key = SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key();
        store.put(&key, b"payload").expect("insert payload");

        let usage = store.usage().expect("read usage");
        assert_eq!(usage.rows, 1);
        assert_eq!(usage.payload_bytes, 7);
        assert!(usage.file_bytes >= usage.payload_bytes);
    }

    #[test]
    fn only_inserted_and_reused_are_durable() {
        for outcome in [
            PutOutcome::Inserted,
            PutOutcome::Reused,
            PutOutcome::Quarantined,
            PutOutcome::Failed,
            PutOutcome::QuotaExceeded,
        ] {
            assert_eq!(
                PutReport::new(outcome).durable,
                matches!(outcome, PutOutcome::Inserted | PutOutcome::Reused)
            );
        }
    }
    #[test]
    fn missing_hex_keys_batches_index_probes_and_reports_an_expired_budget_as_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "presence", BlobPlane::Callgraph).unwrap();
        // More than one batch, every other key stored.
        let keys = (0..(MEMBERSHIP_BATCH * 2 + 7))
            .map(|index| CallgraphKey::for_current(&index.to_le_bytes(), "rust").full_key())
            .collect::<Vec<_>>();
        for key in keys.iter().step_by(2) {
            store.put(key, b"payload").unwrap();
        }
        let hex = keys.iter().map(FullKey::to_hex).collect::<Vec<_>>();
        let malformed = "not-a-key".to_string();
        let asked = hex.iter().chain([&malformed, &hex[0]]);

        let presence = store
            .missing_hex_keys(asked.clone().map(String::as_str), None)
            .unwrap();
        assert!(presence.complete());
        assert_eq!(presence.total, keys.len() + 1, "duplicates count once");
        let expected = hex
            .iter()
            .skip(1)
            .step_by(2)
            .chain([&malformed])
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(presence.missing, expected);

        let expired = store
            .missing_hex_keys(
                asked.map(String::as_str),
                Some(
                    std::time::Instant::now()
                        .checked_sub(Duration::from_millis(1))
                        .unwrap_or_else(std::time::Instant::now),
                ),
            )
            .unwrap();
        assert!(
            !expired.complete(),
            "an expired budget is not a clean check"
        );
        assert_eq!(expired.checked, 0);
        assert!(expired.missing.is_empty());
    }

    #[test]
    fn contains_uses_covering_index_without_payload_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "presence", BlobPlane::Callgraph).unwrap();
        let key = CallgraphKey::for_current(b"source", "typescript").full_key();
        assert!(!store.contains(&key).unwrap());
        let payload = vec![42; 8 * 1024 * 1024];
        store.put(&key, &payload).unwrap();
        PAYLOAD_READ_COUNTS.with(|counts| counts.set(PayloadReadCounts::default()));
        assert!(store.contains(&key).unwrap());
        assert_eq!(
            PAYLOAD_READ_COUNTS.with(|counts| counts.get()),
            PayloadReadCounts::default()
        );
        let plan: String = store
            .connection
            .query_row(
                &format!("EXPLAIN QUERY PLAN {CONTAINS_SQL}"),
                params![key.as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("COVERING INDEX blob_membership"), "{plan}");
        store
            .connection
            .execute("UPDATE blob_payloads SET payload_schema = 999", [])
            .unwrap();
        assert!(
            store.contains(&key).unwrap(),
            "membership does not validate schema"
        );
        assert!(
            store.get(&key).unwrap().is_none(),
            "schema is checked at consumption"
        );
        let semantic = SemanticKey::for_current(b"source", b"source.ts", "model").full_key();
        assert!(matches!(
            store.contains(&semantic),
            Err(BlobStoreError::PlaneKeyMismatch { .. })
        ));
    }
}
