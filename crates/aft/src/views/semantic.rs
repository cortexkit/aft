//! Semantic plane of a per-checkout view, reusing the shared-base overlay.
//!
//! A checkout's semantic answer is the overlay the resident semantic index
//! already implements (`SemanticIndex` over a `SharedSemanticBase`), keyed by
//! view content keys instead of by one root's artifact:
//!
//! - **Base.** The runs the current generation names as `Ready`, decoded once
//!   into the family arena when the generation is admitted and shared by every
//!   view that names the same key.
//! - **Replacements.** Vectors for paths whose current content is not the
//!   base's: live edits, entries the generation still has pending, and every
//!   member after a producer (model, chunker or template) change. They come
//!   from this checkout's fill map, keyed by `(path, content)`.
//! - **Tombstones.** Base files that are superseded, deleted or no longer
//!   members. They are never scored.
//!
//! Keys hash the content, the path (the embedded text names the file) and the
//! producer, so identical content embeds once per family: two views, and two
//! sessions of one view, reuse the same key instead of calling the model again.
//! A fill looks a key up in the arena, then in the family store, and embeds
//! only true misses, claiming each one so a concurrent fill of another view
//! waits for it rather than embedding it too.
//!
//! Pending and failed work is recorded in the published manifest, so it
//! survives folds, eviction and restart; completions are admitted only for the
//! content and producer they were computed for.
//!
//! Registration is opt-in: constructing a plane changes no routing.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane, FamilyStore, PutOrTouch};
use crate::blob_store::{FullKey, SemanticKey, SEMANTIC_PRODUCER_VERSION};
use crate::refresh::{FailureClass, FailureTracker, PreparedWork};
use crate::semantic_index::{
    EmbedTextCaps, SemanticIndex, SemanticResult, ViewPayloadProducer, ViewSemanticBase,
};

use super::contracts::{PlaneAdapter, PlaneError, ViewAccess};
use super::manifest_v2::{EntryV2, ManifestV2};
use super::readiness::{
    Admission, Completion, FillMap, LiveEntries, PlaneReadiness, PlaneState, WorkItem,
};
use super::registry::ViewRegistration;
use super::segment_store::rel_path_to_os;
use super::snapshot::{ComposedGeneration, DiskState, LiveEntry, Snapshot, Source, WatcherState};
use super::RelPath;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`lock`] without waiting: `None` only while another thread holds it.
fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn plane_error(reason: impl Into<String>) -> PlaneError {
    PlaneError {
        plane: Some(FamilyPlane::Semantic),
        reason: reason.into(),
    }
}

/// Everything that decides a semantic payload: the chunker, the embedding
/// text template (including its size caps) and the model. Each is part of
/// every key, so vectors of another producer are never reused or scored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticProducer {
    pub chunker_version: String,
    pub template_version: String,
    pub model_fingerprint: String,
    pub caps: EmbedTextCaps,
}

impl SemanticProducer {
    /// This release's chunker and template for `model_fingerprint`, which the
    /// runtime derives from its embedding configuration.
    pub fn current(model_fingerprint: impl Into<String>, caps: EmbedTextCaps) -> Self {
        Self {
            chunker_version: SEMANTIC_PRODUCER_VERSION.to_owned(),
            // The caps change the embedded text, so they are template identity.
            template_version: format!(
                "{SEMANTIC_PRODUCER_VERSION};caps={}/{}/{}/{}",
                caps.signature_chars, caps.body_lines, caps.body_chars, caps.total_chars
            ),
            model_fingerprint: model_fingerprint.into(),
            caps,
        }
    }

    /// The producer string recorded in manifest headers.
    pub fn id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            &self.chunker_version,
            &self.template_version,
            &self.model_fingerprint,
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        format!("semantic-{}", hasher.finalize().to_hex())
    }

    fn full_key(&self, bytes: &[u8], rel_path: &RelPath) -> FullKey {
        self.full_key_for_summary_format(
            bytes,
            rel_path,
            crate::semantic_index::FILE_SUMMARY_BUDGET_KEY_FORMAT,
        )
    }

    fn summary_key_format(&self, summary_format: u32) -> u32 {
        if self.caps.total_chars < EmbedTextCaps::default().total_chars {
            summary_format
        } else {
            0
        }
    }

    fn full_key_for_summary_format(
        &self,
        bytes: &[u8],
        rel_path: &RelPath,
        summary_format: u32,
    ) -> FullKey {
        SemanticKey::from_bytes(
            bytes,
            rel_path.as_bytes(),
            self.chunker_version.as_str(),
            self.template_version.as_str(),
            self.model_fingerprint.as_str(),
        )
        .file_key_for_summary_format(self.summary_key_format(summary_format))
    }

    fn key_for_content(
        &self,
        content: ContentHash,
        rel_path: &RelPath,
        summary_format: u32,
    ) -> FamilyKey {
        FamilyKey::from(
            &SemanticKey::from_source_hash(
                *content.as_bytes(),
                rel_path.as_bytes(),
                self.chunker_version.as_str(),
                self.template_version.as_str(),
                self.model_fingerprint.as_str(),
            )
            .file_key_for_summary_format(self.summary_key_format(summary_format)),
        )
    }

    pub(crate) fn legacy_key(&self, bytes: &[u8], rel_path: &RelPath) -> FamilyKey {
        FamilyKey::from(&self.full_key_for_summary_format(bytes, rel_path, 0))
    }

    /// The input-addressed family key of `bytes` at `rel_path` under this
    /// producer. Model output can vary across invocations; the store keeps the
    /// first valid payload for these inputs, never replaces it with new floats.
    pub fn key(&self, bytes: &[u8], rel_path: &RelPath) -> FamilyKey {
        FamilyKey::from(&self.full_key(bytes, rel_path))
    }

    fn payload(&self) -> ViewPayloadProducer<'_> {
        ViewPayloadProducer {
            chunker_version: &self.chunker_version,
            template_version: &self.template_version,
            model_fingerprint: &self.model_fingerprint,
        }
    }

    /// Chunk vectors have their own hash domain, never a per-file blob key.
    /// Include the caps explicitly even for manually constructed producers.
    fn chunk_identity(&self) -> String {
        format!(
            "{};caps={}/{}/{}/{}",
            self.id(),
            self.caps.signature_chars,
            self.caps.body_lines,
            self.caps.body_chars,
            self.caps.total_chars
        )
    }

    fn chunk_key(&self, text: &str) -> FamilyKey {
        self.chunk_key_for_digest(&self.chunk_identity(), &blake3::hash(text.as_bytes()))
    }

    fn chunk_key_for_digest(&self, identity: &str, text_digest: &blake3::Hash) -> FamilyKey {
        let mut hash = blake3::Hasher::new();
        // A new cache format gets a new domain: immutable JSON rows must not
        // prevent compact vectors from being stored under otherwise equal inputs.
        hash.update(b"aft-semantic-chunk-vector-v2\0");
        hash.update(&(identity.len() as u64).to_le_bytes());
        hash.update(identity.as_bytes());
        hash.update(text_digest.as_bytes());
        FamilyKey::new(FamilyPlane::Semantic, *hash.finalize().as_bytes())
    }
}

/// Domain of verified-run marker keys and the first bytes of every marker
/// payload. File keys hash under `aft/blob-store/semantic/...` and chunk keys
/// under `aft-semantic-chunk-vector-v2\0`. Neither domain is a prefix of this
/// one, so a marker key can equal another row's key only through a BLAKE3
/// collision. A marker payload starts with `a`, never the version byte (1)
/// that file payloads and chunk vectors start with, so no decoder accepts it.
/// `v2` keys include the store's deletion epoch; `v1` keys, which did not,
/// are never computed and are left for GC.
const VERIFIED_RUN_DOMAIN: &[u8] = b"aft-semantic-verified-run-v2\0";

impl super::semantic_arena::ChunkNaming for SemanticProducer {
    fn chunk_key_for_text_digest(&self, text_digest: &blake3::Hash) -> FamilyKey {
        self.chunk_key_for_digest(&self.chunk_identity(), text_digest)
    }

    fn verified_run_marker(
        &self,
        payload_digest: &blake3::Hash,
        epoch: &crate::blob_store::v2::DeletionEpoch,
    ) -> FamilyKey {
        let mut hash = blake3::Hasher::new();
        hash.update(VERIFIED_RUN_DOMAIN);
        let identity = self.chunk_identity();
        hash.update(&(identity.len() as u64).to_le_bytes());
        hash.update(identity.as_bytes());
        hash.update(payload_digest.as_bytes());
        hash.update(&epoch.to_bytes());
        FamilyKey::new(FamilyPlane::Semantic, *hash.finalize().as_bytes())
    }
}

/// The payload stored under a verified-run marker: the domain, the digest of
/// the run payload it vouches for, and the deletion epoch it was checked at.
/// Only the marker's presence is read; the rest makes a marker row
/// self-describing.
fn verified_run_marker_payload(
    payload_digest: &blake3::Hash,
    epoch: &crate::blob_store::v2::DeletionEpoch,
) -> Vec<u8> {
    let mut payload = VERIFIED_RUN_DOMAIN.to_vec();
    payload.extend_from_slice(payload_digest.as_bytes());
    payload.extend_from_slice(&epoch.to_bytes());
    payload
}

/// Whether every vector of `run` is bit-equal to the chunk row of its text
/// as that row stands at deletion epoch `epoch`, judged from the arena's
/// chunk index. The index holds vectors read from, or stored as, their exact
/// chunk row, and vectors of loaded runs whose marker says they matched
/// those rows, each with the epoch it was checked at. Only entries checked
/// at `epoch` count: an older check may predate a delete and re-put of the
/// row. `run` must still hold its embedded texts. An empty run vouches for
/// nothing.
fn run_matches_chunk_rows(
    run: &crate::semantic_index::SemanticVectors,
    producer: &SemanticProducer,
    arena: &super::semantic_arena::SemanticArena,
    epoch: &crate::blob_store::v2::DeletionEpoch,
) -> bool {
    let identity = producer.chunk_identity();
    let mut entries = run.embedded_texts_and_vectors().peekable();
    entries.peek().is_some()
        && entries.all(|(text, vector)| {
            let key = producer.chunk_key_for_digest(&identity, &blake3::hash(text.as_bytes()));
            arena.verified_chunk(&key).is_some_and(|(row, checked_at)| {
                checked_at.as_ref() == Some(epoch)
                    && row.len() == vector.len()
                    && row
                        .iter()
                        .zip(vector.iter())
                        .all(|(row, vector)| row.to_bits() == vector.to_bits())
            })
        })
}

const CHUNK_VECTOR_FORMAT: u8 = 1;

/// A format byte, two length-prefixed UTF-8 fields, then raw little-endian f32s.
/// The vector occupies exactly four bytes per dimension, preserving signed zero
/// and every float bit. FamilyStore verifies the digest before decoding.
fn encode_chunk_vector(identity: &str, text: &str, vector: &[f32]) -> Result<Vec<u8>, String> {
    let identity_len = u32::try_from(identity.len())
        .map_err(|_| "semantic chunk producer identity is too long".to_string())?;
    let text_len =
        u32::try_from(text.len()).map_err(|_| "semantic chunk text is too long".to_string())?;
    let mut payload = Vec::with_capacity(9 + identity.len() + text.len() + vector.len() * 4);
    payload.push(CHUNK_VECTOR_FORMAT);
    payload.extend_from_slice(&identity_len.to_le_bytes());
    payload.extend_from_slice(identity.as_bytes());
    payload.extend_from_slice(&text_len.to_le_bytes());
    payload.extend_from_slice(text.as_bytes());
    for value in vector {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    Ok(payload)
}

fn take_chunk_field<'a>(remaining: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = u32::from_le_bytes(remaining.get(..4)?.try_into().ok()?) as usize;
    let tail = remaining.get(4..)?;
    let field = tail.get(..length)?;
    *remaining = tail.get(length..)?;
    Some(field)
}

fn decode_chunk_vector(payload: &[u8], identity: &str, text: &str) -> Option<Vec<f32>> {
    let (&format, mut remaining) = payload.split_first()?;
    if format != CHUNK_VECTOR_FORMAT {
        return None;
    }
    // The hash is an index, not proof of equality or producer compatibility.
    if take_chunk_field(&mut remaining)? != identity.as_bytes()
        || take_chunk_field(&mut remaining)? != text.as_bytes()
        || remaining.is_empty()
        || remaining.len() % 4 != 0
    {
        return None;
    }
    Some(
        remaining
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four bytes")))
            .collect(),
    )
}

/// Where a fill reads and writes semantic rows: the owner's v2 family store,
/// or the family's v3 semantic plane store for a generation published in the
/// v3 namespace. Each completed model batch is stored before requesting more
/// embeddings, sharing its v3 reservation, plane transaction and ledger flush.
pub(crate) enum SemanticStore {
    V2(FamilyStore),
    V3(
        super::snapshot::V3Route,
        std::sync::OnceLock<crate::blob_store::v3::PlaneReader>,
    ),
}

/// A v3 source with no semantic store yet: every read misses.
struct NoRuns;
impl super::semantic_arena::RunSource for NoRuns {
    fn run_payload(&self, _: &FamilyKey) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
    fn markers_at_deletion_epoch(
        &self,
        _: &mut dyn FnMut(&crate::blob_store::v2::DeletionEpoch) -> Vec<FamilyKey>,
    ) -> Option<(crate::blob_store::v2::DeletionEpoch, HashSet<FamilyKey>)> {
        None
    }
}

fn v3_store_error(error: crate::blob_store::v3::Error) -> crate::blob_store::v2::StoreError {
    crate::blob_store::v2::StoreError::Io(std::io::Error::other(error.to_string()))
}

impl SemanticStore {
    fn for_generation(
        owner: &ViewRegistration,
        generation: &ComposedGeneration,
    ) -> Result<Self, PlaneError> {
        match generation.v3_route() {
            Some(route) => Ok(Self::V3(route.clone(), std::sync::OnceLock::new())),
            None => owner
                .open_store(FamilyPlane::Semantic)
                .map(Self::V2)
                .map_err(|error| plane_error(error.to_string())),
        }
    }

    fn is_v3(&self) -> bool {
        matches!(self, Self::V3(..))
    }

    fn v3_reader<'a>(
        route: &super::snapshot::V3Route,
        opened: &'a std::sync::OnceLock<crate::blob_store::v3::PlaneReader>,
    ) -> Result<Option<&'a crate::blob_store::v3::PlaneReader>, crate::blob_store::v2::StoreError>
    {
        // A SemanticStore belongs to one fill/materialization pass. Validate
        // once, then share the handle for file and chunk lookups in that pass.
        // Do not remember an absent plane: this fill may create it later.
        if let Some(reader) = opened.get() {
            return Ok(Some(reader));
        }
        let path =
            crate::blob_store::v3::plane_path(&route.storage, &route.family, FamilyPlane::Semantic)
                .map_err(v3_store_error)?;
        if !path.exists() {
            return Ok(None);
        }
        let reader = crate::blob_store::v3::PlaneReader::open_existing(
            &route.storage,
            &route.family,
            FamilyPlane::Semantic,
        )
        .map_err(v3_store_error)?;
        Ok(Some(opened.get_or_init(|| reader)))
    }

    fn get(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, crate::blob_store::v2::StoreError> {
        match self {
            Self::V2(store) => store.get(key),
            Self::V3(route, opened) => match Self::v3_reader(route, opened)? {
                Some(reader) => reader.get(key).map_err(v3_store_error),
                None => Ok(None),
            },
        }
    }

    fn reader(&self) -> Box<dyn super::semantic_arena::RunSource> {
        match self {
            Self::V2(store) => Box::new(store.reader()),
            Self::V3(route, opened) => match Self::v3_reader(route, opened) {
                Ok(Some(reader)) => Box::new(reader.clone()),
                _ => Box::new(NoRuns),
            },
        }
    }

    #[cfg(test)]
    fn put_or_touch(
        &self,
        key: &FamilyKey,
        payload: &[u8],
    ) -> Result<PutOrTouch, crate::blob_store::v2::StoreError> {
        self.put_batch(&[(*key, payload.to_vec())])?
            .pop()
            .expect("one row")
    }

    fn put_batch(
        &self,
        rows: &[(FamilyKey, Vec<u8>)],
    ) -> Result<
        Vec<Result<PutOrTouch, crate::blob_store::v2::StoreError>>,
        crate::blob_store::v2::StoreError,
    > {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            Self::V2(store) => Ok(rows
                .iter()
                .map(|(key, payload)| store.put_or_touch(key, payload))
                .collect()),
            Self::V3(route, _) => {
                #[cfg(test)]
                let _profile =
                    crate::blob_store::v3::work_profile::span("semantic_put_publication");
                let request = rows
                    .iter()
                    .fold(0u64, |bytes, (_, payload)| {
                        bytes.saturating_add(payload.len() as u64)
                    })
                    .saturating_mul(2)
                    .saturating_add(1 << 20);
                let outcomes = crate::blob_store::v3::publish(
                    &route.storage,
                    &route.family,
                    request,
                    None,
                    |publication| {
                        publication.put_or_touch_batch(
                            FamilyPlane::Semantic,
                            rows,
                            FamilyPlane::Semantic.payload_schema(),
                        )
                    },
                )
                .map_err(v3_store_error)?;
                Ok(rows
                    .iter()
                    .zip(outcomes)
                    .map(|((key, _), outcome)| {
                        outcome
                            .map(|inserted| {
                                if inserted {
                                    PutOrTouch::Inserted { ref_epoch: 0 }
                                } else {
                                    PutOrTouch::Reused { ref_epoch: 0 }
                                }
                            })
                            .map_err(|error| match error {
                                crate::blob_store::v3::Error::Refused("payload_mismatch") => {
                                    crate::blob_store::v2::StoreError::ConflictingPayload(*key)
                                }
                                other => v3_store_error(other),
                            })
                    })
                    .collect())
            }
        }
    }

    /// Verified-run markers are a v2 deletion-epoch optimization; v3 stores
    /// none, which only costs later loads their chunk-row reads.
    fn put_at_deletion_epoch(
        &self,
        build: impl FnOnce(&crate::blob_store::v2::DeletionEpoch) -> Option<(FamilyKey, Vec<u8>)>,
    ) -> Result<Option<PutOrTouch>, crate::blob_store::v2::StoreError> {
        match self {
            Self::V2(store) => store.put_at_deletion_epoch(build),
            Self::V3(..) => Ok(None),
        }
    }

    fn deletion_epoch(
        &self,
    ) -> Result<Option<crate::blob_store::v2::DeletionEpoch>, crate::blob_store::v2::StoreError>
    {
        match self {
            Self::V2(store) => store.deletion_epoch(),
            Self::V3(..) => Ok(None),
        }
    }

    fn touch(
        &self,
        keys: &[FamilyKey],
    ) -> Result<crate::blob_store::v2::TouchReport, crate::blob_store::v2::StoreError> {
        match self {
            Self::V2(store) => store.touch(keys),
            Self::V3(route, opened) => {
                if Self::v3_reader(route, opened)?.is_none() {
                    return Ok(crate::blob_store::v2::TouchReport {
                        missing: keys.to_vec(),
                        ..Default::default()
                    });
                }
                let request = (keys.len() as u64).saturating_mul(64 * 1024).max(1 << 20);
                let missing = crate::blob_store::v3::publish(
                    &route.storage,
                    &route.family,
                    request,
                    None,
                    |publication| publication.touch(keys),
                )
                .map_err(v3_store_error)?;
                Ok(crate::blob_store::v2::TouchReport {
                    touched: keys.len() - missing.len(),
                    missing,
                    ref_epoch: 0,
                })
            }
        }
    }
}

fn load_chunk(
    store: &SemanticStore,
    arena: &super::semantic_arena::SemanticArena,
    key: &FamilyKey,
    identity: &str,
    text: &str,
) -> Result<Option<Vec<f32>>, crate::blob_store::v2::StoreError> {
    let Some(payload) = store.get(key)? else {
        return Ok(None);
    };
    arena.note_chunk_row_read(payload.len());
    Ok(decode_chunk_vector(&payload, identity, text))
}

/// Whether the semantic plane has work for `rel_path`.
pub fn applies_to(rel_path: &RelPath) -> bool {
    !rel_path.is_synthetic()
        && rel_path_to_os(rel_path)
            .is_ok_and(|path| crate::semantic_index::is_semantic_indexed_extension(&path))
}

/// The key of a file's bytes, computed during strict reconciliation from the
/// buffer that was hashed. Publication uses it to mark content that any view
/// or earlier session already embedded as ready without a model call.
#[derive(Clone, Debug)]
pub struct SemanticAttachment {
    pub content: ContentHash,
    pub producer: String,
    pub key: FamilyKey,
}

/// How much one fill may do for one checkout.
#[derive(Clone, Copy, Debug)]
pub struct FillBudget {
    /// Work items taken per fill; the rest stay queued for the next fill.
    pub max_files: usize,
    /// Texts per model call.
    pub max_batch: usize,
    /// How long to wait for another view that is embedding a shared key.
    pub shared_wait: Duration,
}

impl Default for FillBudget {
    fn default() -> Self {
        Self {
            max_files: 256,
            max_batch: 64,
            shared_wait: Duration::from_secs(60),
        }
    }
}

/// What one fill did, including source work, reuse, completion and elapsed phase times.
/// `model_calls` and `embedded_texts` count real calls into the embedding function.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FillReport {
    /// Work items queued when the fill started.
    pub queued: usize,
    /// Items left for a later fill because of the budget.
    pub deferred: usize,
    /// Items whose file no longer held the queued content.
    pub moved: usize,
    /// Keys already resident in the family arena.
    pub resident_hits: usize,
    /// Keys found in the family store (another view or an earlier session).
    pub stored_hits: usize,
    /// Keys another view of this process embedded while this fill waited.
    pub shared_waits: usize,
    /// Keys this fill embedded.
    pub embedded_keys: usize,
    pub embedded_texts: usize,
    pub model_calls: usize,
    /// Chunks in claimed file versions, including reused chunks.
    pub chunks: usize,
    pub chunk_reused: usize,
    /// Successful chunk lookups in memory, including admission rechecks.
    pub chunk_resident_hits: usize,
    pub source_reads: usize,
    pub source_bytes: u64,
    /// Completions installed into the fill map.
    pub installed: usize,
    /// Completions dropped because content or producer moved on.
    pub dropped: usize,
    /// Items recorded as deterministic failures.
    pub failed: usize,
    /// Transient errors (the embedding call failed, or the store was busy);
    /// their items stay pending and a later fill retries them.
    pub errors: Vec<String>,
    /// Store errors other than busy: the store refused what was embedded.
    /// Their items stay pending, but retrying on a timer would not help.
    pub store_errors: Vec<String>,
    pub timing: FillTiming,
}

/// Wall-clock phases of a fill. `file_lookup`, `store_write` and `fold` include
/// their SQLite waits and pin-file I/O. `chunk_wait` measures the lock shared by
/// a repository family's chunk batches; `vector_lookup` excludes that lock wait.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FillTiming {
    pub mutex_wait: Duration,
    pub reconcile: Duration,
    pub queue_setup: Duration,
    pub read_hash: Duration,
    pub file_lookup: Duration,
    pub chunk: Duration,
    pub chunk_wait: Duration,
    pub vector_lookup: Duration,
    pub store_write: Duration,
    pub shared_wait: Duration,
    pub fold: Duration,
}

/// A semantic answer for one snapshot. Paths without current vectors are
/// named so the caller can report `complete: false` with a named gap.
#[derive(Clone, Debug)]
pub struct SemanticQuery {
    pub results: Vec<SemanticResult>,
    pub pending: Vec<PathBuf>,
    pub failed: Vec<PathBuf>,
    /// True when membership or content could not be vouched for (the watcher
    /// was not healthy); membership was re-walked, but vectors may lag disk.
    pub unvouched: bool,
    /// Why this checkout's vectors cannot currently be brought up to date,
    /// when the reason is not one a timed retry would fix.
    pub unavailable: Option<String>,
}

impl SemanticQuery {
    pub fn complete(&self) -> bool {
        self.pending.is_empty()
            && self.failed.is_empty()
            && !self.unvouched
            && self.unavailable.is_none()
    }
}

/// The resident semantic state of one open generation.
#[derive(Debug)]
struct Resident {
    base: ViewSemanticBase,
    /// Base members and the content their runs were computed for.
    members: BTreeMap<RelPath, ContentHash>,
    /// Failures the generation records for this producer.
    failed: BTreeMap<RelPath, ContentHash>,
    /// Kept readable by borrowers, but queued for the owning view to repair.
    legacy_summaries: BTreeSet<RelPath>,
}

struct SemanticReader {
    resident: Arc<Resident>,
    state: Arc<Mutex<ViewState>>,
}

type CacheKey = (String, [u8; 32], u64, bool);

/// Per-checkout mutable state: installed completions and failures that are
/// not folded yet, the live pin protecting their keys, and the last overlay.
#[derive(Default)]
struct ViewState {
    fill: FillMap,
    // Ready fills can precede their first overlay; the arena's own reference
    // alone is evictable, so each visible fill retains its immutable run.
    fill_runs: BTreeMap<FamilyKey, Arc<crate::semantic_index::SemanticVectors>>,
    fill_version: u64,
    failed: BTreeMap<(RelPath, ContentHash), String>,
    tracker: FailureTracker,
    live: Option<Arc<Mutex<crate::pins::LivePin>>>,
    /// The live pin in the v3 member directory, for fills of a generation
    /// published in the v3 namespace. It is carried across generation
    /// publications but not trimmed after folds yet (only the v2 pin is), so
    /// it keeps its keys until the view state holding it is dropped.
    live_v3: Option<Arc<Mutex<crate::pins::LivePin>>>,
    active_fills: usize,
    cache: Option<(CacheKey, Arc<SemanticIndex>)>,
}

struct ActiveFill(Arc<Mutex<ViewState>>);
impl ActiveFill {
    fn new(view: &Arc<Mutex<ViewState>>) -> Self {
        lock(view).active_fills += 1;
        Self(view.clone())
    }
}
impl Drop for ActiveFill {
    fn drop(&mut self) {
        lock(&self.0).active_fills -= 1;
    }
}

type ViewKey = (String, String);
type ResidentKey = (String, String, String);

/// The semantic plane: one per process, serving every view of every family.
pub struct SemanticPlane {
    storage: PathBuf,
    producer: SemanticProducer,
    file_summary_format: u32,
    views: Mutex<BTreeMap<ViewKey, Arc<Mutex<ViewState>>>>,
    residents: Mutex<BTreeMap<ResidentKey, Arc<Resident>>>,
    arenas: Mutex<BTreeMap<String, Arc<super::semantic_arena::SemanticArena>>>,
    /// Scored indexes built by `overlay`, for tests that prove a path never
    /// builds one.
    overlay_builds: std::sync::atomic::AtomicU64,
    /// Separator of the native relative paths the overlay holds; see
    /// [`overlay_rel_path`]. Always the platform's own outside tests.
    separator: char,
    /// Unique in the process and never reused, unlike the plane's address.
    id: u64,
    /// Counts actual fill reads separately from the driver's reconciliation
    /// reads, so source-reuse tests cannot miss a read on the embedding path.
    #[cfg(test)]
    source_reads: Mutex<BTreeMap<PathBuf, usize>>,
    #[cfg(test)]
    after_conflict: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

static NEXT_PLANE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// SQLite reports that another connection holds the database for a moment.
fn store_error_is_busy(error: &crate::blob_store::v2::StoreError) -> bool {
    matches!(
        error,
        crate::blob_store::v2::StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Converts a view path into the relative path the semantic overlay holds.
///
/// View keys, manifests and fill maps name files by `RelPath`: bytes with `/`
/// separators on every platform. The overlay is a `SemanticIndex`, whose
/// chunk paths, tombstones and search results use the platform's native form,
/// the form a full build produces from walker paths (`src\a.rs` on Windows).
/// That native form also appears in the text that is embedded, so view
/// vectors, tombstones and results only agree with a full build when every
/// view path is converted here, once, as it enters the overlay. Nothing in
/// the overlay compares a `RelPath` with a native path.
pub(crate) fn overlay_rel_path(rel_path: &RelPath, separator: char) -> Option<PathBuf> {
    if separator == '/' {
        return rel_path_to_os(rel_path).ok();
    }
    let text = std::str::from_utf8(rel_path.as_bytes()).ok()?;
    Some(PathBuf::from(text.replace('/', &separator.to_string())))
}

impl SemanticPlane {
    /// A plane for the views stored under `storage`, the process's AFT
    /// storage root.
    pub fn new(storage: PathBuf, producer: SemanticProducer) -> Self {
        Self {
            storage,
            producer,
            file_summary_format: crate::semantic_index::FILE_SUMMARY_BUDGET_KEY_FORMAT,
            views: Mutex::new(BTreeMap::new()),
            residents: Mutex::new(BTreeMap::new()),
            arenas: Mutex::new(BTreeMap::new()),
            overlay_builds: std::sync::atomic::AtomicU64::new(0),
            id: NEXT_PLANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            separator: std::path::MAIN_SEPARATOR,
            #[cfg(test)]
            source_reads: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            after_conflict: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn take_source_reads(&self) -> BTreeMap<PathBuf, usize> {
        std::mem::take(&mut *lock(&self.source_reads))
    }

    /// Lets a test on any platform run the overlay with Windows' separator.
    /// The arena caches runs with their overlay paths, so a plane with another
    /// separator must use its own storage root.
    #[cfg(test)]
    pub(crate) fn with_path_separator(mut self, separator: char) -> Self {
        self.separator = separator;
        self
    }

    fn overlay_path(&self, rel_path: &RelPath) -> Option<PathBuf> {
        overlay_rel_path(rel_path, self.separator)
    }

    /// This plane's process-unique id.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn semantic_producer(&self) -> &SemanticProducer {
        &self.producer
    }

    /// The family arena this plane uses for `family`.
    pub fn arena(&self, family: &str) -> Arc<super::semantic_arena::SemanticArena> {
        Arc::clone(
            lock(&self.arenas)
                .entry(family.to_owned())
                .or_insert_with(|| super::semantic_arena::arena_for(&self.storage, family)),
        )
    }

    fn view(&self, access: &ViewAccess) -> Arc<Mutex<ViewState>> {
        Arc::clone(
            lock(&self.views)
                .entry((access.family().to_owned(), access.scope().to_owned()))
                .or_default(),
        )
    }

    fn active_view(&self, access: &ViewAccess) -> (Arc<Mutex<ViewState>>, ActiveFill) {
        // Publication cannot fork the current state between lookup and registration.
        let mut views = lock(&self.views);
        let view = views
            .entry((access.family().to_owned(), access.scope().to_owned()))
            .or_default()
            .clone();
        let active = ActiveFill::new(&view);
        (view, active)
    }

    fn query_view(&self, access: &ViewAccess, snapshot: &Snapshot) -> Arc<Mutex<ViewState>> {
        self.query_view_for(access.family(), access.scope(), snapshot)
    }

    fn query_view_for(
        &self,
        family: &str,
        scope: &str,
        snapshot: &Snapshot,
    ) -> Arc<Mutex<ViewState>> {
        if let Some(reader) = self.pinned_reader(family, scope, snapshot) {
            return reader.state.clone();
        }
        lock(&self.views)
            .get(&(family.to_owned(), scope.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn open_resident(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<Arc<Resident>, PlaneError> {
        let resident = if let Some(resident) = self.resident(access, &generation.resident_key()) {
            resident
        } else {
            let resident = Arc::new(self.admit(access, generation)?);
            let key = (
                access.family().to_owned(),
                access.scope().to_owned(),
                generation.resident_key(),
            );
            lock(&self.residents).entry(key).or_insert(resident).clone()
        };
        if matches!(access, ViewAccess::Reader { .. }) && !resident.legacy_summaries.is_empty() {
            log::info!("semantic borrower loaded {} legacy small-budget summary runs; serving published vectors until owner repair", resident.legacy_summaries.len());
        }
        Ok(resident)
    }

    fn pinned_reader(
        &self,
        family: &str,
        scope: &str,
        snapshot: &Snapshot,
    ) -> Option<Arc<SemanticReader>> {
        let key = snapshot.generation().resident_key();
        let readers = snapshot.readers();
        readers
            .and_then(|readers| readers.resource(self as *const Self as usize, family, scope, &key))
            .and_then(|resource| resource.downcast::<SemanticReader>().ok())
    }

    fn pinned_resident(
        &self,
        family: &str,
        scope: &str,
        snapshot: &Snapshot,
    ) -> Option<Arc<Resident>> {
        let key = snapshot.generation().resident_key();
        self.pinned_reader(family, scope, snapshot)
            .map(|reader| reader.resident.clone())
            .or_else(|| {
                lock(&self.residents)
                    .get(&(family.to_owned(), scope.to_owned(), key))
                    .cloned()
            })
    }

    fn resident(&self, access: &ViewAccess, generation: &str) -> Option<Arc<Resident>> {
        lock(&self.residents)
            .get(&(
                access.family().to_owned(),
                access.scope().to_owned(),
                generation.to_owned(),
            ))
            .cloned()
    }

    /// Records semantic readiness for `manifest`'s entries; see
    /// [`super::first_load::CompositePlane::materialize`].
    fn materialize_with(
        &self,
        owner: &ViewRegistration,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut ManifestV2,
        live: &mut crate::pins::LivePin,
        store: &SemanticStore,
    ) -> Result<(), PlaneError> {
        let id = self.producer.id();
        if manifest.header().producers.semantic.as_deref() != Some(id.as_str()) {
            return Err(plane_error("semantic producer mismatch"));
        }
        let view = self.view(&ViewAccess::Owner(owner.clone()));
        let state = lock(&view);
        let mut candidates = Vec::new();
        for (path, entry) in manifest.entries_mut() {
            let EntryV2::Regular {
                content, planes, ..
            } = entry
            else {
                continue;
            };
            if !applies_to(path) {
                planes.semantic = None;
                continue;
            }
            // A completed small-cap repair replaces the old Ready file-run key.
            let repaired = self.producer.caps.total_chars < EmbedTextCaps::default().total_chars
                && state
                    .fill
                    .get(path, content, FamilyPlane::Semantic)
                    .is_some();
            if matches!(planes.semantic, Some(PlaneState::Ready { .. })) && !repaired {
                continue;
            }
            let stored = observed
                .get(path)
                .and_then(|entry| entry.attachments.get(&FamilyPlane::Semantic))
                .and_then(|value| value.downcast_ref::<SemanticAttachment>())
                .filter(|attachment| attachment.content == *content && attachment.producer == id)
                .map(|attachment| attachment.key);
            let key = match state.fill.get(path, content, FamilyPlane::Semantic) {
                Some(key) => Some(*key),
                None => match stored {
                    Some(key) => store
                        .get(&key)
                        .map_err(|error| plane_error(error.to_string()))?
                        .is_some()
                        .then_some(key),
                    None => None,
                },
            };
            if let Some(key) = key {
                candidates.push((path.clone(), key));
            }
            if let Some(reason) = state.failed.get(&(path.clone(), *content)) {
                planes.semantic = Some(PlaneState::Failed {
                    reason: reason.clone(),
                    producer: id.clone(),
                });
                continue;
            }
            if !matches!(&planes.semantic, Some(PlaneState::Failed { producer, .. }) if *producer == id)
            {
                planes.semantic = Some(PlaneState::pending("awaiting semantic fill"));
            }
        }
        if !candidates.is_empty() {
            // Pin all candidates before the touch transaction. Missing rows
            // remain pending/failed: GC may have won before protection landed.
            let keys = candidates.iter().map(|(_, key)| *key).collect::<Vec<_>>();
            live.protect(&keys)
                .map_err(|error| plane_error(error.to_string()))?;
            let missing = store
                .touch(&keys)
                .map_err(|error| plane_error(error.to_string()))?
                .missing
                .into_iter()
                .collect::<HashSet<_>>();
            for (path, key) in candidates {
                if !missing.contains(&key) {
                    if let Some(EntryV2::Regular { planes, .. }) = manifest.get_mut(&path) {
                        planes.semantic = Some(PlaneState::ready(&key));
                    }
                }
            }
        }
        Ok(())
    }

    fn store_reader(
        access: &ViewAccess,
        generation: &ComposedGeneration,
    ) -> Result<Option<Box<dyn super::semantic_arena::RunSource>>, PlaneError> {
        if let super::contracts::StorageRoute::V3(route) = access.route(generation) {
            let opened = std::sync::OnceLock::new();
            return SemanticStore::v3_reader(&route, &opened)
                .map(|reader| {
                    reader.map(|reader| {
                        Box::new(reader.clone()) as Box<dyn super::semantic_arena::RunSource>
                    })
                })
                .map_err(|error| plane_error(error.to_string()));
        }
        match access {
            ViewAccess::Owner(owner) => owner
                .open_store(FamilyPlane::Semantic)
                .map(|store| {
                    Some(Box::new(store.reader()) as Box<dyn super::semantic_arena::RunSource>)
                })
                .map_err(|error| plane_error(error.to_string())),
            ViewAccess::Reader { registration, .. } => registration
                .open_store(FamilyPlane::Semantic)
                .map(|store| {
                    store.map(|store| Box::new(store) as Box<dyn super::semantic_arena::RunSource>)
                })
                .map_err(|error| plane_error(error.to_string())),
        }
    }

    /// Makes `generation`'s semantic base resident, admitting every run it
    /// names into the family arena. Runs another view already admitted are
    /// shared, not decoded again.
    fn admit(
        &self,
        access: &ViewAccess,
        generation: &ComposedGeneration,
    ) -> Result<Resident, PlaneError> {
        let manifest = generation;
        let id = self.producer.id();
        let compatible = manifest.header().producers.semantic.as_deref() == Some(id.as_str());
        let mut runs = Vec::new();
        let mut members = BTreeMap::new();
        let mut failed = BTreeMap::new();
        let mut legacy_summaries = BTreeSet::new();
        if compatible {
            let arena = self.arena(access.family());
            let store = Self::store_reader(access, generation)?;
            // Markers of every run loaded here are probed together afterwards.
            let mut seeds = Vec::new();
            for (path, entry) in manifest.entries() {
                let (Some(content), Some(state)) = (
                    entry.content(),
                    generation.plane_readiness(path, FamilyPlane::Semantic),
                ) else {
                    continue;
                };
                match state {
                    PlaneState::Ready { key } => {
                        let loaded = match (
                            crate::blob_store::v2::parse_hex32(key),
                            store.as_ref(),
                            self.overlay_path(path),
                        ) {
                            (Some(bytes), Some(store), Some(relative)) => arena
                                .load_deferring_seed(
                                    store.as_ref(),
                                    &FamilyKey::new(FamilyPlane::Semantic, bytes),
                                    &relative,
                                    &self.producer.payload(),
                                    &self.producer,
                                    &mut seeds,
                                )
                                .map_err(plane_error)?,
                            _ => None,
                        };
                        // Only a missing run is a cache miss. A failed read
                        // must not silently turn into new embedding work.
                        if let Some(run) = loaded {
                            if self.producer.summary_key_format(self.file_summary_format) > 0
                                && crate::blob_store::v2::parse_hex32(key)
                                    == Some(
                                        *self.producer.key_for_content(content, path, 0).as_bytes(),
                                    )
                            {
                                legacy_summaries.insert(path.clone());
                            }
                            runs.push(run);
                            members.insert(path.clone(), content);
                        }
                    }
                    PlaneState::Failed { producer, .. } if *producer == id => {
                        failed.insert(path.clone(), content);
                    }
                    _ => {}
                }
            }
            if let Some(store) = store.as_ref() {
                arena.seed_verified_runs(store.as_ref(), seeds, &self.producer);
            }
        }
        Ok(Resident {
            base: ViewSemanticBase::new(runs),
            members,
            failed,
            legacy_summaries,
        })
    }

    /// Where the vectors for `path` at `content` come from in this snapshot.
    fn vector_source(
        state: &ViewState,
        resident: &Resident,
        snapshot: &Snapshot,
        path: &RelPath,
        content: ContentHash,
        repair_summaries: bool,
    ) -> VectorSource {
        if let Some(patch) = snapshot
            .readiness_patches()
            .get(path)
            .filter(|patch| patch.content == content)
            .and_then(|patch| patch.planes.semantic.as_ref())
        {
            if snapshot
                .generation()
                .plane_readiness(path, FamilyPlane::Semantic)
                != Some(patch)
            {
                if let Some(key) = state.fill.get(path, &content, FamilyPlane::Semantic) {
                    return VectorSource::Fill(*key);
                }
                return if patch.is_failed() {
                    VectorSource::Failed
                } else {
                    VectorSource::Pending
                };
            }
        }
        // A live entry supersedes the generation: its base run, if any, holds
        // other content and must never be scored.
        let live = matches!(snapshot.source(path), Source::Live(_));
        if !live && resident.members.get(path) == Some(&content) {
            if repair_summaries && resident.legacy_summaries.contains(path) {
                if let Some(key) = state.fill.get(path, &content, FamilyPlane::Semantic) {
                    return VectorSource::Fill(*key);
                }
            }
            return VectorSource::Base;
        }
        if let Some(key) = state.fill.get(path, &content, FamilyPlane::Semantic) {
            return VectorSource::Fill(*key);
        }
        if state.failed.contains_key(&(path.clone(), content))
            || (!live && resident.failed.get(path) == Some(&content))
        {
            return VectorSource::Failed;
        }
        VectorSource::Pending
    }

    fn summary_repair_pending(
        state: &ViewState,
        resident: &Resident,
        snapshot: &Snapshot,
        path: &RelPath,
        content: ContentHash,
        repair_summaries: bool,
    ) -> bool {
        repair_summaries
            && resident.legacy_summaries.contains(path)
            && resident.members.get(path) == Some(&content)
            && !matches!(snapshot.source(path), Source::Live(_))
            && state
                .fill
                .get(path, &content, FamilyPlane::Semantic)
                .is_none()
            && !state.failed.contains_key(&(path.clone(), content))
    }

    /// The base entries `snapshot` inherits whose semantic plane the base
    /// still has pending. Only the base's owner may complete them, by
    /// publishing a readiness successor; a worker never overrides them.
    pub(crate) fn inherited_pending(&self, snapshot: &Snapshot) -> Vec<(RelPath, ContentHash)> {
        let generation = snapshot.generation();
        if generation.base_header().is_none() {
            return Vec::new();
        }
        generation
            .entries()
            .filter_map(|(path, entry)| {
                let EntryV2::Regular {
                    content, planes, ..
                } = entry
                else {
                    return None;
                };
                (applies_to(path)
                    && !matches!(snapshot.source(path), Source::Live(_))
                    && !generation.owns_work(path, FamilyPlane::Semantic)
                    && planes.semantic.as_ref().is_some_and(PlaneState::is_pending))
                .then(|| (path.clone(), *content))
            })
            .collect()
    }

    /// The keys this view's fills completed for `paths` (at the content
    /// given) whose rows the family's store holds, as `(path, content, key)`.
    pub(crate) fn completed_keys(
        &self,
        owner: &ViewRegistration,
        snapshot: &Snapshot,
        paths: &[(RelPath, ContentHash)],
    ) -> Result<Vec<(RelPath, ContentHash, FamilyKey)>, PlaneError> {
        let candidates = {
            let view = self.view(&ViewAccess::Owner(owner.clone()));
            let state = lock(&view);
            paths
                .iter()
                .filter_map(|(path, content)| {
                    state
                        .fill
                        .get(path, content, FamilyPlane::Semantic)
                        .map(|key| (path.clone(), *content, *key))
                })
                .collect::<Vec<_>>()
        };
        let store = SemanticStore::for_generation(owner, snapshot.generation())?;
        let mut stored = Vec::new();
        for (path, content, key) in candidates {
            if store
                .get(&key)
                .map_err(|error| plane_error(error.to_string()))?
                .is_some()
            {
                stored.push((path, content, key));
            }
        }
        Ok(stored)
    }

    /// Missing vectors and owner-only summary repairs are separate work:
    /// a queued repair must not withdraw the file's published vectors.
    fn work_items(
        &self,
        state: &ViewState,
        resident: &Resident,
        snapshot: &Snapshot,
        repair_summaries: bool,
    ) -> Vec<WorkItem> {
        let producer = self.producer.id();
        snapshot
            .membership()
            .into_iter()
            .filter_map(|(path, disk)| {
                let DiskState::Present { content, .. } = disk else {
                    return None;
                };
                (applies_to(&path)
                    && (matches!(snapshot.source(&path), Source::Live(_))
                        || snapshot
                            .readiness_patches()
                            .get(&path)
                            .is_some_and(|patch| {
                                patch.content == content && patch.planes.semantic.is_some()
                            })
                        || snapshot
                            .generation()
                            .owns_work(&path, FamilyPlane::Semantic))
                    && (Self::summary_repair_pending(
                        state,
                        resident,
                        snapshot,
                        &path,
                        content,
                        repair_summaries,
                    ) || matches!(
                        Self::vector_source(
                            state,
                            resident,
                            snapshot,
                            &path,
                            content,
                            repair_summaries
                        ),
                        VectorSource::Pending
                    )))
                .then(|| WorkItem {
                    plane: FamilyPlane::Semantic,
                    rel_path: path,
                    content,
                    producer: producer.clone(),
                })
            })
            .collect()
    }

    /// Embeds this checkout's semantic work within `budget`.
    ///
    /// Each item's bytes are read and checked against the queued content, so
    /// a file that moved on is left for the next fill. Keys are looked up in
    /// the family arena, then the family store, and only misses reach
    /// `embed`; a key another view of this process is embedding is waited
    /// for, not embedded again. Every key goes into this view's live pin
    /// before it is stored or touched. Completions are admitted against
    /// `current()`, the checkout's snapshot once the work is done, and only
    /// for the content and producer they were computed for.
    pub fn fill<F>(
        &self,
        owner: &ViewRegistration,
        snapshot: &Snapshot,
        budget: FillBudget,
        embed: &mut F,
        current: &dyn Fn() -> Snapshot,
    ) -> Result<FillReport, PlaneError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        self.fill_admitted(
            owner,
            snapshot,
            budget,
            embed,
            current,
            &mut || Ok(()),
            false,
        )
    }

    pub(crate) fn fill_admitted<F>(
        &self,
        owner: &ViewRegistration,
        snapshot: &Snapshot,
        budget: FillBudget,
        embed: &mut F,
        current: &dyn Fn() -> Snapshot,
        before_embed: &mut dyn FnMut() -> Result<(), String>,
        bounded_reads: bool,
    ) -> Result<FillReport, PlaneError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let setup_started = Instant::now();
        let access = ViewAccess::Owner(owner.clone());
        if owner.registry().storage() != self.storage {
            return Err(plane_error("view belongs to another storage root"));
        }
        let resident = self
            .pinned_resident(access.family(), access.scope(), snapshot)
            .ok_or_else(|| plane_error("semantic generation not resident"))?;
        let (view, _active_fill) = self.active_view(&access);
        let mut report = FillReport::default();
        // The view's state is locked only for short bookkeeping steps, never
        // across file reads, model calls or store writes, so queries and
        // publication of this checkout proceed while it fills.
        let v3 = snapshot.generation().v3_route().cloned();
        let needs_pin = {
            let state = lock(&view);
            if v3.is_some() {
                state.live_v3.is_none()
            } else {
                state.live.is_none()
            }
        };
        if needs_pin {
            let pin = match &v3 {
                Some(route) => {
                    crate::pins::LivePin::create_v3(&route.storage, &route.family, &route.scope)
                }
                None => crate::pins::LivePin::create(owner),
            }
            .map_err(|error| plane_error(error.to_string()))?;
            let pin = Arc::new(Mutex::new(pin));
            let mut state = lock(&view);
            let slot = if v3.is_some() {
                &mut state.live_v3
            } else {
                &mut state.live
            };
            if slot.is_none() {
                *slot = Some(pin.clone());
            }
            drop(state);
            drop(pin);
        }
        let mut items = self.work_items(&lock(&view), &resident, snapshot, true);
        report.queued = items.len();
        super::io::WORK
            .semantic_fills
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::cold_build_limiter::progress::remaining(items.len());
        report.deferred = items.len().saturating_sub(budget.max_files);
        items.truncate(budget.max_files);
        report.timing.queue_setup = setup_started.elapsed();

        let root = owner.root().to_path_buf();
        let read_started = Instant::now();
        let mut prepared = Vec::new();
        let mut by_path = BTreeMap::new();
        for item in items {
            // The file is read through the OS path; `relative` is the overlay
            // form its chunks and embedded text carry.
            let (Ok(source), Some(relative)) = (
                rel_path_to_os(&item.rel_path),
                self.overlay_path(&item.rel_path),
            ) else {
                continue;
            };
            let source = root.join(&source);
            #[cfg(test)]
            {
                *lock(&self.source_reads).entry(source.clone()).or_default() += 1;
            }
            // A file can grow after its snapshot size was counted toward the
            // small-input limit. Read at most that recorded size plus one byte
            // to detect growth without allocating it. Content that no longer
            // matches the snapshot stays pending for reconciliation.
            let bytes = if bounded_reads {
                let size = match snapshot.disk_state(&item.rel_path) {
                    DiskState::Present { size, .. } => size,
                    DiskState::Absent => 0,
                };
                crate::project_content::open(&source).and_then(|file| {
                    let mut bytes = Vec::new();
                    file.take(size.saturating_add(1)).read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
            } else {
                crate::project_content::read(&source)
            };
            report.source_reads += 1;
            super::io::WORK
                .semantic_files_filled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            report.source_bytes += bytes.as_ref().map_or(0, |bytes| bytes.len() as u64);
            match bytes {
                Ok(bytes) if ContentHash::of(&bytes) == item.content => {
                    prepared.push(PreparedWork {
                        rel_path: item.rel_path.as_bytes().to_vec(),
                        full_key: self.producer.full_key_for_summary_format(
                            &bytes,
                            &item.rel_path,
                            self.file_summary_format,
                        ),
                        payload: bytes,
                    });
                    by_path.insert(item.rel_path.as_bytes().to_vec(), (item, relative));
                }
                _ => report.moved += 1,
            }
        }
        // Equal keys name equal bytes; grouping them makes each key's work
        // happen once however many paths ask for it.
        let mut groups = crate::refresh::deduplicate_full_keys(prepared)
            .map_err(|error| plane_error(error.to_string()))?;
        {
            let state = lock(&view);
            groups.retain(|group| !state.tracker.is_quarantined(&group.full_key));
        }
        report.timing.read_hash += read_started.elapsed();

        let lookup_started = Instant::now();
        let arena = self.arena(owner.family());
        let store = SemanticStore::for_generation(owner, snapshot.generation())?;
        let payload_producer = self.producer.payload();
        let mut ready = Vec::new();
        let mut claimed = Vec::new();
        let mut waiting = Vec::new();
        for group in groups {
            let key = FamilyKey::from(&group.full_key);
            let Some((item, relative)) = group.rel_paths.first().and_then(|path| by_path.get(path))
            else {
                continue;
            };
            if let Some(run) = arena.get(&key) {
                report.resident_hits += 1;
                ready.push((item.clone(), key, run));
            } else if let Some(run) = arena
                .load(
                    &*store.reader(),
                    &key,
                    relative,
                    &payload_producer,
                    &self.producer,
                )
                .map_err(plane_error)?
            {
                report.stored_hits += 1;
                ready.push((item.clone(), key, run));
            } else if let Some(claim) = {
                #[cfg(test)]
                run_before_claim_for_test();
                arena.claim(key)
            } {
                // A previous owner can finish and release its claim between
                // our store miss and acquiring this claim. Recheck under the
                // claim or a completed key would be embedded a second time.
                if let Some(run) = arena.get(&key) {
                    report.resident_hits += 1;
                    ready.push((item.clone(), key, run));
                } else if let Some(run) = arena
                    .load(
                        &*store.reader(),
                        &key,
                        relative,
                        &payload_producer,
                        &self.producer,
                    )
                    .map_err(plane_error)?
                {
                    report.stored_hits += 1;
                    ready.push((item.clone(), key, run));
                } else {
                    claimed.push((item.clone(), relative.clone(), group, claim));
                }
            } else {
                waiting.push((item.clone(), relative.clone(), key));
            }
        }
        report.timing.file_lookup += lookup_started.elapsed();

        // Chunk what this fill must embed, from the bytes that were hashed.
        let chunk_started = Instant::now();
        let mut to_embed = Vec::new();
        let mut chunks = Vec::new();
        let mut failures = Vec::new();
        let repairing = self.producer.caps.total_chars < EmbedTextCaps::default().total_chars
            && self.file_summary_format > 0;
        let mut legacy_reuse = repairing.then(VecDeque::new);
        for (item, relative, group, claim) in claimed {
            let mut legacy_run = None;
            if repairing {
                let old_key = self.producer.legacy_key(&group.payload, &item.rel_path);
                if let Some(payload) = store
                    .get(&old_key)
                    .map_err(|error| plane_error(error.to_string()))?
                {
                    let run = crate::semantic_index::SemanticVectors::decode_view_payload_for_summary_repair(
                        &payload, &relative, &payload_producer,
                    ).map_err(plane_error)?;
                    legacy_run = Some(run);
                }
            }
            let collected = if let Some(run) = legacy_run {
                let (chunks, reuse) = run.into_summary_repair_chunks(self.producer.caps);
                Ok((chunks, Some(reuse)))
            } else {
                crate::semantic_index::chunk_view_file(
                    &root,
                    &relative,
                    &group.payload,
                    self.producer.caps,
                )
                .map(|chunks| (chunks.unwrap_or_default(), None))
            };
            match collected {
                Ok((file_chunks, reuse)) => {
                    if let Some(plan) = legacy_reuse.as_mut() {
                        let mut reuse = reuse.unwrap_or_default().into_iter();
                        plan.extend(
                            file_chunks
                                .iter()
                                .map(|chunk| (chunk.embed_text.clone(), reuse.next().flatten())),
                        );
                    }
                    report.chunks += file_chunks.len();
                    chunks.push(file_chunks);
                    to_embed.push((item, relative, group.full_key, claim));
                }
                Err(reason) => failures.push((item, group.full_key, reason)),
            }
        }
        report.timing.chunk += chunk_started.elapsed();
        let mut transient = Vec::new();
        let mut succeeded = Vec::new();
        // Protect all planned keys once, even though complete file runs are
        // stored in smaller batches. Per-batch growing-pin writes would undo
        // the linear pin-write bound. Unattempted keys are harmless protection.
        let keys = ready
            .iter()
            .map(|(_, key, _)| *key)
            .chain(to_embed.iter().map(|(_, _, _, claim)| *claim.key()))
            .chain(
                chunks
                    .iter()
                    .flatten()
                    .map(|chunk| self.producer.chunk_key(&chunk.embed_text)),
            )
            .collect::<Vec<_>>();
        let store_started = Instant::now();
        protect(&view, &keys, store.is_v3())?;
        report.timing.store_write += store_started.elapsed();
        let mut pending = to_embed.into_iter().zip(chunks).peekable();
        while pending.peek().is_some() {
            // Keep whole files together so every successful request can be
            // stored before the next one. A single larger file still spans
            // requests and cannot be persisted until its run is complete.
            let mut to_embed = Vec::new();
            let mut chunks = Vec::new();
            let mut batch_texts = 0usize;
            while let Some((_, file_chunks)) = pending.peek() {
                if !to_embed.is_empty()
                    && batch_texts.saturating_add(file_chunks.len()) > budget.max_batch.max(1)
                {
                    break;
                }
                let (item, file_chunks) = pending.next().expect("peeked file");
                batch_texts += file_chunks.len();
                to_embed.push(item);
                chunks.push(file_chunks);
            }
            let mut canonical_chunks = Vec::new();
            let row_plan = if let Some(plan) = legacy_reuse.as_mut() {
                let mut rows = Vec::with_capacity(batch_texts);
                for _ in 0..batch_texts {
                    rows.push(
                        plan.pop_front()
                            .ok_or_else(|| plane_error("summary repair row plan exhausted"))?,
                    );
                }
                Some(rows)
            } else {
                None
            };
            let embedded = {
                let mut rows = |batch_start: usize, batch: Vec<String>| {
                    let Some(plan) = row_plan.as_ref() else {
                        return self.embed_chunks(
                            &arena,
                            &store,
                            batch,
                            embed,
                            &mut report,
                            before_embed,
                            &mut canonical_chunks,
                        );
                    };
                    let mut rows = Vec::with_capacity(batch.len());
                    let mut missing = Vec::new();
                    let end = batch_start
                        .checked_add(batch.len())
                        .ok_or("summary repair row cursor overflow")?;
                    let planned = plan
                        .get(batch_start..end)
                        .ok_or("summary repair row plan exhausted")?;
                    for (text, (expected, reuse)) in batch.into_iter().zip(planned) {
                        if expected != &text {
                            return Err("summary repair row plan changed".to_string());
                        }
                        if let Some(vector) = reuse {
                            // Keep legacy vectors local; never seed unvouched
                            // file data into the global canonical chunk cache.
                            rows.push(Some(crate::semantic_index::BuildEmbeddingRow::Embedded {
                                embedded_text: text,
                                vector: vector.to_vec(),
                            }));
                            canonical_chunks.push(Arc::clone(vector));
                            report.chunk_reused += 1;
                        } else {
                            missing.push(text);
                            rows.push(None);
                        }
                    }
                    let mut embedded = if missing.is_empty() {
                        Vec::new()
                    } else {
                        self.embed_chunks(
                            &arena,
                            &store,
                            missing,
                            embed,
                            &mut report,
                            before_embed,
                            &mut canonical_chunks,
                        )?
                    }
                    .into_iter();
                    Ok(rows
                        .into_iter()
                        .map(|row| {
                            row.unwrap_or_else(|| {
                                embedded.next().expect("validated summary repair row count")
                            })
                        })
                        .collect())
                };
                crate::semantic_index::embed_view_files_with_rows_at_offset(
                    chunks,
                    &mut rows,
                    budget.max_batch,
                )
            };
            match embedded {
                Ok(runs) => {
                    let store_started = Instant::now();
                    let payloads: Vec<_> = to_embed
                        .iter()
                        .zip(&runs)
                        .map(|((_, _, _, claim), run)| {
                            (*claim.key(), run.encode_view_payload(&payload_producer))
                        })
                        .collect();
                    let outcomes = match store.put_batch(&payloads) {
                        Ok(outcomes) => outcomes,
                        Err(error) => {
                            if store_error_is_busy(&error) {
                                report.errors.push(error.to_string());
                            } else {
                                report.store_errors.push(error.to_string());
                            }
                            report.deferred += to_embed.len() + pending.count();
                            report.timing.store_write += store_started.elapsed();
                            break;
                        }
                    };
                    for (((item, relative, full_key, claim), mut run), ((_, payload), outcome)) in
                        to_embed
                            .into_iter()
                            .zip(runs)
                            .zip(payloads.into_iter().zip(outcomes))
                    {
                        let key = *claim.key();
                        match outcome {
                            Ok(PutOrTouch::Inserted { .. } | PutOrTouch::Reused { .. }) => {
                                succeeded.push(full_key);
                                // The stored row now holds exactly `payload`.
                                // Vouch for it only after checking every vector
                                // against its chunk row at the deletion epoch
                                // read in the marker's own write transaction,
                                // so no row can be deleted in between and a
                                // later load can index those vectors without
                                // reading the rows. The marker is disposable:
                                // losing it, or failing to write it, only
                                // costs those reads.
                                let digest = blake3::hash(&payload);
                                if let Err(error) = store.put_at_deletion_epoch(|epoch| {
                                    use super::semantic_arena::ChunkNaming;
                                    run_matches_chunk_rows(&run, &self.producer, &arena, epoch)
                                        .then(|| {
                                            (
                                                self.producer.verified_run_marker(&digest, epoch),
                                                verified_run_marker_payload(&digest, epoch),
                                            )
                                        })
                                }) {
                                    log::debug!(
                                        "semantic verified-run marker for {key} not stored: {error}"
                                    );
                                }
                                run.strip_embed_text();
                                let run = arena.install(key, run);
                                report.embedded_keys += 1;
                                ready.push((item, key, run));
                            }
                            Ok(PutOrTouch::Quarantined) => report.failed += 1,
                            // Another process stored a payload under the
                            // same key first (a model need not be bit-stable
                            // across calls); read the stored payload so every
                            // view uses the same vectors for the key.
                            Err(crate::blob_store::v2::StoreError::ConflictingPayload(_)) => {
                                #[cfg(test)]
                                if let Some(hook) = lock(&self.after_conflict).take() {
                                    hook();
                                }
                                if let Some(run) = arena
                                    .load(
                                        &*store.reader(),
                                        &key,
                                        &relative,
                                        &payload_producer,
                                        &self.producer,
                                    )
                                    .map_err(plane_error)?
                                {
                                    report.stored_hits += 1;
                                    ready.push((item, key, run));
                                } else {
                                    // The winning row can disappear before the
                                    // reload. Keep the work pending and visible
                                    // to transient retry accounting.
                                    report.deferred += 1;
                                    transient.push(full_key);
                                }
                            }
                            Err(error) if store_error_is_busy(&error) => {
                                report.errors.push(error.to_string())
                            }
                            Err(error) => report.store_errors.push(error.to_string()),
                        }
                        // Releasing the claim wakes views waiting on the key.
                        drop(claim);
                    }
                    admit_ready(
                        &view,
                        &store,
                        std::mem::take(&mut ready),
                        &self.producer.id(),
                        current,
                        &mut report,
                    )?;
                    report.timing.store_write += store_started.elapsed();
                    if !report.errors.is_empty() || !report.store_errors.is_empty() {
                        report.deferred += pending.count();
                        break;
                    }
                }
                Err(error) => {
                    crate::semantic_index::retain_view_pause_chunks(
                        &root,
                        self.producer.caps,
                        to_embed
                            .iter()
                            .map(|(item, relative, _, _)| (relative.clone(), item.content)),
                        &error,
                    );
                    if crate::synapse_embed::LanePause::from_error(&error).is_none() {
                        transient.extend(to_embed.into_iter().map(|(_, _, full_key, _)| full_key));
                        report.deferred += pending.count();
                    } else {
                        report.deferred += pending.len();
                        crate::semantic_index::retain_unattempted_view_chunks(
                            &root,
                            self.producer.caps,
                            pending.map(|((item, relative, _, _claim), chunks)| {
                                (relative, item.content, chunks)
                            }),
                            &error,
                        );
                    }
                    report.errors.push(error);
                    break;
                }
            }
        }

        for (item, relative, key) in waiting {
            let wait_started = Instant::now();
            arena.wait_released(&key, budget.shared_wait);
            report.timing.shared_wait += wait_started.elapsed();
            let lookup_started = Instant::now();
            let run = match arena.get(&key) {
                Some(run) => Some(run),
                None => arena
                    .load(
                        &*store.reader(),
                        &key,
                        &relative,
                        &payload_producer,
                        &self.producer,
                    )
                    .map_err(plane_error)?,
            };
            if let Some(run) = run {
                report.shared_waits += 1;
                ready.push((item, key, run));
            }
            report.timing.file_lookup += lookup_started.elapsed();
        }

        let store_started = Instant::now();
        admit_ready(
            &view,
            &store,
            ready,
            &self.producer.id(),
            current,
            &mut report,
        )?;
        report.timing.store_write += store_started.elapsed();
        let mut state = lock(&view);
        for full_key in &succeeded {
            state.tracker.record_success(full_key);
        }
        for full_key in &transient {
            state
                .tracker
                .record_failure(owner.family(), full_key, FailureClass::Transient);
        }
        for (item, full_key, reason) in failures {
            state
                .tracker
                .record_failure(owner.family(), &full_key, FailureClass::NonTransient);
            state
                .failed
                .insert((item.rel_path.clone(), item.content), reason);
            report.failed += 1;
        }
        if report.failed > 0 {
            state.fill_version += 1;
        }
        Ok(report)
    }

    fn embed_chunks<F>(
        &self,
        arena: &super::semantic_arena::SemanticArena,
        store: &SemanticStore,
        texts: Vec<String>,
        embed: &mut F,
        report: &mut FillReport,
        before_embed: &mut dyn FnMut() -> Result<(), String>,
        canonical_chunks: &mut Vec<Arc<[f32]>>,
    ) -> Result<Vec<crate::semantic_index::BuildEmbeddingRow>, String>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        use crate::semantic_index::BuildEmbeddingRow;
        // File claims alone do not serialize different versions of one file.
        // Hold a family batch gate through lookup, model and persistence. No
        // file-claim wait occurs under it, and it bounds retained work to one
        // model batch without acquiring chunk claims in conflicting orders.
        let key_started = Instant::now();
        let identity = self.producer.chunk_identity();
        let keys = texts
            .iter()
            .map(|text| self.producer.chunk_key(text))
            .collect::<Vec<_>>();
        report.timing.vector_lookup += key_started.elapsed();
        let mut admitted = false;
        let (_gate, mut rows, misses, checked_at) = loop {
            let wait_started = Instant::now();
            let gate = arena.chunk_batch();
            report.timing.chunk_wait += wait_started.elapsed();
            let lookup_started = Instant::now();
            // Read before any chunk row below is read or stored: a vector
            // remembered in this batch equals its row at least at this epoch.
            // Without an epoch the vectors are still reused, but never vouch
            // for a run.
            let checked_at = store.deletion_epoch().ok().flatten();
            let mut rows = Vec::with_capacity(texts.len());
            let mut misses = Vec::new();
            for (text, key) in texts.iter().zip(&keys) {
                let resident = arena.chunk_vector(key);
                if resident.is_some() {
                    report.chunk_resident_hits += 1;
                }
                let vector = match resident {
                    Some(vector) => Some(vector),
                    None => load_chunk(store, arena, key, &identity, text)
                        .map_err(|error| chunk_store_error(error, report))?
                        .map(|vector| arena.remember_chunk(*key, vector, checked_at)),
                };
                if let Some(vector) = vector {
                    rows.push(Some(BuildEmbeddingRow::Embedded {
                        embedded_text: text.clone(),
                        vector: vector.to_vec(),
                    }));
                    canonical_chunks.push(vector);
                } else {
                    misses.push(text.clone());
                    rows.push(None);
                }
            }
            report.timing.vector_lookup += lookup_started.elapsed();
            if !misses.is_empty() && !admitted {
                // Never wait for cold admission while holding the family gate:
                // a slotted build may need that gate to finish and free a slot.
                // Recheck after admission because a sibling may fill the misses.
                drop(gate);
                before_embed()?;
                admitted = true;
                continue;
            }
            report.chunk_reused += texts.len() - misses.len();
            break (gate, rows, misses, checked_at);
        };
        if !misses.is_empty() {
            report.model_calls += 1;
            report.embedded_texts += misses.len();
            // Validate backend rows and consume adaptive HTTP metadata before
            // caching: shortened or skipped input must not poison the original.
            let embedded = crate::semantic_index::execute_build_embedding_batch(misses, embed)?;
            let store_started = Instant::now();
            let mut payloads = Vec::new();
            let missing_texts = rows
                .iter()
                .zip(&texts)
                .zip(&keys)
                .filter(|((row, _), _)| row.is_none());
            for (((_, text), key), result) in missing_texts.zip(&embedded) {
                if let BuildEmbeddingRow::Embedded {
                    embedded_text,
                    vector,
                } = result
                {
                    if embedded_text == text {
                        payloads.push((*key, encode_chunk_vector(&identity, text, vector)?));
                    }
                }
            }
            let mut outcomes = store
                .put_batch(&payloads)
                .map_err(|error| chunk_store_error(error, report))?
                .into_iter();
            report.timing.store_write += store_started.elapsed();
            let mut embedded = embedded.into_iter();
            for ((row, text), key) in rows.iter_mut().zip(&texts).zip(&keys) {
                if row.is_some() {
                    continue;
                }
                let mut result = embedded.next().expect("validated embedding count");
                if let BuildEmbeddingRow::Embedded {
                    embedded_text,
                    vector,
                } = &mut result
                {
                    if embedded_text == text {
                        let store_started = Instant::now();
                        let cacheable = match outcomes.next().expect("stored exact-text chunk") {
                            Ok(PutOrTouch::Inserted { .. } | PutOrTouch::Reused { .. }) => true,
                            Ok(PutOrTouch::Quarantined) => false,
                            Err(crate::blob_store::v2::StoreError::ConflictingPayload(_)) => {
                                // A different process may have won. Only adopt
                                // its floats after the same exact-text check.
                                if let Some(stored) = load_chunk(store, arena, key, &identity, text)
                                    .map_err(|error| chunk_store_error(error, report))?
                                {
                                    *vector = stored;
                                    true
                                } else {
                                    false
                                }
                            }
                            Err(error) => return Err(chunk_store_error(error, report)),
                        };
                        if cacheable {
                            canonical_chunks.push(arena.remember_chunk(
                                *key,
                                vector.clone(),
                                checked_at,
                            ));
                        }
                        report.timing.store_write += store_started.elapsed();
                    }
                }
                *row = Some(result);
            }
        }
        let store_started = Instant::now();
        store
            .touch(&keys)
            .map_err(|error| chunk_store_error(error, report))?;
        report.timing.store_write += store_started.elapsed();
        Ok(rows
            .into_iter()
            .map(|row| row.expect("resolved chunk"))
            .collect())
    }
}

/// Chunk-cache storage has the same retry policy as per-file storage. Do not
/// turn a permanent store refusal into an embedding outage that retries forever.
fn chunk_store_error(error: crate::blob_store::v2::StoreError, report: &mut FillReport) -> String {
    let reason = error.to_string();
    if !store_error_is_busy(&error) {
        report.store_errors.push(reason.clone());
    }
    reason
}

fn admit_ready(
    view: &Mutex<ViewState>,
    store: &SemanticStore,
    ready: Vec<(
        WorkItem,
        FamilyKey,
        Arc<crate::semantic_index::SemanticVectors>,
    )>,
    producer: &str,
    current: &dyn Fn() -> Snapshot,
    report: &mut FillReport,
) -> Result<(), PlaneError> {
    if ready.is_empty() {
        return Ok(());
    }
    let keys = ready.iter().map(|(_, key, _)| *key).collect::<Vec<_>>();
    protect(view, &keys, store.is_v3())?;
    // GC may have won before the live-pin write. Do not admit a missing row.
    let swept = store
        .touch(&keys)
        .map_err(|error| plane_error(error.to_string()))?
        .missing
        .into_iter()
        .collect::<HashSet<_>>();
    let now = current();
    let mut state = lock(view);
    let installed_before = report.installed;
    for (item, key, run) in ready {
        if swept.contains(&key) {
            continue;
        }
        let live = now.live_state(&item.rel_path).copied();
        match state.fill.admit(
            &Completion { item, key },
            producer,
            live.as_ref(),
            Some(now.generation()),
        ) {
            Admission::Installed => {
                state.fill_runs.insert(key, run);
                report.installed += 1;
            }
            Admission::DroppedContent | Admission::DroppedProducer => report.dropped += 1,
        }
    }
    if report.installed != installed_before {
        crate::cold_build_limiter::progress::advance(report.installed - installed_before);
        state.fill_version += 1;
    }
    Ok(())
}

fn protect(view: &Mutex<ViewState>, keys: &[FamilyKey], v3: bool) -> Result<(), PlaneError> {
    let state = lock(view);
    let pin = if v3 { &state.live_v3 } else { &state.live }.clone();
    drop(state);
    let pin = pin.ok_or_else(|| plane_error("semantic fill has no live pin"))?;
    let result = lock(&pin)
        .protect(keys)
        .map_err(|error| plane_error(error.to_string()));
    result
}

#[cfg(test)]
thread_local! {
    static BEFORE_CLAIM: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_claim_for_test(callback: impl FnOnce() + 'static) {
    BEFORE_CLAIM.with(|hook| *hook.borrow_mut() = Some(Box::new(callback)));
}

#[cfg(test)]
fn run_before_claim_for_test() {
    let callback = BEFORE_CLAIM.with(|hook| hook.borrow_mut().take());
    if let Some(callback) = callback {
        callback();
    }
}

enum VectorSource {
    Base,
    Fill(FamilyKey),
    Failed,
    Pending,
}

/// File names whose content decides walker membership. When one of them
/// differs from the snapshot, the snapshot's membership is not trusted.
const IGNORE_FILE_NAMES: [&str; 3] = super::live_delta::IGNORE_FILE_NAMES;

/// Whether an ignore file on disk differs from what `snapshot` recorded, so
/// its membership may be out of date. It reads only the ignore files, which
/// lets the first query after an ignore edit notice it even before the
/// watcher's reconcile has reached the snapshot.
///
/// Member ignore files are compared with their member content. A root ignore
/// file that is not a member is compared with the record of root ignore files
/// the membership walk ran under: one that ignores itself (a root `.aftignore`
/// ending in `/.aftignore`) is never a member, yet it is no change while its
/// bytes match that record. A snapshot without such a record treats a
/// non-member root ignore file on disk as added, unless the snapshot has a
/// live entry for it.
fn ignore_rules_changed(
    root: &Path,
    snapshot: &Snapshot,
    members: &BTreeMap<RelPath, DiskState>,
) -> bool {
    let is_ignore_file = |path: &RelPath| {
        path.as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|name| {
                IGNORE_FILE_NAMES
                    .iter()
                    .any(|ignore| ignore.as_bytes() == name)
            })
    };
    let on_disk = |path: &RelPath| match rel_path_to_os(path)
        .map(|relative| std::fs::read(root.join(relative)))
    {
        Ok(Ok(bytes)) => DiskState::of_bytes(&bytes),
        _ => DiskState::Absent,
    };
    let recorded_changed = members
        .iter()
        .filter(|(path, _)| is_ignore_file(path))
        .any(|(path, disk)| on_disk(path) != *disk);
    let root_changed = IGNORE_FILE_NAMES.iter().any(|name| {
        let Ok(path) = RelPath::new(name.as_bytes().to_vec()) else {
            return false;
        };
        if members.contains_key(&path) {
            return false;
        }
        match snapshot.root_ignore_files() {
            Some(recorded) => {
                super::live_delta::root_ignore_file_state(root, name)
                    != recorded.get(&path).copied().unwrap_or(DiskState::Absent)
            }
            None => !matches!(snapshot.source(&path), Source::Live(_)) && root.join(name).is_file(),
        }
    });
    recorded_changed || root_changed
}

/// Current walker membership of `root`, paths only.
fn walked_members(root: &Path) -> BTreeSet<RelPath> {
    crate::search_index::walk_project_files(root, &crate::search_index::PathFilters::default())
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(root)
                .ok()
                .and_then(|relative| RelPath::from_os_path(relative).ok())
        })
        .collect()
}

/// The scorable index of one checkout snapshot, and the paths it cannot score.
#[derive(Clone, Debug)]
pub struct SemanticOverlay {
    pub index: Arc<SemanticIndex>,
    pub pending: Vec<PathBuf>,
    pub failed: Vec<PathBuf>,
    pub unvouched: bool,
}

impl SemanticPlane {
    /// A gap path named to the user, in the same native form as results.
    fn absolute(&self, root: &Path, path: &RelPath) -> PathBuf {
        self.overlay_path(path)
            .map(|relative| root.join(relative))
            .unwrap_or_else(|| root.to_path_buf())
    }

    /// Scores `query_vector` against the checkout `snapshot` describes.
    ///
    /// Only current members are scored, each with the vectors of its current
    /// content under this producer: base runs for files unchanged since the
    /// generation, fill-map runs for everything else. Superseded base runs are
    /// tombstoned. Nothing here reads the family store; a generation that is
    /// not resident is an error the caller reports as a gap. Membership comes
    /// from the snapshot unless the watcher cannot vouch for it or an ignore
    /// file changed on disk, in which case the checkout is re-walked.
    pub fn search(
        &self,
        access: &ViewAccess,
        root: &Path,
        snapshot: &Snapshot,
        query_vector: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<SemanticQuery, PlaneError> {
        let overlay = self.overlay(access, root, snapshot)?;
        // Scoring is the resident index's own `search_filtered`, the same
        // function the legacy semantic lane calls, so cosine scores and the
        // exported-symbol multiplier are identical for identical vectors.
        let results = overlay.index.search_filtered(query_vector, top_k, include);
        Ok(SemanticQuery {
            results,
            pending: overlay.pending,
            failed: overlay.failed,
            unvouched: overlay.unvouched,
            unavailable: None,
        })
    }

    /// The index `search` scores for `snapshot`, built from resident runs
    /// only and cached until the snapshot's vector shape or fills change.
    pub fn overlay(
        &self,
        access: &ViewAccess,
        root: &Path,
        snapshot: &Snapshot,
    ) -> Result<SemanticOverlay, PlaneError> {
        let resident = self
            .pinned_resident(access.family(), access.scope(), snapshot)
            .ok_or_else(|| plane_error("semantic generation not resident"))?;
        let arena = self.arena(access.family());
        let view = self.query_view(access, snapshot);
        let mut state = lock(&view);
        let mut members = snapshot.membership();
        let unvouched = snapshot.watcher() != WatcherState::Healthy
            || ignore_rules_changed(root, snapshot, &members);
        let mut pending = BTreeSet::new();
        let mut failed = BTreeSet::new();
        if unvouched {
            let walked = walked_members(root);
            members.retain(|path, _| walked.contains(path));
            // Newly included files have no vectors until a fill sees them.
            pending.extend(
                walked
                    .into_iter()
                    .filter(|path| applies_to(path) && !members.contains_key(path)),
            );
        }
        let intent = snapshot.pending_intent().cloned().collect::<BTreeSet<_>>();
        let mut via_base = HashSet::new();
        let mut replacements = Vec::new();
        let mut budget_limited = self.producer.caps.total_chars
            >= EmbedTextCaps::default().total_chars
            || self.file_summary_format > 0;
        let mut shape = blake3::Hasher::new();
        for (path, disk) in &members {
            let DiskState::Present { content, .. } = disk else {
                continue;
            };
            if !applies_to(path) {
                continue;
            }
            // An acknowledged write not yet applied has unknown bytes: no
            // stored vector can be trusted for it.
            if intent.contains(path) {
                pending.insert(path.clone());
                continue;
            }
            match Self::vector_source(
                &state,
                &resident,
                snapshot,
                path,
                *content,
                matches!(access, ViewAccess::Owner(_)),
            ) {
                VectorSource::Base => {
                    if resident.legacy_summaries.contains(path) {
                        budget_limited = false;
                    }
                    via_base.insert(path.clone());
                }
                VectorSource::Fill(key) => match arena.get(&key) {
                    Some(run) => {
                        shape.update(path.as_bytes());
                        shape.update(&[0]);
                        shape.update(key.as_bytes());
                        replacements.push(run);
                    }
                    None => {
                        pending.insert(path.clone());
                    }
                },
                VectorSource::Failed => {
                    failed.insert(path.clone());
                }
                VectorSource::Pending => {
                    pending.insert(path.clone());
                }
            }
        }
        let tombstones = resident
            .members
            .keys()
            .filter(|path| !via_base.contains(*path))
            .filter_map(|path| self.overlay_path(path))
            .collect::<HashSet<_>>();
        let mut sorted_tombstones = tombstones.iter().collect::<Vec<_>>();
        sorted_tombstones.sort();
        shape.update(&[1]);
        for path in sorted_tombstones {
            shape.update(path.as_os_str().as_encoded_bytes());
            shape.update(&[0]);
        }
        let cache_key = (
            snapshot.generation().resident_key(),
            *shape.finalize().as_bytes(),
            state.fill_version,
            unvouched,
        );
        let index = match state.cache.as_ref() {
            Some((key, index)) if *key == cache_key => Arc::clone(index),
            _ => {
                self.overlay_builds
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut index = SemanticIndex::for_view(
                    root.to_path_buf(),
                    &resident.base,
                    tombstones,
                    &replacements,
                );
                index.set_summary_budget_limited(budget_limited);
                let index = Arc::new(index);
                state.cache = Some((cache_key, Arc::clone(&index)));
                index
            }
        };
        drop(state);
        Ok(SemanticOverlay {
            index,
            pending: pending
                .iter()
                .map(|path| self.absolute(root, path))
                .collect(),
            failed: failed
                .iter()
                .map(|path| self.absolute(root, path))
                .collect(),
            unvouched,
        })
    }

    /// The index `overlay` last built for the owner's view, whatever snapshot
    /// it was built for. Borrowers use `cached_index_for` to name their reader
    /// binding. Never builds one.
    pub fn cached_index(&self, access: &ViewAccess) -> Option<Arc<SemanticIndex>> {
        let view = lock(&self.views)
            .get(&(access.family().to_owned(), access.scope().to_owned()))
            .cloned()?;
        let state = lock(&view);
        state.cache.as_ref().map(|(_, index)| Arc::clone(index))
    }

    /// The last overlay cached in this snapshot's reader binding. Borrowers
    /// have private caches, so a scope-only lookup could observe an owner's
    /// unpublished fills or a different reader's generation. Never builds one.
    pub fn cached_index_for(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
    ) -> Option<Arc<SemanticIndex>> {
        let view = self.query_view(access, snapshot);
        let state = lock(&view);
        state.cache.as_ref().map(|(_, index)| Arc::clone(index))
    }

    /// How many scored indexes `overlay` has built.
    pub fn overlay_builds(&self) -> u64 {
        self.overlay_builds
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// This checkout's own resident semantic bytes: the tombstones of its last
    /// overlay and its handles to the replacement runs. Base and replacement
    /// runs live in the family arena and are counted there once.
    pub fn private_memory(&self, access: &ViewAccess) -> u64 {
        let view = self.view(access);
        let state = lock(&view);
        state.cache.as_ref().map_or(0, |(_, index)| {
            index.estimated_memory().estimated_bytes.unwrap_or(0)
        })
    }

    /// [`Self::private_memory`] without waiting: `None` while the view's
    /// state is locked (an overlay build holds it), `Some(0)` for a view this
    /// plane does not hold. The memory census uses this so it never blocks.
    pub fn try_private_memory(&self, access: &ViewAccess) -> Option<u64> {
        let view = {
            let views = try_lock(&self.views)?;
            match views.get(&(access.family().to_owned(), access.scope().to_owned())) {
                Some(view) => Arc::clone(view),
                None => return Some(0),
            }
        };
        let state = try_lock(&view)?;
        Some(state.cache.as_ref().map_or(0, |(_, index)| {
            index.estimated_memory().estimated_bytes.unwrap_or(0)
        }))
    }

    /// Outstanding semantic work for the checkout, as a fill would see it.
    pub fn pending_work(&self, access: &ViewAccess, snapshot: &Snapshot) -> Vec<WorkItem> {
        let Some(resident) = self.pinned_resident(access.family(), access.scope(), snapshot) else {
            return Vec::new();
        };
        let view = self.query_view(access, snapshot);
        let state = lock(&view);
        self.work_items(
            &state,
            &resident,
            snapshot,
            matches!(access, ViewAccess::Owner(_)),
        )
    }

    /// Remove mutable scope caches and lookup entries when a checkout unbinds.
    /// Query bundles retain their readers and the ViewState whose live pin
    /// protects unpublished fill keys until the last query drops. Only
    /// unreferenced arena runs can be trimmed here.
    pub fn unbind(&self, access: &ViewAccess) {
        let family = access.family().to_owned();
        let scope = access.scope().to_owned();
        let retired_view = lock(&self.views).remove(&(family.clone(), scope.clone()));
        let mut retired_residents = Vec::new();
        {
            let mut residents = lock(&self.residents);
            residents.retain(|(f, s, _), resident| {
                if *f == family && *s == scope {
                    retired_residents.push(resident.clone());
                    false
                } else {
                    true
                }
            });
        }
        drop(retired_view);
        drop(retired_residents);
        let arenas = lock(&self.arenas).values().cloned().collect::<Vec<_>>();
        for arena in arenas {
            arena.trim();
        }
    }
}

impl PlaneAdapter for SemanticPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Semantic
    }

    fn producer(&self) -> String {
        self.producer.id()
    }

    fn applies_to(&self, rel_path: &RelPath) -> bool {
        applies_to(rel_path)
    }

    /// Admits the generation's runs into the family arena (this is the only
    /// place, besides a fill's own puts, that decodes store rows). Live fills
    /// remain protected until the separate durable-publication callback.
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<(), PlaneError> {
        self.open_resident(access, generation).map(|_| ())
    }

    fn open_reader(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<Option<super::snapshot::PlaneAttachment>, PlaneError> {
        let handle = self.open_resident(access, generation)?;
        Ok(Some(handle))
    }

    fn bind_reader(
        &self,
        access: &ViewAccess,
        _: &Arc<ComposedGeneration>,
        handle: Option<super::snapshot::PlaneAttachment>,
    ) -> Result<Option<super::snapshot::PlaneAttachment>, PlaneError> {
        let resident = handle
            .and_then(|handle| handle.downcast::<Resident>().ok())
            .ok_or_else(|| plane_error("semantic reader handle missing"))?;
        let state = match access {
            ViewAccess::Owner(_) => self.view(access),
            ViewAccess::Reader { .. } => Arc::default(),
        };
        let reader = Arc::new(SemanticReader { resident, state });
        Ok(Some(reader))
    }

    fn generation_published(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
        current: &dyn Fn() -> Snapshot,
    ) -> Result<(), PlaneError> {
        let ViewAccess::Owner(owner) = access else {
            return Ok(());
        };
        let view = self.view(access);
        let (version, mut fill, mut runs, failed) = {
            let mut state = lock(&view);
            if state.active_fills != 0 {
                return Ok(());
            }
            if state.fill.is_empty() {
                let retired = state.live.take();
                drop(state);
                drop(retired);
                return Ok(());
            }
            (
                state.fill_version,
                state.fill.clone(),
                state.fill_runs.clone(),
                state.failed.clone(),
            )
        };
        let before = fill.len();
        fill.trim_folded(generation);
        fill.retain_current(&current());
        if fill.len() == before {
            return Ok(());
        }
        let keep = fill.keys().copied().collect::<HashSet<_>>();
        runs.retain(|key, _| keep.contains(key));
        let pin = if keep.is_empty() {
            None
        } else {
            let mut pin = crate::pins::LivePin::create(owner)
                .map_err(|error| plane_error(error.to_string()))?;
            pin.protect(&keep.iter().copied().collect::<Vec<_>>())
                .map_err(|error| plane_error(error.to_string()))?;
            Some(Arc::new(Mutex::new(pin)))
        };
        // Held readers keep the previous maps, run owners, cache and pin. Only
        // new bindings see the trimmed copy, and pin I/O happens outside state locks.
        let key = (access.family().to_owned(), access.scope().to_owned());
        let retired = {
            let mut views = lock(&self.views);
            if views
                .get(&key)
                .is_some_and(|existing| Arc::ptr_eq(existing, &view))
            {
                let mut state = lock(&view);
                if state.active_fills == 0 && state.fill_version == version {
                    let replacement = Arc::new(Mutex::new(ViewState {
                        fill,
                        fill_runs: runs,
                        fill_version: version + 1,
                        failed,
                        tracker: std::mem::take(&mut state.tracker),
                        live: pin.clone(),
                        live_v3: state.live_v3.clone(),
                        active_fills: 0,
                        cache: None,
                    }));
                    views.insert(key, replacement)
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(retired);
        drop(pin);
        Ok(())
    }

    fn release_generation(&self, access: &ViewAccess, generation: &str) {
        let retired = lock(&self.residents).remove(&(
            access.family().to_owned(),
            access.scope().to_owned(),
            generation.to_owned(),
        ));
        drop(retired);
        self.arena(access.family()).trim();
    }

    fn readiness(&self, access: &ViewAccess, snapshot: &Snapshot) -> PlaneReadiness {
        self.readiness_for(
            access.family(),
            access.scope(),
            snapshot,
            matches!(access, ViewAccess::Owner(_)),
        )
    }
}

impl SemanticPlane {
    fn readiness_for(
        &self,
        family: &str,
        scope: &str,
        snapshot: &Snapshot,
        repair_summaries: bool,
    ) -> PlaneReadiness {
        let Some(resident) = self.pinned_resident(family, scope, snapshot) else {
            return PlaneReadiness::Building;
        };
        let view = self.query_view_for(family, scope, snapshot);
        let state = lock(&view);
        let mut pending = 0;
        let mut failed = 0;
        for (path, disk) in snapshot.membership() {
            let DiskState::Present { content, .. } = disk else {
                continue;
            };
            if !applies_to(&path) {
                continue;
            }
            if Self::summary_repair_pending(
                &state,
                &resident,
                snapshot,
                &path,
                content,
                repair_summaries,
            ) {
                pending += 1;
                continue;
            }
            match Self::vector_source(
                &state,
                &resident,
                snapshot,
                &path,
                content,
                repair_summaries,
            ) {
                VectorSource::Pending => pending += 1,
                VectorSource::Failed => failed += 1,
                VectorSource::Base | VectorSource::Fill(_) => {}
            }
        }
        PlaneReadiness::Ready { pending, failed }
    }
}

impl super::first_load::CompositePlane for SemanticPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Semantic
    }

    /// A fresh base may carry the semantic plane: its entries start pending
    /// and the checkout holding the base's build claim fills them later,
    /// publishing readiness successors (see `CheckoutSemantic::fill_base`).
    /// Nothing is embedded during the bind, so completion here is a no-op.
    fn completes_fresh_base(&self) -> bool {
        true
    }

    fn defers_base_completion(&self) -> bool {
        true
    }

    fn applies_to(&self, rel_path: &RelPath) -> bool {
        applies_to(rel_path)
    }
    fn attachment_key(&self, attachment: &super::snapshot::PlaneAttachment) -> Option<FamilyKey> {
        attachment
            .downcast_ref::<SemanticAttachment>()
            .map(|a| a.key)
    }
    fn cached_attachment(
        &self,
        content: ContentHash,
        key: FamilyKey,
    ) -> super::snapshot::PlaneAttachment {
        Arc::new(SemanticAttachment {
            content,
            producer: self.producer.id(),
            key,
        })
    }

    fn attachment(
        &self,
        rel_path: &RelPath,
        bytes: &[u8],
    ) -> Result<super::snapshot::PlaneAttachment, PlaneError> {
        Ok(Arc::new(SemanticAttachment {
            content: ContentHash::of(bytes),
            producer: self.producer.id(),
            key: FamilyKey::from(&self.producer.full_key_for_summary_format(
                bytes,
                rel_path,
                self.file_summary_format,
            )),
        }))
    }

    /// Records each applicable entry's semantic state without embedding
    /// anything: publication never waits for the model. An entry is `Ready`
    /// when its generation already had current vectors, when this checkout's
    /// fill map holds them, or when its content key is already stored in the
    /// family (by another view or an earlier session). A deterministic
    /// failure for this producer is `Failed`. Everything else is `Pending`,
    /// which keeps it queued across folds and restarts until a fill runs.
    fn materialize(
        &self,
        owner: &ViewRegistration,
        _generation: &str,
        _snapshot: &Snapshot,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut ManifestV2,
        live: &mut crate::pins::LivePin,
        _seed_derived: bool,
    ) -> Result<(), PlaneError> {
        let store = owner
            .open_store(FamilyPlane::Semantic)
            .map(SemanticStore::V2)
            .map_err(|error| plane_error(error.to_string()))?;
        self.materialize_with(owner, observed, manifest, live, &store)
    }

    /// The v3 counterpart: the same readiness rules, with presence checks and
    /// touches against the family's v3 semantic store. Only the delta's own
    /// entries are materialized; inherited base entries belong to the base.
    fn materialize_v3(
        &self,
        owner: &ViewRegistration,
        route: &super::snapshot::V3Route,
        _staging: &str,
        _base: &Arc<ComposedGeneration>,
        observed: &BTreeMap<RelPath, LiveEntry>,
        delta: &mut ManifestV2,
        live: &mut crate::pins::LivePin,
    ) -> Result<(), PlaneError> {
        self.materialize_with(
            owner,
            observed,
            delta,
            live,
            &SemanticStore::V3(route.clone(), std::sync::OnceLock::new()),
        )
    }

    fn finish_generation(
        &self,
        _owner: &ViewRegistration,
        _staging: &str,
        _published: &str,
    ) -> Result<(), PlaneError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    include!("semantic_read_tests.rs");
    use super::*;
    use crate::views::contracts::PlaneLoader;
    use crate::views::first_load::{
        CheckoutDriver, CompositePlane, ConfiguredMembershipWalker, MembershipWalker, QueryState,
        SiblingLoader,
    };
    use crate::views::manifest_v2::Producers;
    use crate::views::registry::FamilyRegistry;
    use crate::views::snapshot::LiveDelta;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FILES: &[(&str, &str)] = &[
        (
            "src/alpha.rs",
            "pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n\npub fn alpha_label() -> &'static str {\n    \"alpha\"\n}\n",
        ),
        (
            "src/beta.rs",
            "pub struct BetaCache {\n    entries: Vec<String>,\n}\n\nimpl BetaCache {\n    pub fn insert(&mut self, value: String) {\n        self.entries.push(value);\n    }\n}\n",
        ),
        (
            "src/gamma.rs",
            "fn gamma_parse(input: &str) -> Option<u64> {\n    input.trim().parse().ok()\n}\n",
        ),
        ("notes.txt", "not a semantic source\n"),
    ];

    /// A deterministic model: the vector is a hash of the model name and the
    /// text, so equal texts get equal vectors and a model change changes all.
    fn vector(model: &str, text: &str) -> Vec<f32> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(model.as_bytes());
        hasher.update(&[0]);
        hasher.update(text.as_bytes());
        hasher.finalize().as_bytes()[..16]
            .iter()
            .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
            .collect()
    }

    #[derive(Default)]
    struct Model {
        calls: AtomicUsize,
        texts: AtomicUsize,
    }

    impl Model {
        fn embed(&self, model: &str, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts.iter().map(|text| vector(model, text)).collect())
        }

        fn texts(&self) -> usize {
            self.texts.load(Ordering::SeqCst)
        }
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    fn new_plane(storage: &Path, model: &str) -> Arc<SemanticPlane> {
        Arc::new(SemanticPlane::new(
            storage.to_path_buf(),
            SemanticProducer::current(model, EmbedTextCaps::default()),
        ))
    }

    fn live_pin_path(view: &Mutex<ViewState>) -> std::path::PathBuf {
        let pin = lock(view).live.clone().unwrap();
        let path = lock(&pin).keys_path().to_path_buf();
        path
    }

    #[test]
    fn opening_draft_readers_keeps_unpublished_fill_protection() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "draft-fill", root.path(), &plane);
        let snapshot = checkout.load();
        let view = plane.view(&checkout.access);
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        let content = ContentHash::of(b"file content");
        let key = plane.producer.key(b"file content", &path);
        let completion = super::super::readiness::Completion {
            item: WorkItem {
                plane: FamilyPlane::Semantic,
                rel_path: path.clone(),
                content,
                producer: plane.producer.id(),
            },
            key,
        };
        let disk = DiskState::Present { content, size: 12 };
        {
            let mut state = lock(&view);
            assert_eq!(
                state
                    .fill
                    .admit(&completion, &plane.producer.id(), Some(&disk), None),
                Admission::Installed
            );
            state.live = Some(Arc::new(Mutex::new(
                crate::pins::LivePin::create(&checkout.owner).unwrap(),
            )));
        }
        protect(&view, &[key], false).unwrap();
        let mut manifest =
            super::super::manifest_v2::ManifestV2::new(snapshot.generation().header().clone());
        manifest
            .insert(
                path,
                super::super::manifest_v2::EntryV2::regular(
                    content,
                    12,
                    super::super::manifest_v2::EntryPlanes {
                        semantic: Some(PlaneState::ready(&key)),
                        ..Default::default()
                    },
                ),
            )
            .unwrap();
        let draft = Arc::new(ComposedGeneration::new("unpublished-draft", manifest, None));
        let _draft_reader = plane.open_reader(&checkout.access, &draft).unwrap();
        assert_eq!(lock(&view).fill.len(), 1);
        assert!(live_pin_path(&view).exists());
        assert_eq!(lock(&plane.view(&checkout.access)).fill.len(), 1);
        plane
            .generation_published(&checkout.access, &draft, &|| snapshot.clone())
            .unwrap();
        assert!(lock(&plane.view(&checkout.access)).fill.is_empty());
        assert_eq!(
            lock(&view).fill.len(),
            1,
            "held reader state was trimmed in place"
        );
    }

    #[test]
    fn publication_cleanup_keeps_keys_of_an_active_fill() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "active-fill", root.path(), &plane);
        let snapshot = checkout.load();
        let view = plane.view(&checkout.access);
        let active = ActiveFill::new(&view);
        lock(&view).live = Some(Arc::new(Mutex::new(
            crate::pins::LivePin::create(&checkout.owner).unwrap(),
        )));
        let key = plane
            .producer
            .key(b"pending bytes", &RelPath::new(b"new.rs".to_vec()).unwrap());
        protect(&view, &[key], false).unwrap();
        let path = live_pin_path(&view);
        plane
            .generation_published(&checkout.access, snapshot.generation(), &|| {
                snapshot.clone()
            })
            .unwrap();
        assert!(
            path.exists(),
            "publication must not release a pin still needed by a fill"
        );
        drop(active);
        plane
            .generation_published(&checkout.access, snapshot.generation(), &|| {
                snapshot.clone()
            })
            .unwrap();
        assert!(
            !path.exists(),
            "the idle pin may be released after publication"
        );
    }

    #[test]
    fn a_real_fill_registers_before_lookup_and_publication_cleanup() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        write_tree(
            root.path(),
            &[("src/alpha.rs", "fn early_fill_changed() {}\n")],
        );
        checkout
            .driver
            .record_absolute_change(&root.path().join("src/alpha.rs"));
        let snapshot = checkout.load();
        let checking = plane.clone();
        let access = checkout.access.clone();
        let during = snapshot.clone();
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = reached.clone();
        before_claim_for_test(move || {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            let state = checking.query_view(&access, &during);
            assert!(
                lock(&state).active_fills > 0,
                "publication could remove the setup pin before fill registration"
            );
            checking
                .generation_published(&access, during.generation(), &|| during.clone())
                .unwrap();
            assert!(lock(&state).live.is_some());
        });
        assert_eq!(
            checkout
                .fill(&snapshot, &Model::default(), "model-a")
                .installed,
            1
        );
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            "the fixture did not reach the real pre-claim interleaving"
        );
    }

    #[test]
    fn held_filled_snapshot_survives_publication_and_trim_without_prior_overlay() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let edited = "pub fn alpha_replaced() -> u8 { 7 }\n";
        write_tree(root.path(), &[("src/alpha.rs", edited)]);
        checkout
            .driver
            .record_absolute_change(&root.path().join("src/alpha.rs"));
        let held = checkout.load();
        assert_eq!(
            checkout.fill(&held, &Model::default(), "model-a").installed,
            1
        );
        let file = RelPath::new(b"src/alpha.rs".to_vec()).unwrap();
        let key = plane.producer.key(edited.as_bytes(), &file);
        let arena = plane.arena("family");
        let payload_work = (arena.decode_count(), arena.store_reads());
        let pin_path = live_pin_path(&plane.query_view(&checkout.access, &held));
        arena.trim();
        assert!(
            arena.get(&key).is_some(),
            "a never-overlaid admitted fill had no strong owner"
        );
        let published = checkout.load();
        assert!(lock(&plane.view(&checkout.access)).fill.is_empty());
        assert!(
            pin_path.exists(),
            "publication removed protection still needed by a held reader"
        );
        let answer = checkout.query(&held, "model-a", "alpha");
        assert!(
            answer.complete(),
            "held fill was retired before its reader: {answer:?}"
        );
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "model-a", "alpha")
        );
        assert_eq!(
            (arena.decode_count(), arena.store_reads()),
            payload_work,
            "old reader reopened store rows"
        );
        drop(published);
        plane.unbind(&checkout.access);
        drop(checkout);
        arena.trim();
        assert!(arena.get(&key).is_some());
        assert!(pin_path.exists());
        drop(held);
        arena.trim();
        assert!(arena.get(&key).is_none());
        assert!(!pin_path.exists());
    }

    #[test]
    fn admission_propagates_semantic_payload_decode_errors() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model");
        let checkout = Checkout::open(storage.path(), "bad-payload", root.path(), &plane);
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let bytes = b"pub fn alpha() {}";
        let key = plane.producer.key(bytes, &path);
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        store
            .put_or_touch(&key, b"invalid semantic payload")
            .unwrap();
        let mut manifest = ManifestV2::new(crate::views::manifest_v2::ManifestHeader {
            producers: Producers {
                trigram: "test".into(),
                semantic: Some(plane.producer.id()),
                callgraph: "test".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        manifest
            .insert(
                path,
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    crate::views::manifest_v2::EntryPlanes {
                        semantic: Some(PlaneState::ready(&key)),
                        trigram: None,
                        callgraph: None,
                    },
                ),
            )
            .unwrap();
        let generation = ComposedGeneration::new("bad", manifest, None);
        assert!(plane.admit(&checkout.access, &generation).is_err());
    }

    #[test]
    fn missing_conflict_winner_is_accounted_for_and_retried() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let bytes = "pub fn alpha() -> u32 { 42 }";
        write_tree(root.path(), &[("a.rs", bytes)]);
        let plane = new_plane(storage.path(), "model");
        let checkout = Checkout::open(storage.path(), "conflict", root.path(), &plane);
        let snapshot = checkout.load();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let key = plane.producer.key(bytes.as_bytes(), &path);
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        let store_path = store.path().to_path_buf();
        *lock(&plane.after_conflict) = Some(Box::new(move || {
            rusqlite::Connection::open(store_path)
                .unwrap()
                .execute(
                    "DELETE FROM blob_payloads WHERE full_key = ?1",
                    [key.as_bytes().as_slice()],
                )
                .unwrap();
        }));
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| {
                    store
                        .put_or_touch(&key, b"another process's payload")
                        .unwrap();
                    Ok(texts.iter().map(|text| vector("model", text)).collect())
                },
                &|| snapshot.clone(),
            )
            .unwrap();
        assert_eq!(report.deferred, 1);
        assert_eq!(report.embedded_keys + report.stored_hits + report.failed, 0);
        assert_eq!(
            checkout
                .fill(&snapshot, &Model::default(), "model")
                .embedded_keys,
            1
        );
    }

    struct Checkout {
        root: PathBuf,
        owner: ViewRegistration,
        access: ViewAccess,
        driver: Arc<CheckoutDriver>,
        loader: SiblingLoader,
        plane: Arc<SemanticPlane>,
    }

    impl Checkout {
        fn open(storage: &Path, scope: &str, root: &Path, plane: &Arc<SemanticPlane>) -> Self {
            let registry = FamilyRegistry::open(storage, "family").unwrap();
            let owner = registry
                .register_view_with_storage(registry.storage(), scope, root)
                .unwrap();
            Self::with_owner(root, owner, plane)
        }

        fn with_owner(root: &Path, owner: ViewRegistration, plane: &Arc<SemanticPlane>) -> Self {
            let composite: Arc<dyn CompositePlane> = plane.clone();
            let adapter: Arc<dyn PlaneAdapter> = plane.clone();
            let driver = Arc::new(
                CheckoutDriver::new(
                    owner.clone(),
                    Producers {
                        trigram: "trigram".into(),
                        semantic: Some(plane.semantic_producer().id()),
                        callgraph: "callgraph".into(),
                    },
                    None,
                    Arc::new(ConfiguredMembershipWalker),
                    vec![composite],
                )
                .with_adapters(vec![adapter.clone()]),
            );
            let loader = SiblingLoader::new(driver.clone(), vec![adapter]);
            Self {
                root: root.to_path_buf(),
                access: ViewAccess::Owner(owner.clone()),
                owner,
                driver,
                loader,
                plane: plane.clone(),
            }
        }

        /// Loads the checkout, publishing and installing its own generation.
        /// Called again it folds installed fills into a new generation.
        fn load(&self) -> Snapshot {
            self.loader.load(&self.access).unwrap().snapshot
        }

        fn installed(&self) -> Snapshot {
            self.driver
                .installed_state(&self.access, FamilyPlane::Semantic)
                .0
        }

        fn fill(&self, snapshot: &Snapshot, model: &Model, name: &str) -> FillReport {
            let current = snapshot.clone();
            self.plane
                .fill(
                    &self.owner,
                    snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed(name, texts),
                    &move || current.clone(),
                )
                .unwrap()
        }

        fn query(&self, snapshot: &Snapshot, model: &str, text: &str) -> SemanticQuery {
            self.plane
                .search(
                    &self.access,
                    &self.root,
                    snapshot,
                    &vector(model, text),
                    100,
                    &|_| true,
                )
                .unwrap()
        }
    }

    fn legacy_summary_checkout(
        storage: &Path,
        root: &Path,
        caps: EmbedTextCaps,
    ) -> (Checkout, String, Vec<f32>) {
        let source = "//! A long module description that deliberately exceeds the configured small embedding budget.\npub fn exported() {\n    42;\n}\n";
        let relative = Path::new("src").join("summary.rs");
        write_tree(root, &[("src/summary.rs", source)]);
        let producer = SemanticProducer::current("model-a", caps);
        let mut plane = SemanticPlane::new(storage.to_path_buf(), producer.clone());
        plane.file_summary_format = 0;
        assert_eq!(plane.file_summary_format, 0);
        let plane = Arc::new(plane);
        let checkout = Checkout::open(storage, "summary-scope", root, &plane);
        let mut chunks =
            crate::semantic_index::chunk_view_file(root, &relative, source.as_bytes(), caps)
                .unwrap()
                .unwrap();
        let old = crate::semantic_index::build_file_summary_chunk(
            &root.join(&relative),
            root,
            source,
            &["exported"],
            &[Some("pub fn exported()")],
        );
        chunks
            .iter_mut()
            .find(|chunk| chunk.kind == crate::symbols::SymbolKind::FileSummary)
            .unwrap()
            .embed_text = old.embed_text;
        let symbol_text = chunks
            .iter()
            .find(|chunk| chunk.name == "exported")
            .unwrap()
            .embed_text
            .clone();
        let symbol = vector("model-a", &symbol_text);
        let mut runs = crate::semantic_index::embed_view_files(
            vec![chunks],
            &mut |texts| Model::default().embed("model-a", texts),
            64,
        )
        .unwrap();
        let path = RelPath::new(b"src/summary.rs".to_vec()).unwrap();
        let key = FamilyKey::from(
            &SemanticKey::from_bytes(
                source.as_bytes(),
                path.as_bytes(),
                &producer.chunker_version,
                &producer.template_version,
                &producer.model_fingerprint,
            )
            .full_key(),
        );
        checkout
            .owner
            .open_store(FamilyPlane::Semantic)
            .unwrap()
            .put_or_touch(
                &key,
                &runs.remove(0).encode_view_payload(&producer.payload()),
            )
            .unwrap();
        let snapshot = checkout.load();
        assert!(!checkout
            .query(&snapshot, "model-a", "module")
            .results
            .is_empty());
        (checkout, symbol_text, symbol)
    }

    #[test]
    fn audit10_9_views_small_cap_load_repairs_only_summaries_and_borrowers_adopt() {
        for reader_first in [true, false] {
            let storage = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let caps = EmbedTextCaps {
                total_chars: 70,
                ..EmbedTextCaps::default()
            };
            let (legacy, symbol_text, symbol) =
                legacy_summary_checkout(storage.path(), root.path(), caps);
            let plane = Arc::new(SemanticPlane::new(
                storage.path().to_path_buf(),
                SemanticProducer::current("model-a", caps),
            ));
            assert_eq!(plane.producer.id(), legacy.plane.producer.id());
            assert_eq!(
                plane.producer.chunk_identity(),
                legacy.plane.producer.chunk_identity()
            );
            let checkout = Checkout::with_owner(root.path(), legacy.owner.clone(), &plane);
            if !reader_first {
                checkout.load();
            }
            let chunk_key = plane.producer.chunk_key(&symbol_text);
            let canonical = symbol.iter().map(|value| value * 0.99).collect::<Vec<_>>();
            let canonical_payload =
                encode_chunk_vector(&plane.producer.chunk_identity(), &symbol_text, &canonical)
                    .unwrap();
            let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
            store.put_or_touch(&chunk_key, &canonical_payload).unwrap();
            let reader = FamilyRegistry::open(storage.path(), "family")
                .unwrap()
                .register_reader("summary-reader")
                .unwrap();
            let access = ViewAccess::Reader {
                registration: Arc::new(reader),
                scope: "summary-scope".to_string(),
            };
            let before = checkout.loader.load(&access).unwrap().snapshot;
            let query = plane
                .search(
                    &access,
                    root.path(),
                    &before,
                    &vector("model-a", "module"),
                    100,
                    &|_| true,
                )
                .unwrap();
            assert!(query.complete());
            assert!(!query.results.is_empty());
            let old_summary = plane
                .cached_index_for(&access, &before)
                .unwrap()
                .entry_vectors_for_test()
                .into_iter()
                .find(|(_, name, _)| name == "summary")
                .unwrap()
                .2;
            let snapshot = checkout.load();
            let current = snapshot.clone();
            let mut embedded = Vec::new();
            let report = plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| {
                        embedded.extend(texts.clone());
                        Ok(texts.iter().map(|text| vector("model-a", text)).collect())
                    },
                    &move || current.clone(),
                )
                .unwrap();
            assert_eq!(
                embedded.len(),
                1,
                "only the legacy file summary may be embedded: {report:?}"
            );
            assert!(embedded[0].contains("kind:file-summary"));
            assert_eq!(embedded[0].chars().count(), 70);
            let published = checkout.load();
            assert!(!checkout
                .query(&published, "model-a", "module")
                .results
                .is_empty());
            let index = plane.cached_index(&checkout.access).unwrap();
            assert_eq!(
                index
                    .entry_vectors_for_test()
                    .into_iter()
                    .find(|(_, name, _)| name == "exported")
                    .unwrap()
                    .2,
                symbol
            );
            assert_eq!(store.get(&chunk_key).unwrap().unwrap(), canonical_payload);
            let after = checkout.loader.load(&access).unwrap().snapshot;
            let query = plane
                .search(
                    &access,
                    root.path(),
                    &after,
                    &vector("model-a", "module"),
                    100,
                    &|_| true,
                )
                .unwrap();
            assert!(query.complete());
            assert!(!query.results.is_empty());
            let new_summary = plane
                .cached_index_for(&access, &after)
                .unwrap()
                .entry_vectors_for_test()
                .into_iter()
                .find(|(_, name, _)| name == "summary")
                .unwrap()
                .2;
            assert_ne!(old_summary, new_summary);
            assert_eq!(new_summary, vector("model-a", &embedded[0]));
            assert_eq!(
                plane
                    .cached_index_for(&access, &before)
                    .unwrap()
                    .entry_vectors_for_test()
                    .into_iter()
                    .find(|(_, name, _)| name == "summary")
                    .unwrap()
                    .2,
                old_summary,
                "a newer reader binding must not replace the old query's cache"
            );
            assert_ne!(before.generation().name(), after.generation().name());
            assert!(plane.pending_work(&checkout.access, &published).is_empty());
            let current = published.clone();
            plane
                .fill(
                    &checkout.owner,
                    &published,
                    FillBudget::default(),
                    &mut |_| panic!("repaired summaries must not rebuild"),
                    &move || current.clone(),
                )
                .unwrap();
        }
    }

    #[test]
    fn audit10_9_views_default_cap_load_does_not_rebuild_summaries() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (legacy, _, _) =
            legacy_summary_checkout(storage.path(), root.path(), EmbedTextCaps::default());
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::with_owner(root.path(), legacy.owner.clone(), &plane);
        let snapshot = checkout.load();
        let current = snapshot.clone();
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |_| panic!("default-cap indexes need no summary repair"),
                &move || current.clone(),
            )
            .unwrap();
        assert_eq!(report.embedded_texts, 0);
        assert_eq!(report.source_reads, 0);
        assert!(plane.pending_work(&checkout.access, &snapshot).is_empty());
    }

    #[test]
    fn audit10_9_views_repair_preserves_distinct_vectors_for_duplicate_inputs() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = "//! A module description longer than the tiny input budget.\npub fn exported_one() {\n    42;\n}\npub fn exported_two() {\n    43;\n}\n";
        write_tree(root.path(), &[("src/summary.rs", source)]);
        let path = RelPath::new(b"src/summary.rs".to_vec()).unwrap();
        let relative = Path::new("src").join("summary.rs");
        let caps = EmbedTextCaps {
            total_chars: 10,
            ..EmbedTextCaps::default()
        };
        let producer = SemanticProducer::current("model-a", caps);
        let mut legacy_plane = SemanticPlane::new(storage.path().to_path_buf(), producer.clone());
        legacy_plane.file_summary_format = 0;
        let legacy_plane = Arc::new(legacy_plane);
        let legacy = Checkout::open(
            storage.path(),
            "duplicate-scope",
            root.path(),
            &legacy_plane,
        );
        let mut chunks =
            crate::semantic_index::chunk_view_file(root.path(), &relative, source.as_bytes(), caps)
                .unwrap()
                .unwrap();
        let symbol_inputs = chunks
            .iter()
            .filter(|chunk| chunk.kind != crate::symbols::SymbolKind::FileSummary)
            .map(|chunk| chunk.embed_text.clone())
            .collect::<Vec<_>>();
        assert_eq!(symbol_inputs.len(), 2);
        assert_eq!(symbol_inputs[0], symbol_inputs[1]);
        chunks
            .iter_mut()
            .find(|chunk| chunk.kind == crate::symbols::SymbolKind::FileSummary)
            .unwrap()
            .embed_text = crate::semantic_index::build_file_summary_chunk(
            &root.path().join(&relative),
            root.path(),
            source,
            &["exported_one", "exported_two"],
            &[None, None],
        )
        .embed_text;
        let runs = crate::semantic_index::embed_view_files(
            vec![chunks],
            &mut |texts: Vec<String>| {
                Ok(texts
                    .iter()
                    .enumerate()
                    .map(|(position, text)| {
                        vector("model-a", text)
                            .into_iter()
                            .map(|value| value * (position + 1) as f32)
                            .collect()
                    })
                    .collect())
            },
            64,
        )
        .unwrap();
        legacy
            .owner
            .open_store(FamilyPlane::Semantic)
            .unwrap()
            .put_or_touch(
                &producer.legacy_key(source.as_bytes(), &path),
                &runs[0].encode_view_payload(&producer.payload()),
            )
            .unwrap();
        let snapshot = legacy.load();
        legacy.query(&snapshot, "model-a", "module");
        let symbols = |index: Arc<SemanticIndex>| {
            index
                .entry_vectors_for_test()
                .into_iter()
                .filter(|(_, name, _)| name != "summary")
                .map(|(_, name, vector)| (name, vector))
                .collect::<BTreeMap<_, _>>()
        };
        let before = symbols(legacy_plane.cached_index(&legacy.access).unwrap());
        assert_ne!(before["exported_one"], before["exported_two"]);
        let plane = Arc::new(SemanticPlane::new(storage.path().to_path_buf(), producer));
        let checkout = Checkout::with_owner(root.path(), legacy.owner.clone(), &plane);
        let snapshot = checkout.load();
        let current = snapshot.clone();
        let mut embedded = 0;
        plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts: Vec<String>| {
                    embedded += texts.len();
                    Ok(texts.iter().map(|text| vector("model-a", text)).collect())
                },
                &move || current.clone(),
            )
            .unwrap();
        assert!(embedded <= 1, "symbol rows must not be re-embedded");
        let published = checkout.load();
        checkout.query(&published, "model-a", "module");
        assert_eq!(
            symbols(plane.cached_index(&checkout.access).unwrap()),
            before
        );
    }

    #[test]
    fn summary_repair_keeps_saved_symbols_searchable_while_held() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let caps = EmbedTextCaps {
            total_chars: 70,
            ..EmbedTextCaps::default()
        };
        let (legacy, _, _) = legacy_summary_checkout(storage.path(), root.path(), caps);
        let plane = Arc::new(SemanticPlane::new(
            storage.path().to_path_buf(),
            SemanticProducer::current("model-a", caps),
        ));
        let checkout = Checkout::with_owner(root.path(), legacy.owner.clone(), &plane);
        let snapshot = checkout.load();
        let current = snapshot.clone();
        let mut during = None;
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts: Vec<String>| {
                    during = Some(checkout.query(&snapshot, "model-a", "module"));
                    Ok(texts.iter().map(|text| vector("model-a", text)).collect())
                },
                &move || current.clone(),
            )
            .unwrap();
        assert_eq!(report.embedded_texts, 1);
        let during = during.expect("the summary embedding must hold the repair open");
        assert!(
            during.results.iter().any(|row| row.name == "exported"),
            "saved symbols disappeared during repair: {during:?}"
        );
        assert!(
            during.complete(),
            "a held repair must not withdraw published results"
        );
        assert!(checkout
            .query(&checkout.installed(), "model-a", "module")
            .results
            .iter()
            .any(|row| row.name == "exported"));
    }

    #[test]
    fn composed_worker_never_repairs_inherited_summaries_or_fills_inherited_pending_files() {
        use super::super::manifest_v2::{BaseHeader, EntryPlanes, EntryV2, ManifestV2};
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let caps = EmbedTextCaps {
            total_chars: 70,
            ..EmbedTextCaps::default()
        };
        let (legacy, _, _) = legacy_summary_checkout(storage.path(), root.path(), caps);
        let mut manifest = legacy.installed().generation().flatten().unwrap();
        manifest
            .insert(
                RelPath::new(b"src/pending.rs".to_vec()).unwrap(),
                EntryV2::regular(
                    ContentHash::of(b"pending"),
                    7,
                    EntryPlanes {
                        semantic: Some(PlaneState::pending("base fill pending")),
                        ..Default::default()
                    },
                ),
            )
            .unwrap();
        let header = manifest.header().clone();
        let base = Arc::new(ComposedGeneration::new("base", manifest, None));
        let worker = ManifestV2::new_v3(
            header,
            Some(BaseHeader {
                scope: "base".into(),
                membership_digest: "11".repeat(32),
                identity_digest: "22".repeat(32),
                bound_generation: "base".into(),
            }),
            Default::default(),
        );
        let composed = Arc::new(ComposedGeneration::compose("worker", worker, base, None).unwrap());
        let plane = SemanticPlane::new(
            storage.path().to_path_buf(),
            SemanticProducer::current("model-a", caps),
        );
        let resident = plane.admit(&legacy.access, &composed).unwrap();
        assert_eq!(resident.legacy_summaries.len(), 1);
        let mut live = LiveDelta::new(composed);
        let snapshot = live.snapshot();
        let state = ViewState::default();
        assert!(plane
            .work_items(&state, &resident, &snapshot, true)
            .is_empty());
        for name in ["src/summary.rs", "src/pending.rs"] {
            let path = RelPath::new(name.as_bytes().to_vec()).unwrap();
            let content = snapshot
                .generation()
                .entry(&path)
                .unwrap()
                .content()
                .unwrap();
            live.set_readiness(
                path,
                super::super::snapshot::PlanePatch {
                    content,
                    planes: EntryPlanes {
                        trigram: Some(PlaneState::pending("worker trigram repair")),
                        ..Default::default()
                    },
                },
            );
        }
        assert!(
            plane
                .work_items(&state, &resident, &live.snapshot(), true)
                .is_empty(),
            "a trigram-only live patch claimed inherited semantic work"
        );
    }

    #[test]
    fn small_budget_fill_resumes_after_second_batch_pause_with_aligned_rows() {
        use crate::semantic_index::{retry_lane_pause, with_lane_pause_clock, PauseRetryScope};
        use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), &[("src/many.rs", "pub fn first() {\n    1;\n}\npub fn second() {\n    2;\n}\npub fn third() {\n    3;\n}\n")]);
        let caps = EmbedTextCaps {
            total_chars: 70,
            ..EmbedTextCaps::default()
        };
        let plane = Arc::new(SemanticPlane::new(
            storage.path().to_path_buf(),
            SemanticProducer::current("model-a", caps),
        ));
        let checkout = Checkout::open(storage.path(), "paused-small-budget", root.path(), &plane);
        let snapshot = checkout.load();
        let current = snapshot.clone();
        let mut inputs = Vec::new();
        let result = with_lane_pause_clock(START, |clock| {
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            retry_lane_pause(|| {
                let report = plane
                    .fill(
                        &checkout.owner,
                        &snapshot,
                        FillBudget {
                            max_batch: 1,
                            ..FillBudget::default()
                        },
                        &mut |texts: Vec<String>| {
                            inputs.extend(texts.clone());
                            if inputs.len() == 2 && clock.load(Ordering::SeqCst) == START {
                                Err(SynapseEmbeddingError::LanePaused(LanePause {
                                    lane_id: MODEL.into(),
                                    fingerprint: Some("pinned".into()),
                                    retry_after_ms: Some(PAUSE_MS),
                                    paused_until_ms: START + PAUSE_MS,
                                    reason: "maintenance".into(),
                                    cause: Default::default(),
                                    safe_to_retry_same_request: true,
                                    record_id: 0,
                                })
                                .to_string())
                            } else {
                                Ok(texts.iter().map(|text| vector("model-a", text)).collect())
                            }
                        },
                        &|| current.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                match report.errors.first() {
                    Some(error) => Err(error.clone()),
                    None => Ok(report),
                }
            })
        });
        assert!(result.is_ok(), "paused fill did not resume: {result:?}");
        assert_eq!(inputs.len(), 4, "only the paused request may be retried");
        assert_eq!(inputs[1], inputs[2]);
        let query = checkout.query(&checkout.installed(), "model-a", "module");
        assert_eq!(query.results.len(), 3);
        assert!(query.complete());
    }

    const LARGE_DEFINITIONS: usize = 300;

    fn large_source() -> String {
        (0..LARGE_DEFINITIONS)
            .map(|i| {
                format!("pub fn definition_{i}(input: u32) -> u32 {{\n    input + {i}\n}}\n\n")
            })
            .collect()
    }

    fn edit_snapshot(checkout: &Checkout, text: &str) -> Snapshot {
        let generation = checkout.load();
        write_tree(&checkout.root, &[("src/large.rs", text)]);
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(
            &mut delta,
            &checkout.root,
            &crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            },
        );
        delta.snapshot()
    }

    fn assert_canonical_blob(checkout: &Checkout, source: &str) {
        let producer = checkout.plane.semantic_producer();
        // A full build chunks walker paths in the host's native form. Keep the
        // reference independent of the view's path conversion, including on Windows.
        let relative = Path::new("src").join("large.rs");
        let chunks = crate::semantic_index::chunk_view_file(
            &checkout.root,
            &relative,
            source.as_bytes(),
            producer.caps,
        )
        .unwrap()
        .unwrap();
        let mut full = crate::semantic_index::embed_view_files(
            vec![chunks],
            &mut |texts| Model::default().embed("model-a", texts),
            64,
        )
        .unwrap();
        let expected = full.remove(0).encode_view_payload(&producer.payload());
        let key = producer.key(
            source.as_bytes(),
            &RelPath::new(b"src/large.rs".to_vec()).unwrap(),
        );
        let actual = checkout
            .owner
            .open_store(FamilyPlane::Semantic)
            .unwrap()
            .get(&key)
            .unwrap()
            .unwrap();
        assert_eq!(
            actual, expected,
            "reuse changed the canonical per-file payload"
        );
    }

    #[test]
    fn chunk_reuse_one_definition_edit_embeds_only_changed_chunk() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = large_source();
        write_tree(root.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let initial = checkout.fill(&checkout.load(), &Model::default(), "model-a");
        assert_eq!(
            (initial.model_calls, initial.embedded_texts),
            (5, LARGE_DEFINITIONS)
        );
        let edited = source.replace("input + 149\n", "input * 149\n");
        let snapshot = edit_snapshot(&checkout, &edited);
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        assert_eq!(report.embedded_texts, 1, "one-definition edit: {report:?}");
        assert_eq!(
            (
                report.chunks,
                report.chunk_reused,
                report.model_calls,
                report.installed
            ),
            (300, 299, 1, 1)
        );
        assert_canonical_blob(&checkout, &edited);
        println!(
            "measurement one-function edit: full=5 calls/300 texts reuse={} calls/{} texts",
            report.model_calls, report.embedded_texts
        );
    }

    #[test]
    fn chunk_reuse_line_shift_embeds_zero_texts() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = large_source();
        write_tree(root.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let shifted = format!("\n\n{source}");
        let snapshot = edit_snapshot(&checkout, &shifted);
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        assert_eq!(report.embedded_texts, 0, "line shift: {report:?}");
        assert_eq!(
            (
                report.chunks,
                report.chunk_reused,
                report.model_calls,
                report.installed
            ),
            (300, 300, 0, 1)
        );
        assert_canonical_blob(&checkout, &shifted);
    }

    #[test]
    fn chunk_reuse_cross_checkout_changed_function_and_fresh_tree() {
        let storage = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let source = large_source();
        write_tree(first.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let main = ready_checkout(storage.path(), first.path(), &plane);
        // Drop all arena holders: the new checkout must use persisted indexed
        // rows, not a resident map from the original checkout.
        drop(main);
        drop(plane);
        let plane = new_plane(storage.path(), "model-a");
        write_tree(second.path(), &[("src/large.rs", &source)]);
        let unchanged = Checkout::open(storage.path(), "scope-b", second.path(), &plane);
        let report = unchanged.fill(&unchanged.load(), &Model::default(), "model-a");
        assert_eq!((report.model_calls, report.embedded_texts), (0, 0));
        println!("measurement fresh unchanged checkout: before=0 calls/0 texts after=0 calls/0 texts (whole-file reuse)");
        drop(unchanged);
        drop(plane);
        let plane = new_plane(storage.path(), "model-a");
        let edited = source.replace("input + 149\n", "input * 149\n");
        write_tree(second.path(), &[("src/large.rs", &edited)]);
        let worker = Checkout::open(storage.path(), "scope-c", second.path(), &plane);
        let report = worker.fill(&worker.load(), &Model::default(), "model-a");
        assert_eq!(
            report.embedded_texts, 1,
            "fresh changed checkout: {report:?}"
        );
        assert_eq!(
            (report.chunk_reused, report.model_calls, report.installed),
            (299, 1, 1)
        );
        assert_canonical_blob(&worker, &edited);
    }

    fn large_rel_path() -> RelPath {
        RelPath::new(b"src/large.rs".to_vec()).unwrap()
    }

    /// A ready checkout of `large_source()` whose `src/large.rs` was then
    /// edited in one definition and re-filled. Returns the checkout, the
    /// edited snapshot, and the file's run before and after the edit.
    fn refilled_large_checkout(
        storage: &Path,
        root: &Path,
        plane: &Arc<SemanticPlane>,
    ) -> (
        Checkout,
        Snapshot,
        Arc<crate::semantic_index::SemanticVectors>,
        Arc<crate::semantic_index::SemanticVectors>,
    ) {
        let source = large_source();
        write_tree(root, &[("src/large.rs", &source)]);
        let checkout = ready_checkout(storage, root, plane);
        let producer = plane.semantic_producer();
        let arena = plane.arena("family");
        let old = arena
            .get(&producer.key(source.as_bytes(), &large_rel_path()))
            .expect("the unedited run is resident");
        let edited = source.replace("input + 149\n", "input * 149\n");
        let snapshot = edit_snapshot(&checkout, &edited);
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        assert_eq!(
            (report.chunk_reused, report.embedded_texts, report.installed),
            (299, 1, 1),
            "{report:?}"
        );
        let new = arena
            .get(&producer.key(edited.as_bytes(), &large_rel_path()))
            .expect("the re-filled run is resident");
        (checkout, snapshot, old, new)
    }

    /// A re-fill reuses each unchanged chunk's stored vector. The new run must
    /// point at the allocation the old run already holds rather than at a
    /// fresh copy of the same floats.
    #[test]
    fn refill_of_an_edited_file_shares_reused_chunk_vectors() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let (_checkout, _snapshot, old, new) =
            refilled_large_checkout(storage.path(), root.path(), &plane);
        let shared = new
            .vectors()
            .filter(|vector| old.vectors().any(|old| Arc::ptr_eq(old, vector)))
            .count();
        assert_eq!(
            shared, 299,
            "reused chunks hold copies instead of the old run's vectors"
        );
        let memory = plane.arena("family").memory();
        assert_eq!(
            (memory.runs, memory.chunks, memory.vectors),
            (2, 600, 301),
            "{memory:?}"
        );
    }

    /// The overlay a query scores holds the arena's replacement run itself:
    /// no entry, string or vector of the edited file is copied for the view.
    #[test]
    fn overlay_holds_replacement_runs_without_copying_them() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let (checkout, snapshot, old, new) =
            refilled_large_checkout(storage.path(), root.path(), &plane);
        let index = plane
            .overlay(&checkout.access, root.path(), &snapshot)
            .unwrap()
            .index;
        assert!(
            index
                .view_runs_for_test()
                .iter()
                .any(|run| Arc::ptr_eq(run, &new)),
            "the overlay does not reference the arena's replacement run"
        );
        let live = index.live_vectors_for_test();
        assert_eq!(live.len(), 300);
        assert!(
            live.iter().all(|vector| old
                .vectors()
                .chain(new.vectors())
                .any(|resident| Arc::ptr_eq(resident, vector))),
            "the overlay scores a vector the arena does not hold"
        );
        let memory = index.estimated_memory();
        assert_eq!(
            (
                memory.counts.get("vector_bytes").copied(),
                memory.counts.get("text_bytes").copied()
            ),
            (Some(0), Some(0)),
            "the overlay owns copied entries: {memory:?}"
        );
    }

    /// Sharing changes where entries live, never what a query returns: the
    /// overlay ranks exactly like a cold build, and a clone (which owns its
    /// entries) ranks exactly like the overlay it was cloned from.
    #[test]
    fn overlay_results_after_refill_match_a_cold_build_and_a_private_copy() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let (checkout, snapshot, _old, _new) =
            refilled_large_checkout(storage.path(), root.path(), &plane);
        for text in ["definition 149", "input total", "definition_7"] {
            let query = checkout.query(&snapshot, "model-a", text);
            assert!(query.complete(), "{text}");
            assert_eq!(query.results.len(), 100, "{text}");
            assert_eq!(
                rows(root.path(), &query.results),
                cold(root.path(), "model-a", text),
                "{text}"
            );
            let index = plane
                .overlay(&checkout.access, root.path(), &snapshot)
                .unwrap()
                .index;
            let private = (*index).clone();
            assert!(private.view_runs_for_test().is_empty());
            let query_vector = vector("model-a", text);
            assert_eq!(
                rows(root.path(), &private.search(&query_vector, 100)),
                rows(root.path(), &index.search(&query_vector, 100)),
                "{text}"
            );
        }
    }

    /// Resident vector bytes for one checkout with an overlay over a re-filled
    /// large file, at the production vector width (1024 floats, 4 KiB). Every
    /// distinct allocation reachable from the arena's runs or the overlay is
    /// counted once.
    #[test]
    fn measurement_resident_vector_bytes_for_overlay_over_refilled_large_file() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = large_source();
        write_tree(root.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let mut wide = |texts: Vec<String>| -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|text| {
                    let mut bytes = [0u8; 1024];
                    blake3::Hasher::new()
                        .update(text.as_bytes())
                        .finalize_xof()
                        .fill(&mut bytes);
                    bytes
                        .iter()
                        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
                        .collect()
                })
                .collect())
        };
        let snapshot = checkout.load();
        plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut wide,
                &|| snapshot.clone(),
            )
            .unwrap();
        checkout.load();
        let edited = source.replace("input + 149\n", "input * 149\n");
        let snapshot = edit_snapshot(&checkout, &edited);
        plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut wide,
                &|| snapshot.clone(),
            )
            .unwrap();
        let producer = plane.semantic_producer();
        let arena = plane.arena("family");
        let old = arena
            .get(&producer.key(source.as_bytes(), &large_rel_path()))
            .unwrap();
        let new = arena
            .get(&producer.key(edited.as_bytes(), &large_rel_path()))
            .unwrap();
        let index = plane
            .overlay(&checkout.access, root.path(), &snapshot)
            .unwrap()
            .index;
        let mut seen = HashSet::new();
        let mut bytes = 0u64;
        for vector in old
            .vectors()
            .chain(new.vectors())
            .cloned()
            .chain(index.live_vectors_for_test())
        {
            if seen.insert(Arc::as_ptr(&vector).cast::<f32>() as usize) {
                bytes += crate::semantic_index::vector_bytes(&vector);
            }
        }
        println!(
            "measurement resident vectors: chunks_per_run=300 distinct_vectors={} vector_bytes={bytes} arena={:?}",
            seen.len(),
            arena.memory()
        );
        assert_eq!((seen.len(), bytes), (301, 301 * 4096));
    }

    #[test]
    fn chunk_vector_stored_payload_is_compact_and_preserves_bits() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let store = SemanticStore::V2(checkout.owner.open_store(FamilyPlane::Semantic).unwrap());
        let producer = plane.semantic_producer();
        let identity = producer.chunk_identity();
        let text = "file:src/a.rs\nfn compact_vector()";
        let vector = (0..1024)
            .map(|i| if i == 0 { -0.0 } else { i as f32 / 1024.0 })
            .collect::<Vec<_>>();
        plane
            .embed_chunks(
                &plane.arena("family"),
                &store,
                vec![text.into()],
                &mut |_| Ok(vec![vector.clone()]),
                &mut FillReport::default(),
                &mut || Ok(()),
                &mut Vec::new(),
            )
            .unwrap();
        let key = producer.chunk_key(text);
        let payload = store.get(&key).unwrap().unwrap();
        // A format byte and two u32 lengths are the only fixed overhead.
        let bound = 4 * vector.len() + 9 + identity.len() + text.len();
        assert!(
            payload.len() <= bound,
            "stored chunk payload uses {} bytes; compact bound is {bound}",
            payload.len()
        );
        let reused = load_chunk(&store, &plane.arena("family"), &key, &identity, text)
            .unwrap()
            .unwrap();
        assert_eq!(
            reused
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            vector
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
        println!(
            "1024-dimensional stored chunk payload: {} bytes (bound {bound})",
            payload.len()
        );
    }

    #[test]
    fn chunk_vector_decode_rejects_malformed_old_or_mismatched_payloads() {
        let identity = "producer";
        let text = "text";
        let valid = encode_chunk_vector(identity, text, &[1.0, -0.0]).unwrap();
        assert!(decode_chunk_vector(&valid, identity, text).is_some());
        assert!(decode_chunk_vector(&valid, "other producer", text).is_none());
        assert!(decode_chunk_vector(&valid, identity, "other text").is_none());
        let header_len = 9 + identity.len() + text.len();
        let mut malformed = vec![
            Vec::new(),
            br#"{"producer":"producer","text":"text","bits":[1065353216,2147483648]}"#.to_vec(),
            valid[..header_len].to_vec(),
            valid[..valid.len() - 1].to_vec(),
        ];
        // Every incomplete length or string field must be rejected safely.
        malformed.extend((0..header_len).map(|end| valid[..end].to_vec()));
        let mut unknown_format = valid.clone();
        unknown_format[0] = CHUNK_VECTOR_FORMAT + 1;
        malformed.push(unknown_format);
        let mut oversized_identity = valid.clone();
        oversized_identity[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        malformed.push(oversized_identity);
        let mut oversized_text = valid.clone();
        let text_length_offset = 5 + identity.len();
        oversized_text[text_length_offset..text_length_offset + 4]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        malformed.push(oversized_text);
        let mut partial_float = valid.clone();
        partial_float.push(0);
        malformed.push(partial_float);
        for payload in malformed {
            assert!(decode_chunk_vector(&payload, identity, text).is_none());
        }
    }

    #[test]
    fn chunk_reuse_hash_collision_requires_exact_text() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let store = SemanticStore::V2(checkout.owner.open_store(FamilyPlane::Semantic).unwrap());
        let producer = plane.semantic_producer();
        let text = "file:src/a.rs\nfn correct()";
        let key = producer.chunk_key(text);
        store
            .put_or_touch(
                &key,
                &encode_chunk_vector(
                    &producer.chunk_identity(),
                    "different text at colliding hash",
                    &[999f32; 16],
                )
                .unwrap(),
            )
            .unwrap();
        let mut report = FillReport::default();
        let result = plane
            .embed_chunks(
                &plane.arena("family"),
                &store,
                vec![text.into()],
                &mut |texts| Model::default().embed("model-a", texts),
                &mut report,
                &mut || Ok(()),
                &mut Vec::new(),
            )
            .unwrap();
        match &result[0] {
            crate::semantic_index::BuildEmbeddingRow::Embedded { vector: actual, .. } => {
                assert_eq!(
                    *actual,
                    vector("model-a", text),
                    "colliding text installed the wrong vector"
                )
            }
            _ => panic!("expected vector"),
        }
        assert_eq!((report.embedded_texts, report.chunk_reused), (1, 0));
    }

    #[test]
    fn chunk_reuse_producer_identity_requires_model_template_and_caps() {
        let producer = SemanticProducer::current("model-a", EmbedTextCaps::default());
        let text = "same exact text";
        let key = producer.chunk_key(text);
        for component in ["model", "template", "caps", "chunker"] {
            let mut other = producer.clone();
            match component {
                "model" => other.model_fingerprint.push_str("-new"),
                "template" => other.template_version.push_str("-new"),
                "caps" => other.caps.total_chars += 1,
                "chunker" => other.chunker_version.push_str("-new"),
                _ => unreachable!(),
            }
            assert_ne!(
                key,
                other.chunk_key(text),
                "{component} is missing from chunk identity"
            );
        }
    }

    #[test]
    fn chunk_cache_store_refusal_is_not_an_embedding_retry() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), &[("src/large.rs", &large_source())]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection.execute_batch("CREATE TRIGGER refuse_chunk_cache BEFORE INSERT ON blob_payloads BEGIN SELECT RAISE(ABORT, 'chunk cache storage refused'); END;").unwrap();
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        assert_eq!(report.installed, 0);
        assert_eq!(
            report.store_errors.len(),
            1,
            "permanent cache refusal was classified as an embedding retry: {report:?}"
        );
        assert!(report.store_errors[0].contains("chunk cache storage refused"));
    }

    #[test]
    fn chunk_reuse_concurrent_different_file_keys_embed_each_text_once() {
        let storage = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let source = large_source();
        write_tree(first.path(), &[("src/large.rs", &source)]);
        write_tree(
            second.path(),
            &[(
                "src/large.rs",
                &source.replace("input + 149\n", "input * 149\n"),
            )],
        );
        let plane = new_plane(storage.path(), "model-a");
        let a = Checkout::open(storage.path(), "scope-a", first.path(), &plane);
        let b = Checkout::open(storage.path(), "scope-b", second.path(), &plane);
        let sa = a.load();
        let sb = b.load();
        let model = Model::default();
        let start = std::sync::Barrier::new(2);
        let (ra, rb) = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                start.wait();
                a.fill(&sa, &model, "model-a")
            });
            let right = scope.spawn(|| {
                start.wait();
                b.fill(&sb, &model, "model-a")
            });
            (left.join().unwrap(), right.join().unwrap())
        });
        assert_eq!(
            model.texts(),
            301,
            "overlapping versions duplicated chunk work: {ra:?} {rb:?}"
        );
        assert_eq!(ra.chunk_reused + rb.chunk_reused, 299);
        assert_eq!((ra.installed, rb.installed), (1, 1));
        assert_canonical_blob(&a, &source);
        assert_canonical_blob(&b, &source.replace("input + 149\n", "input * 149\n"));
    }

    type Row = (String, String, u32, u32);

    fn rows(root: &Path, results: &[SemanticResult]) -> Vec<Row> {
        results
            .iter()
            .map(|result| {
                (
                    result
                        .file
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    result.name.clone(),
                    result.start_line,
                    result.score.to_bits(),
                )
            })
            .collect()
    }

    /// An independent cold rebuild: a full `SemanticIndex` build of the
    /// checkout's current walker membership, with no view store involved.
    fn cold(root: &Path, model: &str, text: &str) -> Vec<Row> {
        let files = ConfiguredMembershipWalker.files(root).unwrap();
        let index = SemanticIndex::build(
            root,
            &files,
            &mut |texts: Vec<String>| Ok(texts.iter().map(|text| vector(model, text)).collect()),
            64,
        )
        .unwrap();
        rows(root, &index.search(&vector(model, text), 100))
    }

    fn ready_checkout(storage: &Path, root: &Path, plane: &Arc<SemanticPlane>) -> Checkout {
        let checkout = Checkout::open(storage, "scope-a", root, plane);
        let snapshot = checkout.load();
        checkout.fill(&snapshot, &Model::default(), "model-a");
        checkout.load();
        checkout
    }

    /// A root `.aftignore` that ignores itself is never a walked member, yet
    /// its rules decided membership. While its bytes match what the walk
    /// read, queries are vouched and complete; an edit to it is still noticed
    /// by the first query, before any reconcile.
    #[test]
    fn self_ignoring_root_ignore_file_vouches_until_it_changes() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(
            root.path(),
            &[
                (
                    "src/kept.rs",
                    "pub fn kept_item(value: u32) -> u32 {\n    value\n}\n",
                ),
                (
                    "src/later.rs",
                    "pub fn later_item(value: u32) -> u32 {\n    value\n}\n",
                ),
                ("src/hidden.rs", "pub fn hidden_item() {}\n"),
                (".aftignore", "src/hidden.rs\n/.aftignore\n"),
            ],
        );
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let snapshot = checkout.installed();
        let ignore_file = RelPath::new(b".aftignore".to_vec()).unwrap();
        assert!(!snapshot.membership().contains_key(&ignore_file));

        let query = checkout.query(&snapshot, "model-a", "kept item value");
        assert!(
            !query.unvouched,
            "self-ignoring ignore file counted as changed"
        );
        assert!(query.complete(), "{query:?}");

        // Still self-ignoring, now also excluding a file the snapshot scores.
        write_tree(
            root.path(),
            &[(".aftignore", "src/hidden.rs\nsrc/later.rs\n/.aftignore\n")],
        );
        let early = checkout.query(&snapshot, "model-a", "later item value");
        assert!(early.unvouched, "ignore edit went unnoticed");
        assert!(
            rows(root.path(), &early.results)
                .iter()
                .all(|row| !row.0.ends_with("later.rs")),
            "scored a file the edited rules exclude"
        );

        // Once membership is walked again under the edited rules, vouched.
        checkout.load();
        let settled = checkout.query(&checkout.installed(), "model-a", "kept item value");
        assert!(!settled.unvouched, "{settled:?}");
    }

    #[test]
    fn semantic_superseded_base_vectors_are_never_scored() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        let before = checkout.query(&generation, "model-a", "alpha total");
        assert!(before.complete());
        assert!(rows(root.path(), &before.results)
            .iter()
            .any(|row| row.1 == "alpha_total"));
        assert_eq!(
            rows(root.path(), &before.results),
            cold(root.path(), "model-a", "alpha total")
        );

        // Edit alpha.rs; the edit is in the live delta, not in a generation.
        write_tree(
            root.path(),
            &[(
                "src/alpha.rs",
                "pub fn omega_rewritten() -> bool {\n    true\n}\n",
            )],
        );
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(
            &mut delta,
            root.path(),
            &crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            },
        );
        let edited = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(
            rows(root.path(), &edited.results)
                .iter()
                .all(|row| row.0 != "src/alpha.rs"),
            "vectors of alpha.rs's previous content were scored: {:?}",
            rows(root.path(), &edited.results)
        );
        assert_eq!(edited.pending, vec![root.path().join("src/alpha.rs")]);

        let model = Model::default();
        let snapshot = delta.snapshot();
        let report = checkout
            .plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| model.embed("model-a", texts),
                &|| delta.snapshot(),
            )
            .unwrap();
        assert_eq!(report.embedded_keys, 1, "only the edited file is embedded");
        let filled = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(filled.complete());
        assert_eq!(
            rows(root.path(), &filled.results),
            cold(root.path(), "model-a", "alpha total")
        );
    }

    #[test]
    fn semantic_pending_work_survives_fold_and_reopen() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        checkout.load();
        // Fold again with nothing filled: no live entry remains for any path,
        // so the pending state has to come from the published generation.
        let folded = checkout.load();
        assert!(folded.live_entries().next().is_none());
        assert_eq!(
            plane.readiness(&checkout.access, &folded),
            PlaneReadiness::Ready {
                pending: 3,
                failed: 0
            }
        );
        assert_eq!(plane.pending_work(&checkout.access, &folded).len(), 3);
        let query = checkout.query(&folded, "model-a", "beta cache");
        assert!(query.results.is_empty());
        assert_eq!(query.pending.len(), 3);

        // A new plane (a new session) over the same storage still sees it.
        drop(checkout);
        let reopened_plane = new_plane(storage.path(), "model-a");
        let reopened = Checkout::open(storage.path(), "scope-a", root.path(), &reopened_plane);
        let snapshot = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&reopened.access, &snapshot),
            PlaneReadiness::Ready {
                pending: 3,
                failed: 0
            }
        );
        let model = Model::default();
        let report = reopened.fill(&snapshot, &model, "model-a");
        assert_eq!(report.installed, 3);
        let folded = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&reopened.access, &folded),
            PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
        assert_eq!(
            rows(
                root.path(),
                &reopened.query(&folded, "model-a", "beta cache").results
            ),
            cold(root.path(), "model-a", "beta cache")
        );
    }

    #[test]
    fn two_views_one_key_one_model_call() {
        let storage = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_tree(first.path(), FILES);
        write_tree(second.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let a = Checkout::open(storage.path(), "scope-a", first.path(), &plane);
        let b = Checkout::open(storage.path(), "scope-b", second.path(), &plane);
        let snapshot_a = a.load();
        let snapshot_b = b.load();
        let model = Model::default();
        let first_fill = a.fill(&snapshot_a, &model, "model-a");
        let chunks = model.texts();
        assert!(chunks > 0);
        assert_eq!(first_fill.embedded_texts, chunks);
        let second_fill = b.fill(&snapshot_b, &model, "model-a");
        assert_eq!(
            model.texts(),
            chunks,
            "the second view embedded content the first already embedded: {second_fill:?}"
        );
        assert_eq!(second_fill.resident_hits, 3);
        assert_eq!(second_fill.installed, 3);
        // The shared runs are resident once, and each view's private overlay
        // holds nothing for unedited files.
        let arena = plane.arena("family");
        let once = arena.memory();
        assert_eq!(once.runs, 3);
        let query_a = a.query(&a.load(), "model-a", "parse input");
        let query_b = b.query(&b.load(), "model-a", "parse input");
        assert_eq!(arena.memory(), once);
        assert_eq!(
            rows(first.path(), &query_a.results),
            rows(second.path(), &query_b.results)
        );
        assert_eq!(plane.private_memory(&a.access), 0);
        assert_eq!(plane.private_memory(&b.access), 0);
    }

    fn trigram_policy() -> crate::blob_store::v2::TrigramPolicy {
        crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        }
    }

    #[test]
    fn late_completion_for_moved_content_is_dropped() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        // By the time the work completes, alpha.rs holds other bytes.
        let mut moved = LiveDelta::new(Arc::clone(snapshot.generation()));
        moved.apply(
            RelPath::new(b"src/alpha.rs".to_vec()).unwrap(),
            LiveEntry::new(DiskState::of_bytes(b"pub fn later() {}\n"), 1),
        );
        let model = Model::default();
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| model.embed("model-a", texts),
                &|| moved.snapshot(),
            )
            .unwrap();
        assert_eq!((report.installed, report.dropped), (2, 1));
        let query = checkout.query(&moved.snapshot(), "model-a", "alpha");
        assert_eq!(query.pending, vec![root.path().join("src/alpha.rs")]);
        assert!(rows(root.path(), &query.results)
            .iter()
            .all(|row| row.0 != "src/alpha.rs"));
    }

    #[test]
    fn model_change_never_resurrects_old_vectors() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane_a = new_plane(storage.path(), "model-a");
        let checkout_a = ready_checkout(storage.path(), root.path(), &plane_a);
        let old = checkout_a.installed();
        drop(checkout_a);

        // A plane for another model reads the old generation as incompatible:
        // none of its vectors is scored, every file is pending.
        let plane_b = new_plane(storage.path(), "model-b");
        let checkout_b = Checkout::open(storage.path(), "scope-a", root.path(), &plane_b);
        plane_b
            .open_generation(&checkout_b.access, old.generation())
            .unwrap();
        let stale = checkout_b.query(&old, "model-b", "alpha total");
        assert!(stale.results.is_empty(), "{:?}", stale.results);
        assert_eq!(stale.pending.len(), 3);

        let snapshot = checkout_b.load();
        let model = Model::default();
        let report = checkout_b.fill(&snapshot, &model, "model-b");
        assert_eq!(report.embedded_keys, 3);
        let folded = checkout_b.load();
        assert_eq!(
            rows(
                root.path(),
                &checkout_b.query(&folded, "model-b", "alpha total").results
            ),
            cold(root.path(), "model-b", "alpha total")
        );
    }

    #[test]
    fn deterministic_failure_is_recorded_and_survives_fold_and_reopen() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        std::fs::write(root.path().join("src/broken.rs"), [0xff, 0xfe, b'f', b'n']).unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let model = Model::default();
        let report = checkout.fill(&snapshot, &model, "model-a");
        assert_eq!((report.installed, report.failed), (3, 1));
        let folded = checkout.load();
        assert!(matches!(
            folded
                .generation()
                .entry(&RelPath::new(b"src/broken.rs".to_vec()).unwrap())
                .and_then(|entry| entry.plane_state(FamilyPlane::Semantic)),
            Some(PlaneState::Failed { .. })
        ));
        drop(checkout);
        let reopened_plane = new_plane(storage.path(), "model-a");
        let reopened = Checkout::open(storage.path(), "scope-a", root.path(), &reopened_plane);
        let snapshot = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&reopened.access, &snapshot),
            PlaneReadiness::Ready {
                pending: 0,
                failed: 1
            }
        );
        let texts = model.texts();
        let again = reopened.fill(&snapshot, &model, "model-a");
        assert_eq!((again.queued, model.texts()), (0, texts));
        let query = reopened.query(&snapshot, "model-a", "beta");
        assert_eq!(query.failed, vec![root.path().join("src/broken.rs")]);
        assert_eq!(
            rows(root.path(), &query.results),
            cold(root.path(), "model-a", "beta")
        );
    }

    #[test]
    fn retired_semantic_readiness_uses_its_pinned_fills_after_unbind_and_rebind() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let installed = checkout.installed();
        write_tree(root.path(), &[("src/alpha.rs", "fn alpha_edited() {}\n")]);
        let mut delta = LiveDelta::new(installed.generation().clone());
        crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
        let snapshot = delta
            .snapshot()
            .with_readers(installed.readers().unwrap().clone());
        let model = Model::default();
        assert_eq!(checkout.fill(&snapshot, &model, "model-a").installed, 1);
        let access = checkout.access.clone();
        let ready = PlaneReadiness::Ready {
            pending: 0,
            failed: 0,
        };
        assert_eq!(plane.readiness(&access, &snapshot), ready);
        plane.unbind(&access);
        assert_eq!(plane.readiness(&access, &snapshot), ready);
        assert!(plane.pending_work(&access, &snapshot).is_empty());
        assert!(!lock(&plane.views)
            .contains_key(&(access.family().to_owned(), access.scope().to_owned())));

        let adapters: Vec<Arc<dyn PlaneAdapter>> = vec![plane.clone()];
        let readers = super::super::snapshot::PlaneReaders::open(
            &access,
            snapshot.generation().clone(),
            &adapters,
        )
        .unwrap();
        let rebound = snapshot.clone().with_readers(readers);
        assert_eq!(
            plane.readiness(&access, &rebound),
            PlaneReadiness::Ready {
                pending: 1,
                failed: 0
            }
        );
        assert_eq!(plane.readiness(&access, &snapshot), ready);
        assert!(plane.pending_work(&access, &snapshot).is_empty());
    }

    #[test]
    fn an_old_semantic_bundle_survives_unbind_without_reopening_or_resurrecting_scope_state() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let bundle = checkout.driver.installed_bundle();
        let snapshot = checkout.installed();
        let access = checkout.access.clone();
        let expected = rows(
            root.path(),
            &checkout.query(&snapshot, "model-a", "alpha").results,
        );
        assert!(!expected.is_empty());
        let arena = plane.arena("family");
        let work = (arena.decode_count(), arena.store_reads());
        plane.unbind(&access);
        drop(checkout);
        let answer = plane
            .search(
                &access,
                root.path(),
                &snapshot,
                &vector("model-a", "alpha"),
                100,
                &|_| true,
            )
            .unwrap();
        assert!(answer.complete(), "{answer:?}");
        assert_eq!(rows(root.path(), &answer.results), expected);
        assert_eq!((arena.decode_count(), arena.store_reads()), work);
        assert!(
            !lock(&plane.views)
                .contains_key(&(access.family().to_owned(), access.scope().to_owned())),
            "a retired query must not recreate mutable scope state"
        );
        drop(bundle);
        assert!(arena.memory().runs > 0);
        drop(snapshot);
        assert_eq!(arena.memory().runs, 0);
    }

    #[test]
    fn queries_never_decode_store_rows_and_unbind_releases_the_arena() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        let arena = plane.arena("family");
        let (decodes, reads) = (arena.decode_count(), arena.store_reads());
        for text in ["alpha", "beta", "gamma", "cache insert"] {
            checkout.query(&generation, "model-a", text);
        }
        assert_eq!(
            (arena.decode_count(), arena.store_reads()),
            (decodes, reads)
        );
        assert_eq!(plane.private_memory(&checkout.access), 0);

        // Private memory grows with the edited files (their tombstones and run
        // handles) but holds none of their vectors.
        let edit = |files: &[(&str, &str)]| {
            write_tree(root.path(), files);
            let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
            crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
            let snapshot = delta.snapshot();
            let model = Model::default();
            plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed("model-a", texts),
                    &|| snapshot.clone(),
                )
                .unwrap();
            checkout.query(&snapshot, "model-a", "edited");
            plane.private_memory(&checkout.access)
        };
        let one = edit(&[("src/gamma.rs", "fn gamma_edited() {}\n")]);
        let two = edit(&[("src/alpha.rs", "fn alpha_edited() {}\n")]);
        assert!(
            one > 0 && two > one,
            "one edit {one} bytes, two edits {two} bytes"
        );
        assert!(
            two < arena.memory().bytes,
            "private overlay {two} should stay below the shared arena {}",
            arena.memory().bytes
        );

        plane.unbind(&checkout.access);
        drop(checkout);
        assert!(
            arena.memory().runs > 0,
            "the old query still owns its resident reader"
        );
        drop(generation);
        assert_eq!(
            arena.memory().runs,
            0,
            "unbind left runs resident after the last query drained"
        );
    }

    /// A fold may install a generation built from a cut taken before a fill
    /// landed. Installed completions that generation does not record stay in
    /// the fill map: the work is neither lost nor done again.
    #[test]
    fn semantic_installed_fills_survive_a_fold_that_does_not_carry_them() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        write_tree(
            root.path(),
            &[(
                "src/alpha.rs",
                "pub fn alpha_rewritten() -> u8 {\n    7\n}\n",
            )],
        );
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
        let model = Model::default();
        let snapshot = delta.snapshot();
        let fill = |model: &Model| {
            plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed("model-a", texts),
                    &|| snapshot.clone(),
                )
                .unwrap()
        };
        assert_eq!(fill(&model).installed, 1);
        // Install a generation that still names alpha.rs's previous content.
        plane
            .open_generation(&checkout.access, generation.generation())
            .unwrap();
        let answer = checkout.query(&snapshot, "model-a", "alpha rewritten");
        assert!(answer.complete(), "installed work was lost: {answer:?}");
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "model-a", "alpha rewritten")
        );
        let texts = model.texts();
        assert_eq!(fill(&model).queued, 0);
        assert_eq!(model.texts(), texts);
    }

    #[test]
    fn overlay_converts_view_paths_once_to_the_native_form() {
        assert_eq!(
            overlay_rel_path(&RelPath::new(b"src/module_2.rs".to_vec()).unwrap(), '\\'),
            Some(PathBuf::from("src\\module_2.rs"))
        );
    }

    /// Windows' path form, simulated on every platform. A full build names
    /// files `src\alpha.rs` there, in chunk paths, results and embedded text;
    /// a view must use the same form everywhere in its overlay, or superseded
    /// vectors escape their tombstones and scores differ from a full build.
    #[test]
    fn backslash_overlay_paths_hide_superseded_vectors_and_match_native_results() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = Arc::new(
            SemanticPlane::new(
                storage.path().to_path_buf(),
                SemanticProducer::current("model-a", EmbedTextCaps::default()),
            )
            .with_path_separator('\\'),
        );
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let texts = Mutex::new(Vec::new());
        let current = snapshot.clone();
        plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |batch| {
                    texts.lock().unwrap().extend(batch.iter().cloned());
                    Ok(batch.iter().map(|text| vector("model-a", text)).collect())
                },
                &move || current.clone(),
            )
            .unwrap();
        let texts = texts.into_inner().unwrap();
        assert!(texts.iter().any(|text| text.contains("file:src\\alpha.rs")));
        assert!(
            texts.iter().all(|text| !text.contains("file:src/")),
            "embedded text used the view key form: {texts:?}"
        );
        let generation = checkout.load();
        let answer = checkout.query(&generation, "model-a", "alpha total");
        let files = rows(root.path(), &answer.results)
            .into_iter()
            .map(|row| row.0)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            files,
            BTreeSet::from([
                "src\\alpha.rs".to_owned(),
                "src\\beta.rs".to_owned(),
                "src\\gamma.rs".to_owned()
            ])
        );

        write_tree(root.path(), &[("src/alpha.rs", "pub fn omega() {}\n")]);
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
        let edited = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(
            rows(root.path(), &edited.results)
                .iter()
                .all(|row| row.0 != "src\\alpha.rs"),
            "superseded vectors of src\\alpha.rs were scored"
        );
        assert_eq!(edited.pending, vec![root.path().join("src\\alpha.rs")]);
    }

    /// Two views can hold the same generation (a seed is shared by name while
    /// a sibling loads). Readiness is asked for one view, so each reports its
    /// own fill state instead of whichever view happened to admit it first.
    #[test]
    fn readiness_is_per_view_when_views_share_a_generation() {
        let storage = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_tree(first.path(), FILES);
        write_tree(second.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let a = Checkout::open(storage.path(), "scope-a", first.path(), &plane);
        let b = Checkout::open(storage.path(), "scope-b", second.path(), &plane);
        let shared = a.load();
        plane
            .open_generation(&b.access, shared.generation())
            .unwrap();
        let unfilled = PlaneReadiness::Ready {
            pending: 3,
            failed: 0,
        };
        assert_eq!(plane.readiness(&a.access, &shared), unfilled);
        assert_eq!(plane.readiness(&b.access, &shared), unfilled);

        // Only the first view fills; the second still owes all its work.
        assert_eq!(a.fill(&shared, &Model::default(), "model-a").installed, 3);
        assert_eq!(
            plane.readiness(&a.access, &shared),
            PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
        assert_eq!(plane.readiness(&b.access, &shared), unfilled);
    }
}

#[cfg(test)]
mod v3_store_tests {
    use super::*;

    #[test]
    fn a_v3_semantic_store_puts_touches_and_admits_runs_without_the_v2_store() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let storage = tempfile::tempdir().unwrap();
        let store = SemanticStore::V3(
            super::super::snapshot::V3Route {
                storage: storage.path().to_path_buf(),
                family: "00112233445566ff".into(),
                scope: "worker".into(),
            },
            std::sync::OnceLock::new(),
        );
        let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
        assert_eq!(store.get(&key).unwrap(), None);
        assert_eq!(store.touch(&[key]).unwrap().missing, vec![key]);
        assert!(matches!(
            store.put_or_touch(&key, b"run").unwrap(),
            PutOrTouch::Inserted { .. }
        ));
        assert!(matches!(
            store.put_or_touch(&key, b"run").unwrap(),
            PutOrTouch::Reused { .. }
        ));
        assert!(matches!(
            store.put_or_touch(&key, b"another run"),
            Err(crate::blob_store::v2::StoreError::ConflictingPayload(conflict)) if conflict == key
        ));
        assert_eq!(store.get(&key).unwrap(), Some(b"run".to_vec()));
        assert!(store.touch(&[key]).unwrap().missing.is_empty());
        // Verified-run markers are a v2 optimization; v3 writes none.
        assert!(store.put_at_deletion_epoch(|_| None).unwrap().is_none());
        assert!(store.deletion_epoch().unwrap().is_none());
        assert_eq!(
            store.reader().run_payload(&key).unwrap(),
            Some(b"run".to_vec())
        );
        assert!(!storage.path().join("blobs/v2").exists());
    }

    #[test]
    fn v3_semantic_file_and_chunk_lookups_share_one_validated_reader_per_pass() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let storage = tempfile::tempdir().unwrap();
        let store = SemanticStore::V3(
            super::super::snapshot::V3Route {
                storage: storage.path().to_path_buf(),
                family: "00112233445566ff".into(),
                scope: "worker".into(),
            },
            std::sync::OnceLock::new(),
        );
        let rows: Vec<_> = (0..16)
            .map(|i| (FamilyKey::new(FamilyPlane::Semantic, [i; 32]), vec![i; 8]))
            .collect();
        assert_eq!(store.get(&rows[0].0).unwrap(), None);
        assert!(store
            .put_batch(&rows)
            .unwrap()
            .iter()
            .all(|outcome| matches!(outcome, Ok(PutOrTouch::Inserted { .. }))));
        let before = crate::blob_store::v3::work_profile::count("validate_database");
        for (key, payload) in &rows {
            assert_eq!(store.get(key).unwrap().as_ref(), Some(payload));
            assert_eq!(
                store.reader().run_payload(key).unwrap().as_ref(),
                Some(payload)
            );
        }
        assert_eq!(
            crate::blob_store::v3::work_profile::count("validate_database") - before,
            1
        );
    }
}
