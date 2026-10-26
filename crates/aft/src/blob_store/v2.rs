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

use std::collections::{HashMap, HashSet};
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
/// Layout of a v3 symbol-plane payload (`views::symbol_store`). The symbol
/// plane exists only in v3; no v2 store is ever opened for it.
pub const SYMBOL_PAYLOAD_SCHEMA: u32 = 1;

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
    /// Parsed symbols of one file, keyed by content, language and the
    /// extraction schema. A v3-only plane: it has no manifest readiness and
    /// no v2 store, so it is deliberately absent from [`Self::ALL`], which
    /// the manifest, v2 GC, eviction and per-entry code iterate. The v3
    /// plane-row sweep covers it through [`Self::V3_STORES`].
    Symbol,
}

impl FamilyPlane {
    /// The planes a manifest entry, a v2 family store and the v2 GC know.
    /// Never add [`Self::Symbol`] here: `EntryPlanes::get_mut` has no field
    /// for it and panics.
    pub const ALL: [Self; 3] = [Self::Trigram, Self::Semantic, Self::Callgraph];
    /// Every plane that can have a store under `blobs/v3/<family>/`.
    pub const V3_STORES: [Self; 4] = [Self::Trigram, Self::Semantic, Self::Callgraph, Self::Symbol];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigram => "trigram",
            Self::Semantic => "semantic",
            Self::Callgraph => "callgraph",
            Self::Symbol => "symbol",
        }
    }

    pub(crate) const fn payload_schema(self) -> u32 {
        match self {
            Self::Trigram => TRIGRAM_PAYLOAD_SCHEMA,
            Self::Semantic => crate::blob_store::SEMANTIC_PAYLOAD_SCHEMA,
            Self::Callgraph => crate::blob_store::CALLGRAPH_PAYLOAD_SCHEMA,
            Self::Symbol => SYMBOL_PAYLOAD_SCHEMA,
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
        Self(*crate::views::io::source_hash(bytes).as_bytes())
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
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = put_or_touch_in(&tx, self.inner.plane, key, payload)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Reads the store's deletion epoch and, in the same write transaction,
    /// stores the payload `build` names for it, like [`Self::put_or_touch`].
    /// No row can be deleted between the read and the put. Returns `None`,
    /// storing nothing, when the store has no valid deletion epoch or `build`
    /// declines.
    pub(crate) fn put_at_deletion_epoch(
        &self,
        build: impl FnOnce(&DeletionEpoch) -> Option<(FamilyKey, Vec<u8>)>,
    ) -> StoreResult<Option<PutOrTouch>> {
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(epoch) = deletion_epoch(&tx)? else {
            return Ok(None);
        };
        let Some((key, payload)) = build(&epoch) else {
            return Ok(None);
        };
        self.ensure_plane(&key)?;
        let outcome = put_or_touch_in(&tx, self.inner.plane, &key, &payload)?;
        tx.commit()?;
        Ok(Some(outcome))
    }

    /// The store's current deletion epoch; see [`DeletionEpoch`].
    pub fn deletion_epoch(&self) -> StoreResult<Option<DeletionEpoch>> {
        read_deletion_epoch(&self.inner)
    }

    /// Replaces the store's deletion epoch counter with a fresh one under a
    /// new generation, as setup does for a missing or damaged counter.
    #[cfg(test)]
    pub(crate) fn rebuild_deletion_epoch_for_test(&self) {
        let mut connection = lock(&self.inner.connection);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        replace_deletion_epoch(&tx).unwrap();
        tx.commit().unwrap();
    }
}

/// [`FamilyStore::put_or_touch`] inside the caller's IMMEDIATE transaction.
/// A conflicting payload returns an error and the caller's transaction rolls
/// back when dropped.
fn put_or_touch_in(
    tx: &rusqlite::Transaction<'_>,
    plane: FamilyPlane,
    key: &FamilyKey,
    payload: &[u8],
) -> StoreResult<PutOrTouch> {
    let digest = blake3::hash(payload);
    let quarantined = tx
        .query_row(
            "SELECT 1 FROM blob_quarantine WHERE full_key = ?1",
            params![key.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if quarantined {
        return Ok(PutOrTouch::Quarantined);
    }
    let epoch = read_epoch(tx)?;
    let existing: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT payload_digest, ref_epoch FROM blob_payloads WHERE full_key = ?1",
            params![key.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let outcome = match existing {
        Some((stored, ref_epoch)) if stored.as_slice() == digest.as_bytes() => {
            // Rewriting an unchanged epoch can rebuild payload overflow pages.
            // Still compare the digest before accepting any reuse.
            if ref_epoch < epoch as i64 {
                tx.execute(
                    "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
                    params![key.as_bytes().as_slice(), epoch as i64],
                )?;
            }
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
                    i64::from(plane.payload_schema()),
                    epoch as i64,
                    now_ms() as i64,
                ],
            )?;
            PutOrTouch::Inserted { ref_epoch: epoch }
        }
    };
    Ok(outcome)
}

impl FamilyStore {
    /// A bounded covering-index check for possible garbage. An unmarked key
    /// or an incomplete scan returns true; scan errors propagate. Only a
    /// complete scan with no unmarked keys may suppress a deleting pass.
    pub(crate) fn may_have_unmarked(
        &self,
        marked: &std::collections::BTreeSet<[u8; 32]>,
        deadline: Option<std::time::Instant>,
    ) -> StoreResult<bool> {
        let connection = lock(&self.inner.connection);
        let mut statement =
            connection.prepare("SELECT full_key FROM blob_payloads INDEXED BY blob_membership")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if deadline.is_some_and(|end| std::time::Instant::now() >= end) {
                return Ok(true);
            }
            let key: Vec<u8> = row.get(0)?;
            let key: [u8; 32] = key
                .try_into()
                .map_err(|_| StoreError::Incompatible("invalid stored key".into()))?;
            if !marked.contains(&key) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Stamps the current epoch on stored keys and reports missing keys. A
    /// covering-index read avoids opening a writer when every epoch is current.
    pub fn touch(&self, keys: &[FamilyKey]) -> StoreResult<TouchReport> {
        for key in keys {
            self.ensure_plane(key)?;
        }
        fn inspect(
            connection: &rusqlite::Connection,
            keys: &[FamilyKey],
        ) -> StoreResult<(TouchReport, Vec<FamilyKey>)> {
            let epoch = read_epoch(connection)?;
            let mut report = TouchReport {
                ref_epoch: epoch,
                ..TouchReport::default()
            };
            let mut outdated = Vec::new();
            let mut lookup = connection.prepare(
                "SELECT ref_epoch FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            )?;
            for key in keys {
                let stored: Option<i64> = lookup
                    .query_row(params![key.as_bytes().as_slice()], |row| row.get(0))
                    .optional()?;
                match stored {
                    Some(stored) => {
                        if stored < epoch as i64 {
                            outdated.push(*key);
                        }
                        report.touched += 1;
                    }
                    None => report.missing.push(*key),
                }
            }
            Ok((report, outdated))
        }
        let mut connection = lock(&self.inner.connection);
        let read = connection.transaction()?;
        let (report, outdated) = inspect(&read, keys)?;
        read.commit()?;
        if outdated.is_empty() {
            return Ok(report);
        }
        // Another process may advance GC or remove a row between transactions.
        // Recheck presence and the epoch after taking the write lock.
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        #[cfg(test)]
        TOUCH_TRANSACTIONS.with(|count| count.set(count.get() + 1));
        let (report, outdated) = inspect(&tx, keys)?;
        let mut written = 0;
        {
            let mut update = tx.prepare(
                "UPDATE blob_payloads SET ref_epoch = ?2 WHERE full_key = ?1 AND ref_epoch < ?2",
            )?;
            for key in outdated {
                written +=
                    update.execute(params![key.as_bytes().as_slice(), report.ref_epoch as i64])?;
            }
        }
        tx.commit()?;
        crate::gc::family::record_stale_rows(&self.inner.path, &self.inner.family, written as u64);
        if self.plane() == FamilyPlane::Semantic {
            use std::sync::atomic::Ordering;
            crate::views::io::WORK
                .semantic_touch_transactions
                .fetch_add(1, Ordering::Relaxed);
            crate::views::io::WORK
                .semantic_touch_rows_written
                .fetch_add(written as u64, Ordering::Relaxed);
        }
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
        // ref_epoch follows payload in the WITHOUT ROWID record, so reading it
        // from the table visits overflow pages even though length(BLOB) does not.
        // Scan lengths in place and fetch epochs from the covering index. A
        // primary-key lookup of the payload row can also load its whole record
        // while comparing keys, so the length scan must not become point probes.
        let mut statement = connection.prepare(
            "SELECT p.full_key, length(p.payload),
                    (SELECT b.ref_epoch FROM blob_payloads AS b INDEXED BY blob_membership
                     WHERE b.full_key = p.full_key) AS epoch
             FROM blob_payloads AS p NOT INDEXED
             ORDER BY epoch ASC, p.full_key ASC LIMIT ?1",
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

    /// Reclaims one bounded batch of eligible free pages.
    pub fn incremental_vacuum(&self) -> StoreResult<()> {
        self.reclaim_space_bounded(None)?;
        Ok(())
    }

    /// Reclaims a bounded batch, returning false if eligible work remains.
    /// Uses the store's own connection to preserve SQLite's file locks. Normal
    /// WAL checkpoints eventually shorten the main file; reclamation never
    /// forces a potentially unbounded TRUNCATE checkpoint of an existing WAL.
    pub fn reclaim_space(&self) -> StoreResult<bool> {
        Ok(!self.reclaim_space_bounded(None)?.deferred)
    }

    pub(crate) fn reclaim_space_bounded(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> StoreResult<super::reclaim::ReclaimReport> {
        let connection = lock(&self.inner.connection);
        let mut report = super::reclaim::reclaim(&connection, deadline, &|| false)?;
        checkpoint_after_reclaim(&self.inner, &connection, &mut report)?;
        Ok(report)
    }

    /// Returns up to `max_pages` free pages whatever their share of the
    /// file, on the store's own connection, in bounded incremental-vacuum
    /// steps (never a full `VACUUM`). See
    /// [`super::reclaim::reclaim_all_free_pages`].
    pub(crate) fn reclaim_all_free_pages(
        &self,
        deadline: Option<std::time::Instant>,
        max_pages: u64,
    ) -> StoreResult<super::reclaim::ReclaimReport> {
        let connection = lock(&self.inner.connection);
        let mut report = super::reclaim::reclaim_all_free_pages(&connection, deadline, max_pages)?;
        checkpoint_after_reclaim(&self.inner, &connection, &mut report)?;
        Ok(report)
    }

    pub fn usage(&self) -> StoreResult<StoreUsage> {
        usage(&self.inner)
    }

    /// Sets up the store's payload totals (see [`TOTALS_TABLE_SQL`]) when they
    /// are missing or not trustworthy, so later exact payload totals
    /// read one row instead of every row. Setup reads every row once and
    /// holds the store's write lock until done, so only disk-limit
    /// maintenance calls it. It waits at most [`TOTALS_SETUP_BUSY_WAIT`] for
    /// another writer and otherwise leaves the store for a later pass; a
    /// setup still running at `deadline` is rolled back and not tried again
    /// in this process.
    pub(crate) fn ensure_payload_totals(
        &self,
        deadline: std::time::Instant,
    ) -> StoreResult<PayloadTotals> {
        let path = self.inner.path.clone();
        if lock(slow_totals_setups()).contains(&path) {
            return Ok(PayloadTotals::GaveUp);
        }
        let mut connection = lock(&self.inner.connection);
        if exact_totals(&connection)?.is_some() {
            return Ok(PayloadTotals::Present);
        }
        let started = std::time::Instant::now();
        let timeout: u64 = connection.pragma_query_value(None, "busy_timeout", |r| r.get(0))?;
        connection.busy_timeout(TOTALS_SETUP_BUSY_WAIT)?;
        let outcome = set_up_payload_totals(&mut connection, deadline);
        connection.busy_timeout(Duration::from_millis(timeout))?;
        match outcome {
            Ok(()) => {
                // Setup seeded the totals with one sum over every row.
                count_logical_aggregate();
                Ok(PayloadTotals::Built {
                    elapsed: started.elapsed(),
                })
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Ok(PayloadTotals::Busy)
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::OperationInterrupted =>
            {
                lock(slow_totals_setups()).insert(path);
                Ok(PayloadTotals::GaveUp)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// An upper bound on logical payload bytes, without scanning payload rows.
    /// SQLite's page count includes the connection's current WAL snapshot, not
    /// just pages already checkpointed into the main database file.
    pub(crate) fn payload_bytes_upper_bound(&self) -> StoreResult<u64> {
        let connection = lock(&self.inner.connection);
        let page_count: u64 =
            connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
        let page_size: u64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        Ok(page_count.saturating_mul(page_size))
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
    static LOGICAL_AGGREGATES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_touch_transactions() -> usize {
    TOUCH_TRANSACTIONS.with(|count| count.replace(0))
}

#[cfg(test)]
pub(crate) fn take_logical_aggregates() -> usize {
    LOGICAL_AGGREGATES.with(|count| count.replace(0))
}

const DELETE_UNREFERENCED_SQL: &str =
    "DELETE FROM blob_payloads WHERE full_key = ?1 AND ref_epoch < ?2";

/// The exact row count and payload bytes of `blob_payloads`, in a one-row
/// table that SQLite triggers keep current on every insert, delete and
/// payload update. Triggers live in the store's schema, so SQLite runs them
/// for every writer, also for AFT releases that predate them, and the total
/// stays exact whichever binary wrote last. Epoch-only updates fire none of
/// them. Stores get the table from disk-limit maintenance
/// ([`FamilyStore::ensure_payload_totals`]), never on open; a store without
/// it, or with any part of it missing or altered, is summed row by row.
const TOTALS_TABLE_SQL: &str = "CREATE TABLE payload_totals (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    row_count INTEGER NOT NULL,
    payload_bytes INTEGER NOT NULL
)";
const TOTALS_SEED_SQL: &str = "INSERT INTO payload_totals (singleton, row_count, payload_bytes)
    SELECT 1, COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads";
/// The triggers by name, with the exact text SQLite records for each; a
/// trigger whose recorded text differs is treated as missing.
const TOTALS_TRIGGERS: [(&str, &str); 3] = [
    (
        "payload_totals_insert",
        "CREATE TRIGGER payload_totals_insert AFTER INSERT ON blob_payloads BEGIN
    UPDATE payload_totals SET row_count = row_count + 1,
        payload_bytes = payload_bytes + length(NEW.payload) WHERE singleton = 1;
END",
    ),
    (
        "payload_totals_delete",
        "CREATE TRIGGER payload_totals_delete AFTER DELETE ON blob_payloads BEGIN
    UPDATE payload_totals SET row_count = row_count - 1,
        payload_bytes = payload_bytes - length(OLD.payload) WHERE singleton = 1;
END",
    ),
    (
        "payload_totals_update",
        "CREATE TRIGGER payload_totals_update AFTER UPDATE OF payload ON blob_payloads BEGIN
    UPDATE payload_totals SET
        payload_bytes = payload_bytes - length(OLD.payload) + length(NEW.payload)
        WHERE singleton = 1;
END",
    ),
];
/// How long setup waits for another writer before leaving the store for a
/// later pass.
pub(crate) const TOTALS_SETUP_BUSY_WAIT: Duration = Duration::from_millis(250);
/// VM steps between deadline checks while the totals are set up.
const TOTALS_SETUP_PROGRESS_STEPS: std::ffi::c_int = 10_000;

/// What [`FamilyStore::ensure_payload_totals`] found or did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PayloadTotals {
    /// The store already had complete totals.
    Present,
    /// This call set them up.
    Built { elapsed: Duration },
    /// Another connection kept the write lock; nothing was done.
    Busy,
    /// A setup did not finish by its deadline in this process, so none is
    /// tried again until the process restarts.
    GaveUp,
}

/// The store's exact row count and payload bytes from its totals table,
/// or `None` unless the table holds exactly one well-formed row and all
/// three triggers exist with their expected text. A caller that uses the
/// value reads it within one transaction, so the triggers and the row come
/// from one snapshot.
fn exact_totals(connection: &rusqlite::Connection) -> StoreResult<Option<(u64, u64)>> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql FROM sqlite_schema
         WHERE (type = 'trigger' AND tbl_name = 'blob_payloads')
            OR (type = 'table' AND name = 'payload_totals')",
    )?;
    let schema = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_table = schema.iter().any(|(kind, _, _)| kind == "table");
    let has_triggers = TOTALS_TRIGGERS.iter().all(|(name, sql)| {
        schema.iter().any(|(kind, found, text)| {
            kind == "trigger" && found == name && text.as_deref() == Some(*sql)
        })
    });
    if !has_table || !has_triggers {
        return Ok(None);
    }
    let rows = connection
        .prepare("SELECT singleton, row_count, payload_bytes FROM payload_totals LIMIT 2")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(match rows.as_slice() {
        [(1, count, bytes)] if *count >= 0 && *bytes >= 0 => Some((*count as u64, *bytes as u64)),
        _ => None,
    })
}

/// Store files whose totals setup ran out of time in this process.
fn slow_totals_setups() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static SLOW: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    SLOW.get_or_init(Mutex::default)
}

/// Replaces whatever totals table and triggers the store has with fresh
/// ones, seeded from one scan, in one IMMEDIATE transaction: no write can
/// land between the scan and the triggers. Interrupted, and so rolled back,
/// once `deadline` passes.
fn set_up_payload_totals(
    connection: &mut rusqlite::Connection,
    deadline: std::time::Instant,
) -> rusqlite::Result<()> {
    unsafe extern "C" fn past_deadline(context: *mut std::ffi::c_void) -> std::ffi::c_int {
        let deadline = unsafe { &*context.cast::<std::time::Instant>() };
        std::ffi::c_int::from(std::time::Instant::now() >= *deadline)
    }
    // The caller holds the store's connection lock throughout.
    let db = unsafe { connection.handle() };
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut deadline = Box::new(deadline);
    // Installed only around the setup statements, so it can never interrupt
    // the commit or the rollback, and removed before the box is dropped.
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            db,
            TOTALS_SETUP_PROGRESS_STEPS,
            Some(past_deadline),
            std::ptr::from_mut(deadline.as_mut()).cast(),
        );
    }
    let set_up = (|| {
        for (name, _) in TOTALS_TRIGGERS {
            tx.execute_batch(&format!("DROP TRIGGER IF EXISTS {name}"))?;
        }
        tx.execute_batch("DROP TABLE IF EXISTS payload_totals")?;
        tx.execute_batch(TOTALS_TABLE_SQL)?;
        tx.execute(TOTALS_SEED_SQL, [])?;
        for (_, sql) in TOTALS_TRIGGERS {
            tx.execute_batch(sql)?;
        }
        Ok::<_, rusqlite::Error>(())
    })();
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(db, 0, None, std::ptr::null_mut());
    }
    drop(deadline);
    // On an error the transaction rolls back when dropped.
    set_up?;
    tx.commit()
}

/// A count of the rows deleted from `blob_payloads` since this counter was
/// created, kept in a one-row
/// table by a SQLite trigger. The trigger lives in the store's schema, so it
/// also counts deletes by AFT releases that predate it (their GC and
/// disk-limit SQL). No AFT write path rewrites the payload of an existing
/// key: `put_or_touch` inserts only an absent key and otherwise updates only
/// `ref_epoch`, so a row can change its payload only by being deleted and
/// inserted again, and every such change moves this count.
///
/// The row also holds a random generation, drawn whenever the counter is
/// (re)created, so a counter rebuilt after damage never repeats an earlier
/// (generation, count) pair. A value read from a store therefore names one
/// state of "which rows were ever deleted", and something recorded at one
/// value is known to hold only while the store still reads that value.
///
/// The counter is set up when a writer first opens the store. Any missing or
/// altered part reads as no epoch, which callers treat as "nothing can be
/// vouched for", never as an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DeletionEpoch {
    generation: [u8; 16],
    count: u64,
}

impl DeletionEpoch {
    /// The generation then the count, little-endian: the bytes a key that
    /// depends on this epoch hashes.
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..16].copy_from_slice(&self.generation);
        bytes[16..].copy_from_slice(&self.count.to_le_bytes());
        bytes
    }
}

const DELETIONS_TABLE: (&str, &str) = (
    "payload_deletions",
    "CREATE TABLE payload_deletions (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation BLOB NOT NULL,
    count INTEGER NOT NULL
)",
);
const DELETIONS_SEED_SQL: &str =
    "INSERT INTO payload_deletions (singleton, generation, count) VALUES (1, randomblob(16), 0)";
const DELETIONS_TRIGGER: (&str, &str) = (
    "payload_deletions_count",
    "CREATE TRIGGER payload_deletions_count AFTER DELETE ON blob_payloads BEGIN
    UPDATE payload_deletions SET count = count + 1 WHERE singleton = 1;
END",
);

/// The deletion epoch, or `None` unless the table and the trigger exist with
/// their exact text and the table holds exactly one well-formed row. Callers
/// read it inside a transaction so the schema and the row come from one
/// snapshot.
fn deletion_epoch(connection: &rusqlite::Connection) -> StoreResult<Option<DeletionEpoch>> {
    let mut statement = connection.prepare(
        "SELECT name, sql FROM sqlite_schema
         WHERE (type = 'table' AND name = ?1) OR (type = 'trigger' AND name = ?2)",
    )?;
    let schema = statement
        .query_map(params![DELETIONS_TABLE.0, DELETIONS_TRIGGER.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let exact = |(name, sql): (&str, &str)| {
        schema
            .iter()
            .any(|(found, text)| found == name && text.as_deref() == Some(sql))
    };
    if !exact(DELETIONS_TABLE) || !exact(DELETIONS_TRIGGER) {
        return Ok(None);
    }
    let rows = connection
        .prepare("SELECT singleton, generation, count FROM payload_deletions LIMIT 2")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>();
    // A value of the wrong type is damage, not a store error.
    let Ok(rows) = rows else {
        return Ok(None);
    };
    Ok(match rows.as_slice() {
        [(1, generation, count)] if *count >= 0 => <[u8; 16]>::try_from(generation.as_slice())
            .ok()
            .map(|generation| DeletionEpoch {
                generation,
                count: *count as u64,
            }),
        _ => None,
    })
}

fn read_deletion_epoch(inner: &StoreInner) -> StoreResult<Option<DeletionEpoch>> {
    let mut connection = lock(&inner.connection);
    let tx = connection.transaction()?;
    let epoch = deletion_epoch(&tx)?;
    tx.commit()?;
    Ok(epoch)
}

/// Drops whatever deletion counter the store has and creates a fresh one
/// with a new generation, inside the caller's IMMEDIATE transaction.
fn replace_deletion_epoch(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(&format!("DROP TRIGGER IF EXISTS {}", DELETIONS_TRIGGER.0))?;
    tx.execute_batch(&format!("DROP TABLE IF EXISTS {}", DELETIONS_TABLE.0))?;
    tx.execute_batch(DELETIONS_TABLE.1)?;
    tx.execute(DELETIONS_SEED_SQL, [])?;
    tx.execute_batch(DELETIONS_TRIGGER.1)
}

/// Sets up the deletion counter when it is missing or damaged, in one
/// IMMEDIATE transaction that checks again under the write lock, so a
/// counter another process just created is kept. Failure only leaves the
/// store without an epoch; the caller does not report it.
fn set_up_deletion_epoch(connection: &mut rusqlite::Connection) -> StoreResult<()> {
    let present = {
        let tx = connection.transaction()?;
        let present = deletion_epoch(&tx)?.is_some();
        tx.commit()?;
        present
    };
    if present {
        return Ok(());
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if deletion_epoch(&tx)?.is_none() {
        replace_deletion_epoch(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

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

    /// The store's current deletion epoch; see [`DeletionEpoch`].
    pub fn deletion_epoch(&self) -> StoreResult<Option<DeletionEpoch>> {
        read_deletion_epoch(&self.inner)
    }

    /// In one read transaction: reads the store's deletion epoch, asks `keys`
    /// which keys to probe for it, and returns the epoch with the probed keys
    /// that are stored. Keys are probed in key order through the payload-free
    /// membership index, so neighbouring probes reuse the same cached pages. `None` when the
    /// store has no valid deletion epoch.
    pub(crate) fn contains_many_at_deletion_epoch(
        &self,
        keys: impl FnOnce(&DeletionEpoch) -> Vec<FamilyKey>,
    ) -> StoreResult<Option<(DeletionEpoch, HashSet<FamilyKey>)>> {
        let mut connection = lock(&self.inner.connection);
        let tx = connection.transaction()?;
        let Some(epoch) = deletion_epoch(&tx)? else {
            return Ok(None);
        };
        let mut sorted = keys(&epoch)
            .into_iter()
            .filter(|key| key.plane() == self.inner.plane)
            .collect::<Vec<_>>();
        sorted.sort_unstable();
        sorted.dedup();
        let mut present = HashSet::new();
        {
            let mut statement = tx.prepare(
                "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            )?;
            for key in sorted {
                if statement.exists(params![key.as_bytes().as_slice()])? {
                    present.insert(key);
                }
            }
        }
        tx.commit()?;
        Ok(Some((epoch, present)))
    }

    pub fn usage(&self) -> StoreResult<StoreUsage> {
        usage(&self.inner)
    }

    /// The stored payload length and digest of `key`, read on the store's own
    /// connection without reading the payload. A v3 copy sizes its
    /// reservation with them and checks for an equal v3 row.
    pub(super) fn copy_source_identity(
        &self,
        key: &FamilyKey,
    ) -> StoreResult<Option<(u64, Vec<u8>)>> {
        self.ensure_copy_plane(key)?;
        Ok(lock(&self.inner.connection)
            .query_row(
                "SELECT length(payload), payload_digest FROM blob_payloads WHERE full_key = ?1",
                params![key.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// The stored payload, digest and `payload_schema` of `key`, unverified,
    /// on the store's own connection. A v3 copy verifies the digest itself and
    /// keeps the stored schema rather than this plane's current one.
    pub(super) fn copy_source_row(
        &self,
        key: &FamilyKey,
    ) -> StoreResult<Option<(Vec<u8>, Vec<u8>, i64)>> {
        self.ensure_copy_plane(key)?;
        Ok(lock(&self.inner.connection)
            .query_row(
                "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
                params![key.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    fn ensure_copy_plane(&self, key: &FamilyKey) -> StoreResult<()> {
        if key.plane() == self.inner.plane {
            Ok(())
        } else {
            Err(StoreError::PlaneMismatch {
                store: self.inner.plane,
                key: key.plane(),
            })
        }
    }

    /// Bytes of the store's database file that hold no data: its free pages,
    /// plus any tail of the main file past the database's last page, which
    /// returning free pages leaves in place until a checkpoint cuts it off.
    /// Reads the database header in one read transaction on the store's own
    /// connection, and the main file's length.
    pub(crate) fn unused_file_bytes(&self) -> StoreResult<u64> {
        let mut connection = lock(&self.inner.connection);
        let read = connection.transaction()?;
        let page_size: u64 = read.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let pages: u64 = read.pragma_query_value(None, "page_count", |row| row.get(0))?;
        let free: u64 = read.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
        read.commit()?;
        let main_file = fs::metadata(&self.inner.path)?.len();
        Ok(free
            .saturating_mul(page_size)
            .saturating_add(main_file.saturating_sub(pages.saturating_mul(page_size))))
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
    // Every insert and delete fires a payload-totals trigger, which makes it
    // a statement SQLite journals separately inside its transaction; with
    // temporary files those statement journals spill to disk and more than
    // triple the bytes a bulk insert writes. Kept in memory, they stay
    // small. The one temporary b-tree these connections build, for the
    // oldest-first listing's sort, is bounded by that listing's row limit.
    connection.pragma_update(None, "temp_store", "MEMORY")?;
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

/// Gives the space a reclaim freed back to the filesystem now. Returning
/// free pages shrinks the database only in the write-ahead log; the main
/// file is cut to the new size when a checkpoint copies the whole log into
/// it. SQLite checkpoints on its own only once the log reaches 1,000
/// frames, which a small reclaim on a store nobody writes to afterwards may
/// never reach, so this runs one checkpoint, on the store's own connection,
/// whenever the reclaim returned pages or the main file is still longer
/// than the database (a cut an earlier pass could not make).
///
/// The checkpoint is PASSIVE: it copies what it can and never waits for
/// readers or writers, and never calls the busy handler. A reader holding
/// an older snapshot leaves the file uncut and `checkpoint_blocked` set;
/// the next pass tries again, and until then the main file's tail counts
/// as unused bytes (see [`FamilyStoreReader::unused_file_bytes`]). FULL,
/// RESTART and TRUNCATE would instead wait on the busy handler for other
/// connections, up to the store's busy timeout, and RESTART and TRUNCATE
/// also wait for every reader to finish so the log can start over. The
/// copy is bounded by the log's current frame count: SQLite checkpoints on
/// its own at 1,000 frames, but a checkpoint a reader kept from finishing
/// can leave the log larger than that.
fn checkpoint_after_reclaim(
    inner: &StoreInner,
    connection: &rusqlite::Connection,
    report: &mut super::reclaim::ReclaimReport,
) -> StoreResult<()> {
    if report.reclaimed_pages == 0 {
        let page_size: u64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let pages: u64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
        if fs::metadata(&inner.path)?.len() <= pages.saturating_mul(page_size) {
            return Ok(());
        }
    }
    let checkpoint = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });
    match checkpoint {
        // Columns: whether it was blocked, frames in the log, frames copied.
        Ok((busy, log, copied)) => report.checkpoint_blocked |= busy != 0 || copied < log,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            report.checkpoint_blocked = true;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
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
    // Without the counter nothing is vouched for by deletion epoch, so a
    // failed setup costs only that; it never fails the open.
    if let Err(error) = set_up_deletion_epoch(&mut connection) {
        log::debug!(
            "family store {} has no deletion epoch: {error}",
            inner.path.display()
        );
    }
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
    if inner.plane == FamilyPlane::Semantic {
        crate::views::io::WORK
            .semantic_store_gets
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    if inner.plane == FamilyPlane::Semantic {
        crate::views::io::WORK
            .semantic_store_payload_bytes
            .fetch_add(payload.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
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
            "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            params![key.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn usage(inner: &StoreInner) -> StoreResult<StoreUsage> {
    let mut connection = lock(&inner.connection);
    // One read transaction, so the totals' triggers, their row and the page
    // counts all come from one snapshot.
    let read = connection.transaction()?;
    let (rows, payload_bytes) = match exact_totals(&read)? {
        Some(totals) => totals,
        None => {
            count_logical_aggregate();
            read.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
        }
    };
    let page_count: u64 = read.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let page_size: u64 = read.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let freelist_pages: u64 = read.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    read.commit()?;
    Ok(StoreUsage {
        rows,
        payload_bytes,
        file_bytes: page_count.saturating_mul(page_size),
        freelist_pages,
    })
}

/// Counts one sum over every payload row.
fn count_logical_aggregate() {
    crate::views::io::WORK
        .logical_aggregates
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    LOGICAL_AGGREGATES.with(|count| count.set(count.get() + 1));
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
mod sizing_tests;

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
    fn symbol_is_not_a_manifest_plane() {
        // `EntryPlanes::get_mut` panics for Symbol because a manifest entry
        // has no symbol state. That panic is unreachable only while every
        // list the manifest, v2 store, GC, eviction and per-entry code
        // iterates (`FamilyPlane::ALL`) excludes the v3-only symbol plane.
        assert!(!FamilyPlane::ALL.contains(&FamilyPlane::Symbol));
        assert!(FamilyPlane::V3_STORES.contains(&FamilyPlane::Symbol));
        for plane in FamilyPlane::ALL {
            assert!(FamilyPlane::V3_STORES.contains(&plane));
        }
        assert_eq!(FamilyPlane::Symbol.as_str(), "symbol");
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
    fn family_gc_retries_large_freelists_without_new_deletions_in_bounded_passes() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::views::registry::FamilyRegistry::open(dir.path(), "family").unwrap();
        let store = FamilyStore::open(&access(dir.path()), FamilyPlane::Semantic).unwrap();
        {
            // Overflow pages model a large deleted payload without allocating
            // a matching Rust buffer. Keep the real production reclaim policy.
            let connection = lock(&store.inner.connection);
            connection
                .execute_batch(
                    "CREATE TABLE reclaim_fixture (data BLOB);
                INSERT INTO reclaim_fixture VALUES (zeroblob(335544320));
                DELETE FROM reclaim_fixture;",
                )
                .unwrap();
        }
        let initial = store.usage().unwrap().file_bytes;
        for _ in 0..3 {
            let before = store.usage().unwrap().file_bytes;
            let report = crate::gc::family::sweep_family(
                &registry,
                None,
                crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
                None,
            )
            .unwrap();
            let after = store.usage().unwrap().file_bytes;
            assert_eq!(report.deleted_blobs, 0);
            assert_eq!(report.space_reclaim.examined_stores, 1);
            assert!(report.space_reclaim.reclaimed_pages > 0);
            assert!(report.space_reclaim.reclaimed_pages <= 4096);
            assert!(before > after);
            assert!(
                before - after <= 4102 * 4096,
                "one pass exceeded its page budget"
            );
            assert!(report.reclaim_deferred);
        }
        assert!(store.usage().unwrap().file_bytes > initial - 3 * 4102 * 4096);
        let connection = lock(&store.inner.connection);
        assert_eq!(
            connection
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, u64>(0))
                .unwrap(),
            super::super::BUSY_TIMEOUT_MS
        );
    }

    /// A store whose allocated pages already fit the byte budget skips the
    /// payload scan, but its large free list must still be returned to the
    /// file system in bounded passes, and the remaining work reported.
    #[test]
    fn family_gc_reclaims_large_freelists_when_the_store_fits_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::views::registry::FamilyRegistry::open(dir.path(), "family").unwrap();
        let store = FamilyStore::open(&access(dir.path()), FamilyPlane::Semantic).unwrap();
        {
            // 320 MiB of free pages clears the production 256 MiB threshold.
            let connection = lock(&store.inner.connection);
            connection
                .execute_batch(
                    "CREATE TABLE reclaim_fixture (data BLOB);
                INSERT INTO reclaim_fixture VALUES (zeroblob(335544320));
                DELETE FROM reclaim_fixture;",
                )
                .unwrap();
        }
        let before = store.usage().unwrap().file_bytes;
        let budget = store.payload_bytes_upper_bound().unwrap() * 2;
        let report = crate::gc::family::sweep_family(
            &registry,
            None,
            crate::gc::family::FamilySweepPolicy {
                byte_budget: budget,
            },
            None,
        )
        .unwrap();
        let after = store.usage().unwrap().file_bytes;
        assert_eq!(report.deleted_blobs, 0);
        assert_eq!(report.examined_blobs, 0, "the fitting store skips the scan");
        assert_eq!(report.space_reclaim.examined_stores, 1);
        assert!(report.space_reclaim.reclaimed_pages > 0);
        assert!(report.space_reclaim.reclaimed_pages <= 4096);
        assert!(before > after, "{before} -> {after}");
        assert!(report.reclaim_deferred, "eligible free pages remain");
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

#[cfg(test)]
pub(crate) mod read_tests;
