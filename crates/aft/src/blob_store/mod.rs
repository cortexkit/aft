//! Immutable, content-addressed payload storage shared by a repository family.
//!
//! Each [`BlobStore`] owns one SQLite connection for exactly one plane.  The
//! caller supplies the family (`artifact_key`); this module maps it to the
//! fixed on-disk layout and never derives a family from a checkout path.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};

use crate::db::lifecycle::{SqliteStore, TrackedConnection};

/// The SQLite busy wait used by every blob-store connection.
pub const BUSY_TIMEOUT_MS: u64 = 5_000;
/// SQLite's documented connection default.  The blob store reads this value
/// after opening instead of overriding it so future SQLite defaults are caught.
pub const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;

/// Payload encodings and the key producers that name them are pinned together.
/// A payload format change must update the corresponding producer string in the
/// same edit, because producer versions are components of content-address keys.
pub const SEMANTIC_PAYLOAD_SCHEMA: u32 = 1;
pub const SEMANTIC_PRODUCER_VERSION: &str = "semantic-v1";
pub const CALLGRAPH_PAYLOAD_SCHEMA: u32 = 1;
pub const CALLGRAPH_PRODUCER_VERSION: &str = "callgraph-v1";

const BLOB_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS blob_payloads (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32),
    payload BLOB NOT NULL,
    payload_digest BLOB NOT NULL CHECK(length(payload_digest) = 32),
    payload_schema INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS blob_quarantine (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32)
) WITHOUT ROWID;
"#;

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
}

impl SemanticKey {
    pub fn from_bytes(
        bytes: &[u8],
        rel_path: &[u8],
        chunker_version: impl Into<String>,
        embed_template_version: impl Into<String>,
        model_fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            source_digest: *blake3::hash(bytes).as_bytes(),
            rel_path: rel_path.to_vec(),
            chunker_version: chunker_version.into(),
            embed_template_version: embed_template_version.into(),
            model_fingerprint: model_fingerprint.into(),
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
        full_key(
            BlobPlane::Semantic,
            b"aft/blob-store/semantic/v1",
            &[
                &self.source_digest,
                &self.rel_path,
                self.chunker_version.as_bytes(),
                self.embed_template_version.as_bytes(),
                self.model_fingerprint.as_bytes(),
            ],
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
        Self {
            source_digest: *blake3::hash(bytes).as_bytes(),
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
            fs::create_dir_all(parent)?;
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
        let mut connection = TrackedConnection::open(&path, SqliteStore::BlobStore)?;
        configure_connection(&connection)?;
        ensure_schema(&mut connection)?;
        let pragmas = read_and_assert_pragmas(&connection)?;
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

    pub fn pragmas(&self) -> &BlobStorePragmas {
        &self.pragmas
    }

    /// Returns current payload rows plus the SQLite file allocation.  Quota
    /// admission uses this before an insert so a rejected payload never enters
    /// the immutable store.
    pub fn usage(&self) -> Result<BlobUsage, BlobStoreError> {
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
        Ok(PutReport::new(if inserted == 1 {
            PutOutcome::Inserted
        } else {
            PutOutcome::Reused
        }))
    }

    /// Reads a payload only after verifying its stored digest and schema.  A
    /// malformed row is indistinguishable from a miss to consumers, but remains
    /// on disk for quarantine/forensics rather than being rewritten in place.
    pub fn get(&self, full_key: &FullKey) -> Result<Option<Vec<u8>>, BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        let row = self
            .connection
            .query_row(
                "SELECT payload, payload_digest, payload_schema
                 FROM blob_payloads WHERE full_key = ?1",
                params![full_key.as_bytes().as_slice()],
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

        let digest_matches = payload_digest.as_slice() == blake3::hash(&payload).as_bytes();
        let schema_matches = payload_schema == i64::from(self.plane.payload_schema());
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
            "blob store rejected committed payload for key {} in {}/{}: {}",
            full_key,
            self.artifact_key,
            self.plane.as_str(),
            reason
        );
        Ok(None)
    }

    /// Records a deterministic failure without modifying the immutable payload
    /// table.  Subsequent puts report `quarantined` until a changed key is used.
    pub fn quarantine(&mut self, full_key: &FullKey) -> Result<(), BlobStoreError> {
        self.ensure_key_plane(full_key)?;
        self.connection.execute(
            "INSERT INTO blob_quarantine (full_key) VALUES (?1)
             ON CONFLICT(full_key) DO NOTHING",
            params![full_key.as_bytes().as_slice()],
        )?;
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

fn configure_connection(connection: &Connection) -> Result<(), BlobStoreError> {
    // Set the wait policy before WAL attempts to acquire the journal lock so
    // concurrent first-open callers wait instead of failing immediately.
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    // Switching a rollback-journal file to WAL needs an exclusive lock, and
    // SQLite skips the busy handler on that upgrade when another connection
    // is mid-switch (it would risk a deadlock), so two first-openers racing on
    // a fresh file see SQLITE_BUSY straight away. Wait it out ourselves within
    // the same budget the busy handler would have used.
    retry_while_busy(Duration::from_millis(BUSY_TIMEOUT_MS), || {
        connection.pragma_update(None, "journal_mode", "WAL")
    })?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
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
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(BLOB_SCHEMA)?;
    let has_created_at = tx
        .prepare("PRAGMA table_info(blob_payloads)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == "created_at_ms");
    if !has_created_at {
        // Existing immutable rows predate per-row timestamps and are eligible for
        // the normal age-floor policy immediately after this migration.
        tx.execute(
            "ALTER TABLE blob_payloads ADD COLUMN created_at_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_and_assert_pragmas(connection: &Connection) -> Result<BlobStorePragmas, BlobStoreError> {
    let pragmas = BlobStorePragmas {
        journal_mode: connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?,
        synchronous: connection.pragma_query_value(None, "synchronous", |row| row.get(0))?,
        busy_timeout_ms: connection.pragma_query_value(None, "busy_timeout", |row| row.get(0))?,
        foreign_keys: connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
        // Do not write this pragma: preserving SQLite's default is part of the
        // storage contract and querying it catches accidental future overrides.
        wal_autocheckpoint_pages: connection.pragma_query_value(
            None,
            "wal_autocheckpoint",
            |row| row.get(0),
        )?,
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
    use super::*;

    /// Eight racing first-openers on a fresh file trip the WAL-switch BUSY that
    /// the busy handler does not cover. Without the retry this sees 1-3
    /// failures per 60 rounds on an idle laptop; 150 rounds make the red
    /// reliable while keeping the test under two seconds.
    #[test]
    fn concurrent_first_opens_never_see_busy() {
        let mut failures = Vec::new();
        for round in 0..150 {
            let dir = tempfile::tempdir().expect("tempdir");
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let storage = dir.path().to_path_buf();
                    std::thread::spawn(move || {
                        BlobStore::open(&storage, "fam", BlobPlane::Semantic).map(|_| ())
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
            "concurrent first-open must wait out the WAL switch: {failures:?}"
        );
    }

    #[test]
    fn payload_schema_and_producer_version_pairs_are_pinned() {
        assert_eq!(
            [
                (SEMANTIC_PAYLOAD_SCHEMA, SEMANTIC_PRODUCER_VERSION),
                (CALLGRAPH_PAYLOAD_SCHEMA, CALLGRAPH_PRODUCER_VERSION),
            ],
            [(1, "semantic-v1"), (1, "callgraph-v1")],
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
}
