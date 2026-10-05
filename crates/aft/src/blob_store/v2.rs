//! Versioned family blob stores for per-checkout views (`blobs/v2/<family>/`).
//!
//! The v1 stores beside this module stay untouched until the cutover, because
//! old daemons still sweep `blobs/<family>/` with a single view's references.
//! A v2 store differs from v1 in four ways that the family GC protocol needs:
//!
//! - every row carries a `ref_epoch`, and each store keeps a `gc_epoch`;
//! - `put` is put-or-touch: inserting or reusing a key stamps the store's
//!   current epoch on the row in the same IMMEDIATE transaction, so a sweep
//!   that bumped the epoch before the touch cannot delete the row;
//! - a put whose payload differs from the stored payload for the same key is
//!   rejected instead of being reported as a reuse, because every producer
//!   input is part of the key and equal keys must name equal bytes;
//! - stores are created with `auto_vacuum=INCREMENTAL` so a sweep can shrink
//!   the file after deleting rows.
//!
//! There is exactly one SQLite connection per store file per process. Every
//! handle for the same file shares it, which keeps a second descriptor off a
//! live SQLite file set (closing any descriptor drops every POSIX lock the
//! process holds on that file).
//!
//! Opening a store for writing needs a [`StoreWriteAccess`], which only a
//! registered view can hand out (see `views::registry`). That makes
//! registration happen before the first put, pin or segment of a view, which
//! the GC safety argument depends on. Readers that hold no view get a
//! read-only handle that never creates or modifies a store.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use rusqlite::{params, OpenFlags, OptionalExtension, TransactionBehavior};

use crate::blob_store::{retry_while_busy, FullKey, BUSY_TIMEOUT_MS};
use crate::db::lifecycle::{SqliteStore, TrackedConnection};

/// Directory component that separates v2 family stores from v1 ones.
pub const V2_DIR: &str = "v2";
/// Schema version recorded in every v2 plane store.
pub const STORE_SCHEMA_VERSION: i64 = 1;
/// Payload encoding version of trigram blobs. Bumping it must also change the
/// trigram policy fingerprint, because that fingerprint is part of every key.
pub const TRIGRAM_PAYLOAD_SCHEMA: u32 = 1;

const STORE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS store_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    plane TEXT NOT NULL,
    gc_epoch INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blob_payloads (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32),
    payload BLOB NOT NULL,
    payload_digest BLOB NOT NULL CHECK(length(payload_digest) = 32),
    payload_schema INTEGER NOT NULL,
    ref_epoch INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL
) WITHOUT ROWID;
-- Membership and epoch probes must not read payload overflow pages.
CREATE INDEX IF NOT EXISTS blob_membership ON blob_payloads(full_key, ref_epoch);
CREATE TABLE IF NOT EXISTS blob_quarantine (
    full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key) = 32)
) WITHOUT ROWID;
-- Trigram segments are content-named files beside the store. A row exists
-- before the file is written, so a sweep can never meet a segment file that no
-- row accounts for while its builder is alive.
CREATE TABLE IF NOT EXISTS segments (
    segment_id BLOB NOT NULL PRIMARY KEY CHECK(length(segment_id) = 32),
    state TEXT NOT NULL CHECK(state IN ('building', 'durable')),
    byte_len INTEGER NOT NULL,
    ref_epoch INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL
) WITHOUT ROWID;
"#;

/// The planes of a v2 family. Trigram payloads are family-shared in v2, unlike
/// v1, where trigram data was per-view derived state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum FamilyPlane {
    Trigram,
    Semantic,
    Callgraph,
}

impl FamilyPlane {
    pub const ALL: [Self; 3] = [Self::Trigram, Self::Semantic, Self::Callgraph];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigram => "trigram",
            Self::Semantic => "semantic",
            Self::Callgraph => "callgraph",
        }
    }

    const fn payload_schema(self) -> u32 {
        match self {
            Self::Trigram => TRIGRAM_PAYLOAD_SCHEMA,
            Self::Semantic => crate::blob_store::SEMANTIC_PAYLOAD_SCHEMA,
            Self::Callgraph => crate::blob_store::CALLGRAPH_PAYLOAD_SCHEMA,
        }
    }
}

impl From<crate::blob_store::BlobPlane> for FamilyPlane {
    fn from(plane: crate::blob_store::BlobPlane) -> Self {
        match plane {
            crate::blob_store::BlobPlane::Semantic => Self::Semantic,
            crate::blob_store::BlobPlane::Callgraph => Self::Callgraph,
        }
    }
}

/// BLAKE3 of one file's bytes. Every plane key of a manifest entry is derived
/// from the one buffer whose hash this is.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    pub fn from_hex(value: &str) -> Option<Self> {
        parse_hex32(value).map(Self)
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl serde::Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text)
            .ok_or_else(|| serde::de::Error::custom("content must be 64 lower-case hex digits"))
    }
}

/// A key stored in a v2 plane. The bytes are a domain-separated digest of the
/// plane's key tuple, and that tuple always includes the producer identity
/// (model and chunker, extractor version, or trigram policy fingerprint).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FamilyKey {
    plane: FamilyPlane,
    bytes: [u8; 32],
}

impl FamilyKey {
    pub const fn new(plane: FamilyPlane, bytes: [u8; 32]) -> Self {
        Self { plane, bytes }
    }

    pub const fn plane(&self) -> FamilyPlane {
        self.plane
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    pub fn to_hex(&self) -> String {
        to_hex(&self.bytes)
    }
}

impl From<&FullKey> for FamilyKey {
    fn from(key: &FullKey) -> Self {
        Self::new(key.plane().into(), *key.as_bytes())
    }
}

impl fmt::Display for FamilyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.plane.as_str(), self.to_hex())
    }
}

/// Everything that decides a trigram payload, hashed into every trigram key.
/// Two checkouts with different size limits can therefore never store
/// different payloads under one key.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TrigramPolicy {
    pub max_file_size: u64,
}

impl TrigramPolicy {
    /// The trigram packing, case folding and mask scheme this payload encodes.
    /// It mirrors the search index's posting fold; change it with that fold.
    pub const MASK_SCHEME: &'static str = "lower-ascii/next31x7/pos-mod-8/eof-next-bit0";
    /// The binary rule applied to a file's bytes before extraction.
    pub const BINARY_RULE: &'static str = "content_inspector";

    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"aft/trigram-policy/v1");
        for field in [
            &TRIGRAM_PAYLOAD_SCHEMA.to_be_bytes()[..],
            Self::MASK_SCHEME.as_bytes(),
            Self::BINARY_RULE.as_bytes(),
            &self.max_file_size.to_be_bytes()[..],
        ] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        *hasher.finalize().as_bytes()
    }

    pub fn fingerprint_hex(&self) -> String {
        to_hex(&self.fingerprint())
    }
}

/// A trigram blob identity: content plus policy, and deliberately no path,
/// because trigram postings do not embed the path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TrigramKey {
    pub content: ContentHash,
    pub policy: TrigramPolicy,
}

impl TrigramKey {
    pub fn family_key(&self) -> FamilyKey {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"aft/blob-store/trigram/v2");
        hasher.update(self.content.as_bytes());
        hasher.update(&self.policy.fingerprint());
        FamilyKey::new(FamilyPlane::Trigram, *hasher.finalize().as_bytes())
    }
}

/// Outcomes of [`FamilyStore::put_or_touch`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PutOrTouch {
    /// A new row, stamped with the store's current epoch.
    Inserted { ref_epoch: u64 },
    /// An equal payload already existed; its row now carries the current epoch.
    Reused { ref_epoch: u64 },
    /// The key is quarantined as a deterministic failure; nothing was stored.
    Quarantined,
}

/// The result of touching keys that the caller already relies on.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TouchReport {
    pub touched: usize,
    /// Keys that are not stored any more. The caller must put them again from
    /// the bytes it still holds, or abort the work that relied on them.
    pub missing: Vec<FamilyKey>,
    pub ref_epoch: u64,
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidFamily(String),
    PlaneMismatch {
        store: FamilyPlane,
        key: FamilyPlane,
    },
    /// The stored payload for this key differs from the one being put.
    ConflictingPayload(FamilyKey),
    /// The file on disk is a store of another plane or of a newer schema.
    Incompatible(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "family store I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "family store SQLite error: {error}"),
            Self::InvalidFamily(family) => write!(f, "invalid family key `{family}`"),
            Self::PlaneMismatch { store, key } => write!(
                f,
                "a {} key cannot be stored in the {} plane",
                key.as_str(),
                store.as_str()
            ),
            Self::ConflictingPayload(key) => write!(
                f,
                "a different payload is already stored under key {key}; equal keys must name equal bytes"
            ),
            Self::Incompatible(message) => write!(f, "incompatible family store: {message}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Proof that the caller is a registered view of `family`. Only the family
/// registry constructs it, so no v2 store can be written before registration.
#[derive(Clone, Debug)]
pub struct StoreWriteAccess {
    storage: PathBuf,
    family: String,
}

impl StoreWriteAccess {
    /// Crate-internal: the family registry hands this out to a registered view.
    pub(crate) fn for_registered_view(storage: &Path, family: &str) -> Self {
        Self {
            storage: storage.to_path_buf(),
            family: family.to_owned(),
        }
    }

    pub fn storage(&self) -> &Path {
        &self.storage
    }

    pub fn family(&self) -> &str {
        &self.family
    }
}

/// `<storage>/blobs/v2/<family>/`.
pub fn family_dir(storage: &Path, family: &str) -> StoreResult<PathBuf> {
    validate_family(family)?;
    Ok(storage.join("blobs").join(V2_DIR).join(family))
}

pub fn plane_path(storage: &Path, family: &str, plane: FamilyPlane) -> StoreResult<PathBuf> {
    Ok(family_dir(storage, family)?.join(format!("{}.sqlite", plane.as_str())))
}

/// `<family>/trigram-seg-<id>.bin`.
pub fn segment_path(storage: &Path, family: &str, segment_id: &[u8; 32]) -> StoreResult<PathBuf> {
    Ok(family_dir(storage, family)?.join(format!("trigram-seg-{}.bin", to_hex(segment_id))))
}

struct StoreInner {
    family: String,
    plane: FamilyPlane,
    path: PathBuf,
    connection: Mutex<TrackedConnection>,
    schema_ready: Mutex<bool>,
}

impl fmt::Debug for StoreInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreInner")
            .field("family", &self.family)
            .field("plane", &self.plane)
            .field("path", &self.path)
            .finish()
    }
}

fn open_stores() -> &'static Mutex<HashMap<PathBuf, Weak<StoreInner>>> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Weak<StoreInner>>>> = OnceLock::new();
    STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A writable handle on one v2 plane store. Clones share one connection.
#[derive(Clone, Debug)]
pub struct FamilyStore {
    inner: Arc<StoreInner>,
}

/// A read-only handle: no create, no schema change, no put, no touch.
#[derive(Clone, Debug)]
pub struct FamilyStoreReader {
    inner: Arc<StoreInner>,
}

impl FamilyStore {
    /// Opens (creating when absent) `<storage>/blobs/v2/<family>/<plane>.sqlite`.
    pub fn open(access: &StoreWriteAccess, plane: FamilyPlane) -> StoreResult<Self> {
        let path = plane_path(&access.storage, &access.family, plane)?;
        if let Some(parent) = path.parent() {
            crate::private_storage::open_dir(&access.storage, parent)?;
        }
        let inner = shared_inner(&access.family, plane, &path, true)?;
        ensure_schema(&inner)?;
        Ok(Self { inner })
    }

    pub fn reader(&self) -> FamilyStoreReader {
        FamilyStoreReader {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn plane(&self) -> FamilyPlane {
        self.inner.plane
    }

    pub fn family(&self) -> &str {
        &self.inner.family
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Inserts a payload once, or stamps the current epoch on an equal existing
    /// payload. The caller's protection (an assembly or live pin listing this
    /// key) must already be durable; see `pins::Protection`.
    pub fn put_or_touch(&self, key: &FamilyKey, payload: &[u8]) -> StoreResult<PutOrTouch> {
        self.ensure_plane(key)?;
        let digest = blake3::hash(payload);
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let quarantined = tx
            .query_row(
                "SELECT 1 FROM blob_quarantine WHERE full_key = ?1",
                params![key.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if quarantined {
            tx.commit()?;
            return Ok(PutOrTouch::Quarantined);
        }
        let epoch = read_epoch(&tx)?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT payload_digest FROM blob_payloads WHERE full_key = ?1",
                params![key.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let outcome = match existing {
            Some(stored) if stored.as_slice() == digest.as_bytes() => {
                tx.execute(
                    "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
                    params![key.as_bytes().as_slice(), epoch as i64],
                )?;
                PutOrTouch::Reused { ref_epoch: epoch }
            }
            Some(_) => return Err(StoreError::ConflictingPayload(*key)),
            None => {
                tx.execute(
                    "INSERT INTO blob_payloads
                     (full_key, payload, payload_digest, payload_schema, ref_epoch, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        key.as_bytes().as_slice(),
                        payload,
                        digest.as_bytes().as_slice(),
                        i64::from(self.inner.plane.payload_schema()),
                        epoch as i64,
                        now_ms() as i64,
                    ],
                )?;
                PutOrTouch::Inserted { ref_epoch: epoch }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Stamps the current epoch on every stored key in one transaction and
    /// reports the ones that are gone.
    pub fn touch(&self, keys: &[FamilyKey]) -> StoreResult<TouchReport> {
        for key in keys {
            self.ensure_plane(key)?;
        }
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        #[cfg(test)]
        TOUCH_TRANSACTIONS.with(|count| count.set(count.get() + 1));
        let epoch = read_epoch(&tx)?;
        let mut report = TouchReport {
            ref_epoch: epoch,
            ..TouchReport::default()
        };
        {
            let mut update = tx.prepare(
                "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
            )?;
            for key in keys {
                if update.execute(params![key.as_bytes().as_slice(), epoch as i64])? == 1 {
                    report.touched += 1;
                } else {
                    report.missing.push(*key);
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Records a deterministic failure; later puts of the key report
    /// [`PutOrTouch::Quarantined`].
    pub fn quarantine(&self, key: &FamilyKey) -> StoreResult<()> {
        self.ensure_plane(key)?;
        lock(&self.inner.connection).execute(
            "INSERT INTO blob_quarantine (full_key) VALUES (?1) ON CONFLICT(full_key) DO NOTHING",
            params![key.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    pub fn get(&self, key: &FamilyKey) -> StoreResult<Option<Vec<u8>>> {
        get(&self.inner, key)
    }

    pub fn contains(&self, key: &FamilyKey) -> StoreResult<bool> {
        contains(&self.inner, key)
    }

    pub fn gc_epoch(&self) -> StoreResult<u64> {
        read_epoch(&lock(&self.inner.connection))
    }

    /// Raises the store's epoch to at least `epoch`. The family sweep calls
    /// this in every store before it starts marking.
    pub fn raise_epoch(&self, epoch: u64) -> StoreResult<u64> {
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE store_meta SET gc_epoch = MAX(gc_epoch, ?1) WHERE singleton = 1",
            params![epoch as i64],
        )?;
        let current = read_epoch(&tx)?;
        tx.commit()?;
        Ok(current)
    }

    /// Every stored key with its payload size and epoch, oldest epoch first.
    pub fn rows(&self) -> StoreResult<Vec<StoredRow>> {
        self.rows_oldest(usize::MAX)
    }

    /// At most `limit` stored keys, oldest epoch first. The limit is applied
    /// by SQLite, so a store with millions of rows never has to be listed
    /// whole by a caller that only needs the oldest ones.
    pub fn rows_oldest(&self, limit: usize) -> StoreResult<Vec<StoredRow>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let connection = lock(&self.inner.connection);
        let mut statement = connection.prepare(
            "SELECT full_key, length(payload), ref_epoch FROM blob_payloads
             ORDER BY ref_epoch ASC, full_key ASC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (key, bytes, ref_epoch) = row?;
            let Ok(key) = <[u8; 32]>::try_from(key) else {
                continue;
            };
            result.push(StoredRow {
                key: FamilyKey::new(self.inner.plane, key),
                payload_bytes: bytes,
                ref_epoch: ref_epoch.max(0) as u64,
            });
        }
        Ok(result)
    }

    /// Deletes `key` only when no touch at or after epoch `sweep_epoch` has
    /// reached it. This is the revalidation that makes deletion safe against a
    /// reuse that committed after marking began.
    pub fn delete_if_unreferenced_since(
        &self,
        key: &FamilyKey,
        sweep_epoch: u64,
    ) -> StoreResult<bool> {
        self.ensure_plane(key)?;
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            DELETE_UNREFERENCED_SQL,
            params![key.as_bytes().as_slice(), sweep_epoch as i64],
        )?;
        tx.commit()?;
        Ok(deleted == 1)
    }

    /// Returns freed pages to the filesystem.
    pub fn incremental_vacuum(&self) -> StoreResult<()> {
        lock(&self.inner.connection).execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }

    /// Shrinks the store's files after rows were deleted: `incremental_vacuum`
    /// moves the freed pages out of the database, and a TRUNCATE checkpoint
    /// copies that change from the write-ahead log into the main file and
    /// empties the log. In WAL mode the main file only gets shorter at that
    /// checkpoint. Both go through this store's own connection; opening the
    /// files any other way would drop the locks SQLite holds on them.
    ///
    /// Returns `false` when another connection kept the checkpoint from
    /// finishing. The freed pages are still out of the database then, and
    /// the next checkpoint returns them.
    pub fn reclaim_space(&self) -> StoreResult<bool> {
        let connection = lock(&self.inner.connection);
        connection.execute_batch("PRAGMA incremental_vacuum;")?;
        let busy: i64 =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        Ok(busy == 0)
    }

    pub fn usage(&self) -> StoreResult<StoreUsage> {
        usage(&self.inner)
    }

    pub fn auto_vacuum_mode(&self) -> StoreResult<i64> {
        Ok(lock(&self.inner.connection)
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))?)
    }

    fn ensure_plane(&self, key: &FamilyKey) -> StoreResult<()> {
        if key.plane() == self.inner.plane {
            Ok(())
        } else {
            Err(StoreError::PlaneMismatch {
                store: self.inner.plane,
                key: key.plane(),
            })
        }
    }

    /// Records a segment before its file is written (state `building`), or
    /// touches an existing row. Only the trigram store holds segments.
    pub fn begin_segment(&self, segment_id: &[u8; 32]) -> StoreResult<SegmentRow> {
        self.require_trigram()?;
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch = read_epoch(&tx)?;
        tx.execute(
            "INSERT INTO segments (segment_id, state, byte_len, ref_epoch, created_at_ms)
             VALUES (?1, 'building', 0, ?2, ?3)
             ON CONFLICT(segment_id) DO UPDATE SET ref_epoch = MAX(ref_epoch, excluded.ref_epoch)",
            params![segment_id.as_slice(), epoch as i64, now_ms() as i64],
        )?;
        let row = read_segment_row(&tx, segment_id)?.ok_or_else(|| {
            StoreError::Incompatible("segment row vanished inside its transaction".to_string())
        })?;
        tx.commit()?;
        Ok(row)
    }

    /// Marks a segment row durable after its file was renamed into place and
    /// synced. A row that disappeared meanwhile is reported as `false`, and the
    /// caller must begin the segment again.
    pub fn finish_segment(&self, segment_id: &[u8; 32], byte_len: u64) -> StoreResult<bool> {
        self.require_trigram()?;
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch = read_epoch(&tx)?;
        let updated = tx.execute(
            "UPDATE segments SET state = 'durable', byte_len = ?2,
                                 ref_epoch = MAX(ref_epoch, ?3)
             WHERE segment_id = ?1",
            params![segment_id.as_slice(), byte_len as i64, epoch as i64],
        )?;
        tx.commit()?;
        Ok(updated == 1)
    }

    /// Touches a segment that a new generation adopts.
    pub fn touch_segment(&self, segment_id: &[u8; 32]) -> StoreResult<Option<SegmentRow>> {
        self.require_trigram()?;
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch = read_epoch(&tx)?;
        tx.execute(
            "UPDATE segments SET ref_epoch = MAX(ref_epoch, ?2) WHERE segment_id = ?1",
            params![segment_id.as_slice(), epoch as i64],
        )?;
        let row = read_segment_row(&tx, segment_id)?;
        tx.commit()?;
        Ok(row)
    }

    pub fn segment(&self, segment_id: &[u8; 32]) -> StoreResult<Option<SegmentRow>> {
        self.require_trigram()?;
        read_segment_row(&lock(&self.inner.connection), segment_id)
    }

    pub fn segments(&self) -> StoreResult<Vec<SegmentRow>> {
        if self.inner.plane != FamilyPlane::Trigram {
            return Ok(Vec::new());
        }
        let connection = lock(&self.inner.connection);
        let mut statement = connection.prepare(
            "SELECT segment_id, state, byte_len, ref_epoch FROM segments ORDER BY segment_id",
        )?;
        let rows = statement.query_map([], segment_row_from)?;
        let mut result = Vec::new();
        for row in rows {
            if let Some(row) = row? {
                result.push(row);
            }
        }
        Ok(result)
    }

    /// Deletes a segment row with the same epoch revalidation as a blob, and
    /// unlinks its file while the write lock is still held. A builder that
    /// re-adopts the segment has to insert its row first, which it can only do
    /// after this transaction ends, so the unlink can never remove a file the
    /// builder wrote after re-adopting it.
    pub fn delete_segment_if_unreferenced_since(
        &self,
        segment_id: &[u8; 32],
        sweep_epoch: u64,
        file: &Path,
    ) -> StoreResult<SegmentDeletion> {
        self.delete_segment_observed(segment_id, sweep_epoch, file, &|_| {})
    }

    /// [`Self::delete_segment_if_unreferenced_since`], reporting each step
    /// to `observe` while the write lock is still held. Tests use the steps
    /// to stop or kill a deleting process between the row and the file.
    pub fn delete_segment_observed(
        &self,
        segment_id: &[u8; 32],
        sweep_epoch: u64,
        file: &Path,
        observe: &dyn Fn(SegmentDeletionStep),
    ) -> StoreResult<SegmentDeletion> {
        self.require_trigram()?;
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM segments WHERE segment_id = ?1 AND ref_epoch < ?2",
            params![segment_id.as_slice(), sweep_epoch as i64],
        )?;
        if deleted == 0 {
            tx.commit()?;
            return Ok(SegmentDeletion::Retained);
        }
        observe(SegmentDeletionStep::RowDeleted);
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // An open segment cannot be unlinked on Windows. Keep the row so
                // the next sweep retries instead of orphaning the file.
                drop(tx);
                return Ok(SegmentDeletion::FileBusy);
            }
        }
        observe(SegmentDeletionStep::FileUnlinked);
        tx.commit()?;
        crate::fs_lock::sync_parent(file);
        Ok(SegmentDeletion::Deleted)
    }

    fn require_trigram(&self) -> StoreResult<()> {
        if self.inner.plane == FamilyPlane::Trigram {
            Ok(())
        } else {
            Err(StoreError::Incompatible(format!(
                "segments live in the trigram store, not {}",
                self.inner.plane.as_str()
            )))
        }
    }
}

#[cfg(test)]
thread_local! {
    static TOUCH_TRANSACTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_touch_transactions() -> usize {
    TOUCH_TRANSACTIONS.with(|count| count.replace(0))
}

const DELETE_UNREFERENCED_SQL: &str =
    "DELETE FROM blob_payloads WHERE full_key = ?1 AND ref_epoch < ?2";

impl FamilyStoreReader {
    /// Opens an existing store read-only. A store that does not exist is
    /// reported as `None`; a reader never creates one.
    pub fn open_existing(
        storage: &Path,
        family: &str,
        plane: FamilyPlane,
    ) -> StoreResult<Option<Self>> {
        let path = plane_path(storage, family, plane)?;
        if !path.is_file() {
            return Ok(None);
        }
        let inner = shared_inner(family, plane, &path, false)?;
        Ok(Some(Self { inner }))
    }

    pub fn plane(&self) -> FamilyPlane {
        self.inner.plane
    }

    pub fn get(&self, key: &FamilyKey) -> StoreResult<Option<Vec<u8>>> {
        get(&self.inner, key)
    }

    pub fn contains(&self, key: &FamilyKey) -> StoreResult<bool> {
        contains(&self.inner, key)
    }

    pub fn usage(&self) -> StoreResult<StoreUsage> {
        usage(&self.inner)
    }
}

/// One stored payload row, for the sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredRow {
    pub key: FamilyKey,
    pub payload_bytes: u64,
    pub ref_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentState {
    Building,
    Durable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentRow {
    pub segment_id: [u8; 32],
    pub state: SegmentState,
    pub byte_len: u64,
    pub ref_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentDeletion {
    Deleted,
    Retained,
    FileBusy,
}

/// Steps inside a segment deletion, all taken while the store's write lock
/// is held: the row is deleted (not yet committed), then the file is
/// unlinked, then the deletion commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentDeletionStep {
    RowDeleted,
    FileUnlinked,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreUsage {
    pub rows: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
    pub freelist_pages: u64,
}

fn shared_inner(
    family: &str,
    plane: FamilyPlane,
    path: &Path,
    writable: bool,
) -> StoreResult<Arc<StoreInner>> {
    let mut stores = lock(open_stores());
    stores.retain(|_, store| store.strong_count() > 0);
    if let Some(existing) = stores.get(path).and_then(Weak::upgrade) {
        if existing.plane != plane {
            return Err(StoreError::Incompatible(format!(
                "{} is already open as the {} plane",
                path.display(),
                existing.plane.as_str()
            )));
        }
        return Ok(existing);
    }
    let connection = if writable {
        TrackedConnection::open(path, SqliteStore::BlobStore)?
    } else {
        // No CREATE flag: a reader must not bring a store into existence.
        TrackedConnection::open_path_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
            SqliteStore::BlobStore,
        )?
    };
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    let inner = Arc::new(StoreInner {
        family: family.to_owned(),
        plane,
        path: path.to_path_buf(),
        connection: Mutex::new(connection),
        schema_ready: Mutex::new(false),
    });
    if !writable {
        verify_existing_schema(&inner)?;
    }
    stores.insert(path.to_path_buf(), Arc::downgrade(&inner));
    Ok(inner)
}

fn ensure_schema(inner: &StoreInner) -> StoreResult<()> {
    let mut ready = lock(&inner.schema_ready);
    if *ready {
        return Ok(());
    }
    let mut connection = lock(&inner.connection);
    // auto_vacuum only takes effect before the first table exists, and it must
    // be set before WAL is enabled on a fresh file.
    let has_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'",
        [],
        |row| row.get(0),
    )?;
    if has_tables == 0 {
        connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    }
    retry_while_busy(Duration::from_millis(BUSY_TIMEOUT_MS), || {
        connection.pragma_update(None, "journal_mode", "WAL")
    })?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(STORE_SCHEMA)?;
    tx.execute(
        "INSERT OR IGNORE INTO store_meta (singleton, schema_version, plane, gc_epoch)
         VALUES (1, ?1, ?2, 0)",
        params![STORE_SCHEMA_VERSION, inner.plane.as_str()],
    )?;
    check_meta(&tx, inner.plane)?;
    tx.commit()?;
    *ready = true;
    Ok(())
}

fn verify_existing_schema(inner: &StoreInner) -> StoreResult<()> {
    let connection = lock(&inner.connection);
    check_meta(&connection, inner.plane)
}

fn check_meta(connection: &rusqlite::Connection, plane: FamilyPlane) -> StoreResult<()> {
    let meta: Option<(i64, String)> = connection
        .query_row(
            "SELECT schema_version, plane FROM store_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match meta {
        Some((version, stored_plane))
            if version == STORE_SCHEMA_VERSION && stored_plane == plane.as_str() =>
        {
            Ok(())
        }
        Some((version, stored_plane)) => Err(StoreError::Incompatible(format!(
            "store is schema {version} for the {stored_plane} plane; expected schema {STORE_SCHEMA_VERSION} for {}",
            plane.as_str()
        ))),
        None => Err(StoreError::Incompatible(
            "store has no metadata row".to_string(),
        )),
    }
}

fn read_epoch(connection: &rusqlite::Connection) -> StoreResult<u64> {
    let epoch: i64 = connection.query_row(
        "SELECT gc_epoch FROM store_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(epoch.max(0) as u64)
}

fn get(inner: &StoreInner, key: &FamilyKey) -> StoreResult<Option<Vec<u8>>> {
    if key.plane() != inner.plane {
        return Err(StoreError::PlaneMismatch {
            store: inner.plane,
            key: key.plane(),
        });
    }
    let row: Option<(Vec<u8>, Vec<u8>, i64)> = lock(&inner.connection)
        .query_row(
            "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
            params![key.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((payload, digest, schema)) = row else {
        return Ok(None);
    };
    if digest.as_slice() != blake3::hash(&payload).as_bytes()
        || schema != i64::from(inner.plane.payload_schema())
    {
        log::warn!(
            "family store {} rejected a committed payload for key {key} that fails its digest or schema",
            inner.path.display()
        );
        return Ok(None);
    }
    Ok(Some(payload))
}

fn contains(inner: &StoreInner, key: &FamilyKey) -> StoreResult<bool> {
    if key.plane() != inner.plane {
        return Ok(false);
    }
    Ok(lock(&inner.connection)
        .query_row(
            "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
            params![key.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn usage(inner: &StoreInner) -> StoreResult<StoreUsage> {
    let connection = lock(&inner.connection);
    let (rows, payload_bytes): (u64, u64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let page_count: u64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let page_size: u64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let freelist_pages: u64 =
        connection.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    Ok(StoreUsage {
        rows,
        payload_bytes,
        file_bytes: page_count.saturating_mul(page_size),
        freelist_pages,
    })
}

fn read_segment_row(
    connection: &rusqlite::Connection,
    segment_id: &[u8; 32],
) -> StoreResult<Option<SegmentRow>> {
    Ok(connection
        .query_row(
            "SELECT segment_id, state, byte_len, ref_epoch FROM segments WHERE segment_id = ?1",
            params![segment_id.as_slice()],
            segment_row_from,
        )
        .optional()?
        .flatten())
}

fn segment_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<SegmentRow>> {
    let id: Vec<u8> = row.get(0)?;
    let state: String = row.get(1)?;
    let Ok(segment_id) = <[u8; 32]>::try_from(id) else {
        return Ok(None);
    };
    Ok(Some(SegmentRow {
        segment_id,
        state: if state == "durable" {
            SegmentState::Durable
        } else {
            SegmentState::Building
        },
        byte_len: row.get::<_, i64>(2)?.max(0) as u64,
        ref_epoch: row.get::<_, i64>(3)?.max(0) as u64,
    }))
}

pub(crate) fn validate_family(family: &str) -> StoreResult<()> {
    if family.is_empty() || family == "." || family == ".." || family.contains(['/', '\\', '\0']) {
        return Err(StoreError::InvalidFamily(family.to_owned()));
    }
    Ok(())
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

pub fn parse_hex32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(dir: &Path) -> StoreWriteAccess {
        StoreWriteAccess::for_registered_view(dir, "family")
    }

    fn trigram_key(bytes: &[u8]) -> FamilyKey {
        TrigramKey {
            content: ContentHash::of(bytes),
            policy: TrigramPolicy {
                max_file_size: 1024,
            },
        }
        .family_key()
    }

    #[test]
    fn a_conflicting_payload_for_one_key_is_rejected_and_the_stored_payload_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = FamilyStore::open(&access(dir.path()), FamilyPlane::Trigram).unwrap();
        let key = trigram_key(b"source");
        assert!(matches!(
            store.put_or_touch(&key, b"first").unwrap(),
            PutOrTouch::Inserted { .. }
        ));
        let error = store.put_or_touch(&key, b"second").unwrap_err();
        assert!(matches!(error, StoreError::ConflictingPayload(k) if k == key));
        assert_eq!(store.get(&key).unwrap().as_deref(), Some(&b"first"[..]));
        assert!(matches!(
            store.put_or_touch(&key, b"first").unwrap(),
            PutOrTouch::Reused { .. }
        ));
    }

    #[test]
    fn a_touch_after_the_epoch_bump_protects_the_row_from_the_conditional_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = FamilyStore::open(&access(dir.path()), FamilyPlane::Trigram).unwrap();
        let key = trigram_key(b"old");
        store.put_or_touch(&key, b"payload").unwrap();
        let sweep_epoch = store.raise_epoch(1).unwrap();
        let touched = store.touch(&[key]).unwrap();
        assert_eq!(touched.touched, 1);
        assert_eq!(touched.ref_epoch, sweep_epoch);
        assert!(!store
            .delete_if_unreferenced_since(&key, sweep_epoch)
            .unwrap());
        assert!(store.contains(&key).unwrap());
        store.raise_epoch(sweep_epoch + 1).unwrap();
        assert!(store
            .delete_if_unreferenced_since(&key, sweep_epoch + 1)
            .unwrap());
        assert_eq!(store.touch(&[key]).unwrap().missing, vec![key]);
    }

    #[test]
    fn stores_are_incremental_auto_vacuum_and_share_one_connection_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = FamilyStore::open(&access(dir.path()), FamilyPlane::Semantic).unwrap();
        let second = FamilyStore::open(&access(dir.path()), FamilyPlane::Semantic).unwrap();
        assert!(Arc::ptr_eq(&first.inner, &second.inner));
        assert_eq!(first.auto_vacuum_mode().unwrap(), 2);
        let reader = FamilyStoreReader::open_existing(dir.path(), "family", FamilyPlane::Semantic)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first.inner, &reader.inner));
    }

    #[test]
    fn a_reader_never_creates_a_store() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            FamilyStoreReader::open_existing(dir.path(), "family", FamilyPlane::Callgraph)
                .unwrap()
                .is_none()
        );
        assert!(!dir.path().join("blobs").exists());
    }

    #[test]
    fn keys_carry_the_producer_and_refuse_the_wrong_plane() {
        let a = trigram_key(b"same bytes");
        let b = TrigramKey {
            content: ContentHash::of(b"same bytes"),
            policy: TrigramPolicy {
                max_file_size: 2048,
            },
        }
        .family_key();
        assert_ne!(a, b, "a policy change must create new keys");
        let dir = tempfile::tempdir().unwrap();
        let store = FamilyStore::open(&access(dir.path()), FamilyPlane::Semantic).unwrap();
        assert!(matches!(
            store.put_or_touch(&a, b"x"),
            Err(StoreError::PlaneMismatch { .. })
        ));
    }
}
