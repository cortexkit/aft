use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::commands::semantic_search::comparator::{CandidateResult, SymbolOffsetRange};
use crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use crate::commands::semantic_search::generation_token::GenerationToken;
use crate::native_stat::{now_ns, NativeStat};
use crate::search_index::ExactEvidenceScope;

const MAX_EXACT_MEMO_ENTRIES: usize = 128;
const MAX_EXACT_MEMO_BYTES: usize = 8 * 1024 * 1024;
const MAX_RETIRED_POISONED_KEYS: usize = 128;
const MAX_LITERAL_PRESENCE_ENTRIES: usize = 128;
const MAX_LITERAL_PRESENCE_BYTES: usize = 1024 * 1024;
const MAX_RETIRED_POISONED_BYTES: usize = 1024 * 1024;
const MAX_DIGEST_PROOF_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct DigestProof {
    stat: NativeStat,
    observed_ns: u128,
    digest: String,
}

#[derive(Default)]
struct DigestProofCache {
    files: HashMap<PathBuf, DigestProof>,
    payload_bytes: usize,
}

impl DigestProofCache {
    fn estimated_bytes(&self) -> usize {
        self.files.iter().fold(
            map_storage_bytes::<PathBuf, DigestProof>(self.files.capacity()),
            |bytes, (path, proof)| {
                bytes
                    .saturating_add(path.capacity())
                    .saturating_add(proof.digest.capacity())
            },
        )
    }

    fn insert(&mut self, path: &Path, proof: DigestProof) {
        // Include a conservative hash-table allowance as well as owned bytes.
        // Oversized proofs bypass retention, never content verification.
        let weight = path.as_os_str().len()
            + proof.digest.capacity()
            + 4 * std::mem::size_of::<(PathBuf, DigestProof)>();
        if weight > MAX_DIGEST_PROOF_BYTES {
            return;
        }
        if self.payload_bytes + weight > MAX_DIGEST_PROOF_BYTES {
            *self = Self::default();
        }
        // Replacement can overestimate retained bytes, which only evicts sooner.
        self.payload_bytes += weight;
        self.files.insert(path.to_path_buf(), proof);
    }
}

fn regular_file_stat(path: &Path) -> Option<NativeStat> {
    let stat = NativeStat::read(path).ok()?;
    // NativeStat observes the final component without following symlinks. Its
    // proof cannot certify the bytes of a symlink's independently changing target.
    #[cfg(unix)]
    if stat.mode & u32::from(libc::S_IFMT) != u32::from(libc::S_IFREG) {
        return None;
    }
    #[cfg(windows)]
    if stat.mode
        & (windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY)
        != 0
    {
        return None;
    }
    Some(stat)
}

fn read_digest(
    path: &Path,
    request_files: Option<&super::request_files::RequestFiles>,
) -> Result<String, PathBuf> {
    #[cfg(test)]
    crate::search_hot_path_measurements::record_memo_file_read(path);
    super::waits::timed(super::waits::SearchWait::FileRead, || {
        super::request_files::read_through(request_files, path, |path| Ok((fs::read(path)?, None)))
    })
    .map(|fetched| compute_content_digest(fetched.file.bytes()))
    .map_err(|_| path.to_path_buf())
}

/// Whether some file holds a code literal's whole text, as one search settled
/// it. `Undetermined` means the check could not finish (its read bound ran
/// out, or a file could not be read); it never means absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralPresence {
    Present,
    Absent,
    Undetermined,
}

/// Size and modification time of a file when it was read, used to tell
/// whether it changed since.
pub type FileStamp = Option<(u64, Option<std::time::SystemTime>)>;

/// One settled presence answer and what it rests on, so a later page of the
/// same query can tell whether it still holds without reading any file.
#[derive(Clone, Debug)]
pub struct LiteralPresenceEntry {
    pub presence: LiteralPresence,
    /// The files whose text decided the answer, as they were when read: the
    /// holder for `Present`, every file read for `Absent`.
    pub read: Vec<(PathBuf, FileStamp)>,
    /// For `Absent`: every file the index said might hold the literal. A
    /// file that gains the literal joins this set, so a different set means
    /// the answer must be settled again.
    pub candidates: Vec<PathBuf>,
}

/// Key of a presence answer: the root searched, the index generation it was
/// settled on, and the literal as the exact lane compares phrases.
pub type LiteralPresenceKey = (PathBuf, GenerationToken, String);

// HashMap capacity counts usable slots, not all allocated buckets. Estimate
// inline keys/values, one occupancy-control byte per bucket, and a 16-byte
// control group; nested path/string buffers are counted separately below.
fn map_storage_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = capacity
        .saturating_add(1)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    buckets
        .saturating_mul(std::mem::size_of::<(K, V)>().saturating_add(1))
        .saturating_add(16)
}

fn key_bytes(key: &MemoKey) -> usize {
    let scope = match &key.evidence_scope {
        ExactEvidenceScope::DefinitionsMentioning(words) => words
            .capacity()
            .saturating_mul(std::mem::size_of::<String>())
            .saturating_add(words.iter().map(String::capacity).sum::<usize>()),
        _ => 0,
    };
    key.project_root
        .capacity()
        .saturating_add(key.snapshot_generation.as_str().len())
        .saturating_add(key.normalized_query.capacity())
        .saturating_add(scope)
}

fn literal_presence_bytes(key: &LiteralPresenceKey, entry: &LiteralPresenceEntry) -> usize {
    key.0
        .capacity()
        .saturating_add(key.1.as_str().len())
        .saturating_add(key.2.capacity())
        .saturating_add(
            entry
                .read
                .capacity()
                .saturating_mul(std::mem::size_of::<(PathBuf, FileStamp)>()),
        )
        .saturating_add(
            entry
                .read
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>(),
        )
        .saturating_add(
            entry
                .candidates
                .capacity()
                .saturating_mul(std::mem::size_of::<PathBuf>()),
        )
        .saturating_add(
            entry
                .candidates
                .iter()
                .map(PathBuf::capacity)
                .sum::<usize>(),
        )
}

/// Normalize query for memo key comparison.
pub fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Compute content digest for raw bytes using Blake3.
pub fn compute_content_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Compute content digest for a file on disk.
pub fn compute_file_content_digest(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| compute_content_digest(&b))
}

/// Memo key K = (project_root, snapshot_generation, normalized_query, include_tests,
/// evidence_scope).
///
/// Per spec:
/// Equality is the only permitted operation on snapshot_generation.
/// Neither epoch nor poisoned is part of this key.
///
/// The evidence scope is part of the key because a narrowly scoped pass skips
/// files whose only evidence is of a kind its caller drops; serving that entry
/// to a caller that keeps the kind would lose those files.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoKey {
    pub project_root: PathBuf,
    pub snapshot_generation: GenerationToken,
    pub normalized_query: String,
    pub include_tests: bool,
    pub evidence_scope: ExactEvidenceScope,
}

impl MemoKey {
    pub fn new(
        project_root: impl Into<PathBuf>,
        snapshot_generation: GenerationToken,
        query: &str,
        include_tests: bool,
    ) -> Self {
        Self {
            project_root: project_root.into(),
            snapshot_generation,
            normalized_query: normalize_query(query),
            include_tests,
            evidence_scope: ExactEvidenceScope::All,
        }
    }

    /// The same key for a pass that keeps only the evidence in `scope`.
    pub fn with_evidence_scope(mut self, scope: ExactEvidenceScope) -> Self {
        self.evidence_scope = scope;
        self
    }
}

/// Memo entry holding the verified candidate set, per-file digests, per-candidate descriptors,
/// internal epoch and per-key poisoned flag.
///
/// Per spec:
/// None of epoch/poisoned is emitted, keyed on, or readable by ranking.
#[derive(Clone, Debug)]
pub struct MemoEntry {
    pub results: Vec<CandidateResult>,
    pub file_digests: HashMap<PathBuf, String>,
    pub descriptors: HashMap<(PathBuf, Option<SymbolOffsetRange>), EvidenceDescriptor>,
    pub bound_disclosure: Option<String>,
    pub stability_void: bool,
    epoch: usize,
    poisoned: bool,
    retained_bytes: usize,
}

impl MemoEntry {
    pub fn new(
        results: Vec<CandidateResult>,
        file_digests: HashMap<PathBuf, String>,
        bound_disclosure: Option<String>,
        stability_void: bool,
        epoch: usize,
        poisoned: bool,
    ) -> Self {
        let mut descriptors = HashMap::new();
        for candidate in &results {
            descriptors.insert(
                (candidate.path.clone(), candidate.symbol_range),
                candidate.evidence.clone(),
            );
        }
        let retained_bytes = std::mem::size_of::<Self>()
            .saturating_add(
                results
                    .capacity()
                    .saturating_mul(std::mem::size_of::<CandidateResult>()),
            )
            .saturating_add(results.iter().map(|row| row.path.capacity()).sum::<usize>())
            .saturating_add(map_storage_bytes::<PathBuf, String>(
                file_digests.capacity(),
            ))
            .saturating_add(
                file_digests
                    .iter()
                    .map(|(path, digest)| path.capacity().saturating_add(digest.capacity()))
                    .sum::<usize>(),
            )
            .saturating_add(map_storage_bytes::<
                (PathBuf, Option<SymbolOffsetRange>),
                EvidenceDescriptor,
            >(descriptors.capacity()))
            .saturating_add(
                descriptors
                    .keys()
                    .map(|(path, _)| path.capacity())
                    .sum::<usize>(),
            )
            .saturating_add(bound_disclosure.as_ref().map_or(0, String::capacity));
        Self {
            results,
            file_digests,
            descriptors,
            bound_disclosure,
            stability_void,
            epoch,
            poisoned,
            retained_bytes,
        }
    }

    /// Internal epoch getter (not emitted in public output or read by ranking).
    pub fn internal_epoch(&self) -> usize {
        self.epoch
    }

    /// Internal poisoned flag getter (not emitted in public output or read by ranking).
    pub fn internal_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// Outcome of serving a page from the memo.
#[derive(Clone, Debug)]
pub struct ServeOutcome {
    pub results: Vec<CandidateResult>,
    pub bound_disclosure: Option<String>,
    pub void_disclosure: Option<String>,
    pub stability_void: bool,
    pub served_page_digest_mismatch: bool,
}

#[derive(Clone, Debug, Default)]
struct KeyLifecycleState {
    current_epoch: Option<usize>,
    poisoned: bool,
    retired_at: Option<usize>,
}

#[derive(Debug)]
pub enum MemoError {
    ReverificationRejected(usize),
    VerificationFailed(String),
}

impl fmt::Display for MemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReverificationRejected(epoch) => {
                write!(
                    f,
                    "re-verification of live memo entry at epoch {epoch} is rejected"
                )
            }
            Self::VerificationFailed(err) => write!(f, "verification failed: {err}"),
        }
    }
}

impl std::error::Error for MemoError {}

/// Verified exact set produced by the exact pass verifier.
#[derive(Clone, Debug)]
pub struct VerifiedExactSet {
    pub results: Vec<CandidateResult>,
    pub file_digests: HashMap<PathBuf, String>,
    pub bound_disclosure: Option<String>,
    pub stability_void: bool,
}

/// Thread-safe in-memory memo store for the exact lane.
///
/// Holds memoized exact sets keyed by K = (project_root, snapshot_generation, normalized_query, include_tests).
/// Manages epoch progression, poisoning, served-page content-digest re-check, and verifier counter.
pub struct ExactMemoStore {
    // Readers keep the immutable verified payload alive after releasing the
    // store lock. Only the served page needs an owned candidate copy.
    entries: RwLock<HashMap<MemoKey, Arc<MemoEntry>>>,
    lifecycle: RwLock<HashMap<MemoKey, KeyLifecycleState>>,
    in_flight: Mutex<HashSet<(MemoKey, usize)>>,
    in_flight_changed: Condvar,
    next_epoch: AtomicUsize,
    pub verifier_counter: Arc<AtomicUsize>,
    /// Presence answers for code literals, kept so every page of one query
    /// reuses the first page's answer instead of reading files again.
    literal_presence: Mutex<HashMap<LiteralPresenceKey, LiteralPresenceEntry>>,
    digest_proofs: Mutex<DigestProofCache>,
}

struct VerificationPermit<'a> {
    store: &'a ExactMemoStore,
    key: MemoKey,
    epoch: usize,
}

impl Drop for VerificationPermit<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.store.in_flight.lock();
        in_flight.remove(&(self.key.clone(), self.epoch));
        self.store.prune_lifecycle(&in_flight);
        self.store.in_flight_changed.notify_all();
    }
}

impl Default for ExactMemoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ExactMemoStore {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            lifecycle: RwLock::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            in_flight_changed: Condvar::new(),
            next_epoch: AtomicUsize::new(0),
            verifier_counter: Arc::new(AtomicUsize::new(0)),
            literal_presence: Mutex::new(HashMap::new()),
            digest_proofs: Mutex::new(DigestProofCache::default()),
        }
    }

    /// The presence answer an earlier page of this query settled, if any.
    pub fn literal_presence(&self, key: &LiteralPresenceKey) -> Option<LiteralPresenceEntry> {
        self.literal_presence.lock().get(key).cloned()
    }

    /// Keep a presence answer for later pages. The cache is small: when it
    /// is full it starts over, which only costs a later page one re-check.
    pub fn store_literal_presence(&self, key: LiteralPresenceKey, entry: LiteralPresenceEntry) {
        let mut cache = self.literal_presence.lock();
        let incoming = literal_presence_bytes(&key, &entry).saturating_add(map_storage_bytes::<
            LiteralPresenceKey,
            LiteralPresenceEntry,
        >(
            MAX_LITERAL_PRESENCE_ENTRIES,
        ));
        cache.retain(|old, _| old.0 != key.0 || old.2 != key.2 || old.1 == key.1);
        if incoming > MAX_LITERAL_PRESENCE_BYTES {
            cache.remove(&key);
            return;
        }
        let retained = cache
            .iter()
            .filter(|(old, _)| *old != &key)
            .map(|(key, entry)| literal_presence_bytes(key, entry))
            .sum::<usize>();
        if (cache.len() >= MAX_LITERAL_PRESENCE_ENTRIES && !cache.contains_key(&key))
            || retained.saturating_add(incoming) > MAX_LITERAL_PRESENCE_BYTES
        {
            cache.clear();
        }
        cache.insert(key, entry);
    }

    /// Number of verifier calls executed across all keys.
    pub fn verifier_call_count(&self) -> usize {
        self.verifier_counter.load(Ordering::SeqCst)
    }

    /// Check if a key currently has a live entry.
    pub fn has_live_entry(&self, key: &MemoKey) -> bool {
        self.entries.read().contains_key(key)
    }

    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.read().len()
    }

    /// Retained cache capacity, without reading any source files or cloning payloads.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let Some(entries) = self.entries.try_read() else {
            return crate::memory::MemoryEstimate::busy();
        };
        let Some(lifecycle) = self.lifecycle.try_read() else {
            return crate::memory::MemoryEstimate::busy();
        };
        let Some(presence) = self.literal_presence.try_lock() else {
            return crate::memory::MemoryEstimate::busy();
        };
        let Some(proofs) = self.digest_proofs.try_lock() else {
            return crate::memory::MemoryEstimate::busy();
        };
        let proof_bytes = proofs.estimated_bytes();
        let payload_bytes = entries.iter().fold(0usize, |bytes, (key, entry)| {
            bytes
                .saturating_add(key_bytes(key))
                .saturating_add(entry.retained_bytes)
        });
        let lifecycle_bytes = lifecycle.keys().map(key_bytes).sum::<usize>();
        let presence_bytes = presence
            .iter()
            .map(|(key, entry)| literal_presence_bytes(key, entry))
            .sum::<usize>();
        let bytes = payload_bytes
            .saturating_add(map_storage_bytes::<MemoKey, Arc<MemoEntry>>(
                entries.capacity(),
            ))
            .saturating_add(lifecycle_bytes)
            .saturating_add(map_storage_bytes::<MemoKey, KeyLifecycleState>(
                lifecycle.capacity(),
            ))
            .saturating_add(presence_bytes)
            .saturating_add(
                map_storage_bytes::<LiteralPresenceKey, LiteralPresenceEntry>(presence.capacity()),
            )
            .saturating_add(proof_bytes);
        crate::memory::MemoryEstimate::partial(bytes as u64)
            .count("entries", entries.len())
            .count(
                "candidates",
                entries.values().map(|entry| entry.results.len()).sum(),
            )
            .count(
                "file_digests",
                entries.values().map(|entry| entry.file_digests.len()).sum(),
            )
            .count(
                "descriptors",
                entries.values().map(|entry| entry.descriptors.len()).sum(),
            )
            .count("lifecycle_keys", lifecycle.len())
            .count("literal_presence_entries", presence.len())
            .count("digest_proof_entries", proofs.files.len())
            .count("digest_proof_bytes", proof_bytes)
            .count("digest_proof_budget_bytes", MAX_DIGEST_PROOF_BYTES)
            .count("payload_budget_bytes", MAX_EXACT_MEMO_BYTES)
            .gap("in_flight_verifiers_and_detached_readers")
    }

    fn allocate_epoch(&self) -> usize {
        // A retired key must never reuse an old verifier's publication ticket,
        // including after clear() removes all of its per-key state.
        let mut epoch = self.next_epoch.load(Ordering::Relaxed);
        loop {
            let next = epoch.checked_add(1).expect("exact memo epoch exhausted");
            match self.next_epoch.compare_exchange_weak(
                epoch,
                next,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(epoch) => return epoch,
                Err(current) => epoch = current,
            }
        }
    }

    fn prune_lifecycle(&self, in_flight: &HashSet<(MemoKey, usize)>) {
        // The admission gate is held by every caller. Cached entries carry
        // their own poison bit; only active work and retired poison need rows.
        let entries = self.entries.read();
        let mut lifecycle = self.lifecycle.write();
        lifecycle.retain(|key, state| {
            if in_flight.iter().any(|(active, _)| active == key) {
                state.retired_at = None;
                true
            } else if entries.contains_key(key) || !state.poisoned {
                false
            } else {
                state.current_epoch = None;
                state
                    .retired_at
                    .get_or_insert_with(|| self.allocate_epoch());
                true
            }
        });
        while lifecycle
            .values()
            .filter(|state| state.retired_at.is_some())
            .count()
            > MAX_RETIRED_POISONED_KEYS
            || lifecycle
                .iter()
                .filter(|(_, state)| state.retired_at.is_some())
                .map(|(key, _)| key_bytes(key))
                .sum::<usize>()
                > MAX_RETIRED_POISONED_BYTES
        {
            let oldest = lifecycle
                .iter()
                .filter_map(|(key, state)| state.retired_at.map(|retired| (key, retired)))
                .min_by_key(|(_, retired)| *retired)
                .map(|(key, _)| key.clone())
                .expect("retired poison exceeds its bound");
            lifecycle.remove(&oldest);
        }
    }

    // The result set is still served in full. Only cache ownership is bounded:
    // an old index generation cannot answer this query on a newer snapshot.
    fn make_room_for(&self, key: &MemoKey, incoming_bytes: usize) -> bool {
        let mut entries = self.entries.write();
        let obsolete: Vec<_> = entries
            .keys()
            .filter(|old| {
                old.project_root == key.project_root
                    && old.normalized_query == key.normalized_query
                    && old.include_tests == key.include_tests
                    && old.evidence_scope == key.evidence_scope
                    && old.snapshot_generation != key.snapshot_generation
            })
            .cloned()
            .collect();
        let retire = |entries: &mut HashMap<MemoKey, Arc<MemoEntry>>, victim: MemoKey| {
            let entry = entries.remove(&victim).expect("selected payload exists");
            if entry.poisoned {
                let mut lifecycle = self.lifecycle.write();
                let state = lifecycle.entry(victim).or_default();
                state.poisoned = true;
                state.retired_at = None;
            }
        };
        for old in obsolete {
            retire(&mut entries, old);
        }
        let incoming = incoming_bytes
            .saturating_add(key_bytes(key))
            .saturating_add(map_storage_bytes::<MemoKey, Arc<MemoEntry>>(
                MAX_EXACT_MEMO_ENTRIES,
            ));
        if incoming > MAX_EXACT_MEMO_BYTES {
            return false;
        }
        while entries.len() >= MAX_EXACT_MEMO_ENTRIES
            || entries
                .iter()
                .map(|(key, entry)| key_bytes(key).saturating_add(entry.retained_bytes))
                .sum::<usize>()
                .saturating_add(incoming)
                > MAX_EXACT_MEMO_BYTES
        {
            let Some(victim) = entries.keys().next().cloned() else {
                break;
            };
            retire(&mut entries, victim);
        }
        true
    }

    fn invalidate_observed_entry(&self, key: &MemoKey, entry: &Arc<MemoEntry>) {
        let admission = self.in_flight.lock();
        let mut entries = self.entries.write();
        if entries
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(key);
            let mut lifecycle = self.lifecycle.write();
            let state = lifecycle.entry(key.clone()).or_default();
            state.poisoned = true;
            state.current_epoch = None;
            state.retired_at = None;
        }
        drop(entries);
        self.prune_lifecycle(&admission);
    }

    /// Read the internal epoch of a key if present.
    pub fn get_epoch(&self, key: &MemoKey) -> Option<usize> {
        self.entries.read().get(key).map(|e| e.epoch)
    }

    /// Read the internal poisoned flag of a key if present in entry or lifecycle.
    pub fn is_key_poisoned(&self, key: &MemoKey) -> bool {
        if let Some(entry) = self.entries.read().get(key) {
            return entry.poisoned;
        }
        self.lifecycle.read().get(key).is_some_and(|s| s.poisoned)
    }

    /// Re-check the digests of exactly the files backing the served page.
    /// Returns Ok(()) if all match, or Err(mismatched_path) on any mismatch or missing file.
    pub fn check_served_page_digests(
        page: &[CandidateResult],
        file_digests: &HashMap<PathBuf, String>,
    ) -> Result<(), PathBuf> {
        Self::check_page_digests(page, file_digests, |path, expected| {
            let request_files = super::request_files::current();
            if read_digest(path, request_files.as_deref())? != expected {
                return Err(path.to_path_buf());
            }
            Ok(())
        })
    }

    fn check_page_digests(
        page: &[CandidateResult],
        file_digests: &HashMap<PathBuf, String>,
        mut check: impl FnMut(&Path, &str) -> Result<(), PathBuf>,
    ) -> Result<(), PathBuf> {
        let mut checked = HashSet::new();
        for candidate in page {
            if !checked.insert(&candidate.path) {
                continue;
            }
            let Some(expected_digest) = file_digests.get(&candidate.path) else {
                continue;
            };
            check(&candidate.path, expected_digest)?;
        }
        Ok(())
    }

    fn check_file_digest(&self, path: &Path, expected: &str) -> Result<(), PathBuf> {
        self.check_file_digest_with_read_hooks(path, expected, || {}, || {})
    }

    fn check_file_digest_with_read_hooks(
        &self,
        path: &Path,
        expected: &str,
        before_read: impl FnOnce(),
        after_read: impl FnOnce(),
    ) -> Result<(), PathBuf> {
        // A file this search request already read is checked from those
        // bytes: every phase of one request sees the same version of a file.
        let request_files = super::request_files::current();
        if let Some(file) = request_files.as_ref().and_then(|files| files.kept(path)) {
            return if file.digest().to_hex().as_str() == expected {
                Ok(())
            } else {
                Err(path.to_path_buf())
            };
        }
        // Windows can preserve both last-write and change time across an
        // immediate same-length rewrite. Even an old, apparently stable stamp
        // cannot certify unchanged content there, so always hash the bytes.
        if cfg!(windows) {
            before_read();
            let digest = read_digest(path, request_files.as_deref())?;
            after_read();
            return if digest == expected {
                Ok(())
            } else {
                Err(path.to_path_buf())
            };
        }
        let observed_ns = now_ns();
        let before = regular_file_stat(path);
        if let Some(current) = &before {
            let proofs = self.digest_proofs.lock();
            if let Some(proof) = proofs
                .files
                .get(path)
                .filter(|proof| proof.stat.reusable(current, proof.observed_ns))
            {
                return if proof.digest == expected {
                    Ok(())
                } else {
                    Err(path.to_path_buf())
                };
            }
        }

        #[cfg(test)]
        crate::search_hot_path_measurements::record_memo_file_read(path);
        before_read();
        let read_wait = super::waits::time(super::waits::SearchWait::FileRead);
        let mut file = fs::File::open(path).map_err(|_| path.to_path_buf())?;
        let opened = NativeStat::read_open_file(&file, path).ok();
        let read_stat = file
            .metadata()
            .ok()
            .map(|metadata| super::request_files::FileStat::of(&metadata));
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| path.to_path_buf())?;
        drop(read_wait);
        #[cfg(test)]
        super::request_files::record_disk_read(path);
        let digest = compute_content_digest(&bytes);
        after_read();
        // Later phases of this request (snippets, matching lines) read the
        // same file; keep the bytes for them.
        if let Some(files) = &request_files {
            files.keep_read(path, bytes, read_stat);
        }
        if digest != expected {
            return Err(path.to_path_buf());
        }
        if let (Some(before), Some(after)) = (before, regular_file_stat(path)) {
            // The content hash and both stamps belong to this read, not a later
            // sweep. Preserve that observation boundary on hits; recent/coarse
            // timestamps always reread bytes.
            if before.reusable(&after, observed_ns)
                && opened.as_ref() == Some(&before)
                && NativeStat::read_open_file(&file, path).ok().as_ref() == Some(&after)
            {
                self.digest_proofs.lock().insert(
                    path,
                    DigestProof {
                        stat: before,
                        observed_ns,
                        digest,
                    },
                );
            }
        }
        Ok(())
    }

    /// Serve the requested page from a cached exact set, or run its verifier.
    ///
    /// One verifier is active per key/epoch; failed attempts remain retryable.
    /// A digest mismatch retires only the observed payload. Detached old pages
    /// report void stability without invalidating a newer replacement payload.
    /// Poison survives rebuilds while the key is cached or in the bounded retired
    /// history (128 keys, at most 1 MiB of key buffers), because pages across
    /// changed contents are not one stable set.
    pub fn get_or_verify<F>(
        &self,
        key: &MemoKey,
        offset: usize,
        top_k: usize,
        verifier: F,
    ) -> Result<ServeOutcome, MemoError>
    where
        F: FnOnce() -> Result<VerifiedExactSet, MemoError>,
    {
        self.get_or_verify_with_miss_hook(key, offset, top_k, verifier, || {})
    }

    fn get_or_verify_with_miss_hook<F, H>(
        &self,
        key: &MemoKey,
        offset: usize,
        top_k: usize,
        verifier: F,
        miss_hook: H,
    ) -> Result<ServeOutcome, MemoError>
    where
        F: FnOnce() -> Result<VerifiedExactSet, MemoError>,
        H: FnOnce(),
    {
        // 1. Check if we have an active entry
        let existing_entry = super::waits::timed(super::waits::SearchWait::ExactMemo, || {
            self.entries.read().get(key).cloned()
        });

        if let Some(entry) = existing_entry {
            // Slice the requested page
            let total_len = entry.results.len();
            let page = if offset < total_len {
                let end = (offset + top_k).min(total_len);
                &entry.results[offset..end]
            } else {
                &[]
            };

            // Re-check digests of exactly the files backing the served page
            if let Err(_mismatched_file) =
                Self::check_page_digests(page, &entry.file_digests, |path, expected| {
                    self.check_file_digest(path, expected)
                })
            {
                // Digest mismatch!
                // Drops the entry and poisons K without re-verifying (counter still unchanged).
                self.invalidate_observed_entry(key, &entry);

                return Ok(ServeOutcome {
                    results: page.to_vec(),
                    bound_disclosure: entry.bound_disclosure.clone(),
                    void_disclosure: Some("content changed - page stability void".to_string()),
                    stability_void: true,
                    served_page_digest_mismatch: true,
                });
            }

            // Digest check passed.
            let void_disclosure = if entry.poisoned {
                Some("content changed - page stability void".to_string())
            } else if entry.stability_void {
                Some(entry.bound_disclosure.clone().unwrap_or_else(|| {
                    "exact pass: bounded (time limit) - page stability void".to_string()
                }))
            } else {
                None
            };

            return Ok(ServeOutcome {
                results: page.to_vec(),
                bound_disclosure: entry.bound_disclosure.clone(),
                void_disclosure,
                stability_void: entry.stability_void || entry.poisoned,
                served_page_digest_mismatch: false,
            });
        }

        miss_hook();
        // Admission, publication and eviction share this gate. Store lock order
        // is admission -> entries -> lifecycle; disk verification holds none.
        // Waiting for the gate, and for another request verifying the same
        // key, is the memo wait; this request's own verifier is not.
        let memo_wait = super::waits::time(super::waits::SearchWait::ExactMemo);
        let mut in_flight = self.in_flight.lock();
        if self.entries.read().contains_key(key) {
            drop(in_flight);
            drop(memo_wait);
            return self.get_or_verify(key, offset, top_k, verifier);
        }
        let current_epoch = self
            .lifecycle
            .read()
            .get(key)
            .and_then(|state| state.current_epoch);
        if let Some(flight_key) = current_epoch
            .map(|epoch| (key.clone(), epoch))
            .filter(|flight_key| in_flight.contains(flight_key))
        {
            while in_flight.contains(&flight_key) {
                if crate::executor::current_job_cancelled() {
                    return Err(MemoError::VerificationFailed(
                        "exact verification cancelled while waiting for an in-flight request"
                            .to_string(),
                    ));
                }
                self.in_flight_changed
                    .wait_for(&mut in_flight, Duration::from_millis(25));
            }
            drop(in_flight);
            drop(memo_wait);
            return self.get_or_verify(key, offset, top_k, verifier);
        }
        drop(memo_wait);
        let epoch = self.allocate_epoch();
        let is_poisoned = {
            let mut lifecycle = self.lifecycle.write();
            let state = lifecycle.entry(key.clone()).or_default();
            state.current_epoch = Some(epoch);
            state.retired_at = None;
            state.poisoned
        };
        in_flight.insert((key.clone(), epoch));
        self.prune_lifecycle(&in_flight);
        drop(in_flight);
        let permit = VerificationPermit {
            store: self,
            key: key.clone(),
            epoch,
        };

        // Run verifier (increments instrumented counter)
        self.verifier_counter.fetch_add(1, Ordering::SeqCst);
        let verified_set = verifier()?;

        let entry = Arc::new(MemoEntry::new(
            verified_set.results,
            verified_set.file_digests,
            verified_set.bound_disclosure,
            verified_set.stability_void,
            epoch,
            is_poisoned,
        ));

        // Publish before dropping the permit so same-key waiters observe the entry.
        {
            let admission = super::waits::timed(super::waits::SearchWait::ExactMemo, || {
                self.in_flight.lock()
            });
            let current_epoch = self
                .lifecycle
                .read()
                .get(key)
                .and_then(|state| state.current_epoch);
            if current_epoch != Some(epoch) {
                return Err(MemoError::VerificationFailed(
                    "exact verification superseded by memo lifecycle change".to_string(),
                ));
            }
            if self.make_room_for(key, entry.retained_bytes) {
                self.entries.write().insert(key.clone(), entry.clone());
            }
            self.prune_lifecycle(&admission);
        }
        drop(permit);

        // Slice served page
        let total_len = entry.results.len();
        let page = if offset < total_len {
            let end = (offset + top_k).min(total_len);
            &entry.results[offset..end]
        } else {
            &[]
        };

        let void_disclosure = if is_poisoned {
            Some("content changed - page stability void".to_string())
        } else if entry.stability_void {
            Some(entry.bound_disclosure.clone().unwrap_or_else(|| {
                "exact pass: bounded (time limit) - page stability void".to_string()
            }))
        } else {
            None
        };

        Ok(ServeOutcome {
            results: page.to_vec(),
            bound_disclosure: entry.bound_disclosure.clone(),
            void_disclosure,
            stability_void: entry.stability_void || is_poisoned,
            served_page_digest_mismatch: false,
        })
    }

    /// Drop entries and poison history without reusing publication epochs.
    pub fn clear(&self) {
        *self.digest_proofs.lock() = DigestProofCache::default();
        let _admission = self.in_flight.lock();
        *self.entries.write() = HashMap::new();
        *self.lifecycle.write() = HashMap::new();
        *self.literal_presence.lock() = HashMap::new();
    }

    /// Idle reaping never waits for a verifier or a reader holding a cache lock.
    pub(crate) fn try_clear_idle(&self) -> bool {
        let Some(admission) = self.in_flight.try_lock() else {
            return false;
        };
        if !admission.is_empty() {
            return false;
        }
        let Some(mut entries) = self.entries.try_write() else {
            return false;
        };
        let Some(mut lifecycle) = self.lifecycle.try_write() else {
            return false;
        };
        let Some(mut presence) = self.literal_presence.try_lock() else {
            return false;
        };
        let Some(mut proofs) = self.digest_proofs.try_lock() else {
            return false;
        };
        let retired = (
            std::mem::take(&mut *entries),
            std::mem::take(&mut *lifecycle),
            std::mem::take(&mut *presence),
            std::mem::take(&mut *proofs),
        );
        drop((proofs, presence, lifecycle, entries, admission));
        drop(retired);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit01_empty_set() -> Result<VerifiedExactSet, MemoError> {
        Ok(VerifiedExactSet {
            results: Vec::new(),
            file_digests: HashMap::new(),
            bound_disclosure: None,
            stability_void: false,
        })
    }

    fn audit01_key(query: &str) -> MemoKey {
        MemoKey::new(
            "/project",
            GenerationToken::new_with_str("generation"),
            query,
            false,
        )
    }

    fn retire_poisoned_key(memo: &ExactMemoStore, key: &MemoKey) {
        memo.get_or_verify(key, 0, 1, audit01_empty_set).unwrap();
        let observed = memo.entries.read().get(key).unwrap().clone();
        memo.invalidate_observed_entry(key, &observed);
    }

    fn write_old_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_700_000_000, 123))
            .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn exact_memo_windows_immediate_same_length_rewrite_invalidates_cached_page() {
        let root = tempfile::tempdir().unwrap();
        for backdated in [false, true] {
            let path = root.path().join(format!("file-{backdated}.rs"));
            if backdated {
                write_old_file(&path, b"old");
            } else {
                fs::write(&path, b"old").unwrap();
            }
            let memo = ExactMemoStore::new();
            let key = MemoKey::new(
                root.path(),
                GenerationToken::new_with_str("fixed"),
                "old",
                true,
            );
            memo.get_or_verify(&key, 0, 10, || {
                Ok(VerifiedExactSet {
                    results: vec![CandidateResult::new_exact(
                        path.clone(),
                        None,
                        EvidenceDescriptor::for_e1(1, true, false),
                    )],
                    file_digests: HashMap::from([(path.clone(), compute_content_digest(b"old"))]),
                    bound_disclosure: None,
                    stability_void: false,
                })
            })
            .unwrap();
            memo.get_or_verify(&key, 0, 10, || panic!("memo hit"))
                .unwrap();
            assert!(
                memo.digest_proofs.lock().files.is_empty(),
                "Windows must not retain stat-only content proofs"
            );
            let observed = now_ns();
            let before = NativeStat::read(&path).ok();
            // Do not sleep or restore timestamps after this same-length write.
            fs::write(&path, b"new").unwrap();
            let after = NativeStat::read(&path).ok();
            let stat_would_reuse = before
                .as_ref()
                .zip(after.as_ref())
                .map(|(a, b)| a.reusable(b, observed));
            eprintln!("Windows immediate rewrite: backdated={backdated}, native_stat_would_reuse={stat_would_reuse:?}, before={before:?}, after={after:?}");
            crate::search_hot_path_measurements::reset();
            let changed = memo
                .get_or_verify(&key, 0, 10, || {
                    panic!("must invalidate without rebuilding the page")
                })
                .unwrap();
            assert!(changed.served_page_digest_mismatch);
            assert!(memo.is_key_poisoned(&key));
            assert!(!memo.has_live_entry(&key));
            assert_eq!(
                crate::search_hot_path_measurements::counts().memo_digest_reads,
                1
            );
        }
    }

    #[test]
    fn exact_memo_census_counts_digest_proof_capacity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        write_old_file(&path, b"old");
        let memo = ExactMemoStore::new();
        let before = memo.estimated_memory().estimated_bytes.unwrap();
        let mut retained_path = PathBuf::with_capacity(16 * 1024);
        retained_path.push(&path);
        let mut digest = String::with_capacity(8 * 1024);
        digest.push_str(&compute_content_digest(b"old"));
        let nested_bytes = retained_path.capacity() + digest.capacity();
        // Deliberately spare capacity makes a length-only estimate fail.
        let mut proofs = memo.digest_proofs.lock();
        proofs.files.reserve(128);
        proofs.files.insert(
            retained_path,
            DigestProof {
                stat: NativeStat::read(&path).unwrap(),
                observed_ns: now_ns(),
                digest,
            },
        );
        let inline_bytes = proofs.files.capacity() * std::mem::size_of::<(PathBuf, DigestProof)>();
        drop(proofs);
        let estimate = memo.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() >= before + (nested_bytes + inline_bytes) as u64);
        assert_eq!(estimate.counts["digest_proof_entries"], 1);
        assert!(estimate.counts["digest_proof_bytes"] >= (nested_bytes + inline_bytes) as u64);
        assert_eq!(
            estimate.counts["digest_proof_budget_bytes"],
            MAX_DIGEST_PROOF_BYTES as u64
        );
    }

    #[test]
    fn exact_memo_idle_clear_releases_digest_proofs_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        write_old_file(&path, b"old");
        let memo = ExactMemoStore::new();
        let key = audit01_key("idle");
        memo.get_or_verify(&key, 0, 10, audit01_empty_set).unwrap();
        memo.digest_proofs.lock().insert(
            &path,
            DigestProof {
                stat: NativeStat::read(&path).unwrap(),
                observed_ns: now_ns(),
                digest: compute_content_digest(b"old"),
            },
        );
        let held_proofs = memo.digest_proofs.lock();
        assert!(
            !memo.try_clear_idle(),
            "idle reaping must not wait for a proof reader"
        );
        assert!(
            memo.has_live_entry(&key),
            "a refused reap must leave other memo state intact"
        );
        drop(held_proofs);
        assert!(memo.estimated_memory().estimated_bytes.unwrap() > 0);
        assert!(memo.try_clear_idle());
        let proofs = memo.digest_proofs.lock();
        assert_eq!(proofs.files.capacity(), 0);
        assert_eq!(proofs.payload_bytes, 0);
        drop(proofs);
        let estimate = memo.estimated_memory();
        assert_eq!(estimate.estimated_bytes, Some(0));
        assert_eq!(estimate.counts["digest_proof_entries"], 0);
        assert_eq!(estimate.counts["digest_proof_bytes"], 0);
        assert!(!memo.has_live_entry(&key));
    }

    #[test]
    fn exact_memo_warm_full_set_reuses_content_proofs() {
        let root = tempfile::tempdir().unwrap();
        let memo = ExactMemoStore::new();
        let mut results = Vec::new();
        let mut digests = HashMap::new();
        let mut reusable = true;
        for index in 0..200 {
            let path = root.path().join(format!("file-{index}.rs"));
            write_old_file(&path, b"fn marker() {}\n");
            reusable &= regular_file_stat(&path).unwrap().reuse_supported && !cfg!(windows);
            digests.insert(path.clone(), compute_content_digest(b"fn marker() {}\n"));
            for _ in 0..40 {
                results.push(CandidateResult::new_exact(
                    path.clone(),
                    None,
                    EvidenceDescriptor::for_e1(1, true, false),
                ));
            }
        }
        let key = MemoKey::new(
            root.path(),
            GenerationToken::new_with_str("fixed"),
            "marker",
            true,
        );
        let first = memo
            .get_or_verify(&key, 0, usize::MAX, || {
                Ok(VerifiedExactSet {
                    results,
                    file_digests: digests,
                    bound_disclosure: None,
                    stability_void: false,
                })
            })
            .unwrap();
        assert_eq!(first.results.len(), 8_000);
        crate::search_hot_path_measurements::reset();
        let warm = memo
            .get_or_verify(&key, 0, usize::MAX, || panic!("memo hit"))
            .unwrap();
        assert!(!warm.served_page_digest_mismatch);
        assert_eq!(
            crate::search_hot_path_measurements::counts().memo_digest_reads,
            200
        );
        crate::search_hot_path_measurements::reset();
        let hot = memo
            .get_or_verify(&key, 0, usize::MAX, || panic!("memo hit"))
            .unwrap();
        let reads = crate::search_hot_path_measurements::counts().memo_digest_reads;
        assert_eq!(hot.results.len(), 8_000);
        assert!(!hot.served_page_digest_mismatch);
        assert_eq!(reads, if reusable { 0 } else { 200 });
        assert_eq!(memo.verifier_call_count(), 1);
        eprintln!("full exact memo set: 8000 rows / 200 files; first hit=200 reads; proof hit={reads} reads");
        memo.clear();
        assert!(memo.digest_proofs.lock().files.is_empty());
    }

    #[test]
    fn exact_memo_proofs_reject_restored_mtime_edits_replacements_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        let digest = compute_content_digest(b"old");
        let memo = ExactMemoStore::new();
        write_old_file(&path, b"old");
        memo.check_file_digest(&path, &digest).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
        fs::write(&path, b"new").unwrap();
        filetime::set_file_mtime(&path, mtime).unwrap();
        assert_eq!(memo.check_file_digest(&path, &digest), Err(path.clone()));

        write_old_file(&path, b"old");
        memo.check_file_digest(&path, &digest).unwrap();
        crate::file_identity::replace_keeping_times(&path, b"new");
        assert_eq!(memo.check_file_digest(&path, &digest), Err(path.clone()));
        fs::remove_file(&path).unwrap();
        assert_eq!(memo.check_file_digest(&path, &digest), Err(path));
    }

    #[test]
    fn exact_memo_proofs_keep_observation_fence_and_fall_back_when_unsupported() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        write_old_file(&path, b"old");
        let digest = compute_content_digest(b"old");
        let stat = regular_file_stat(&path).unwrap();
        let memo = ExactMemoStore::new();
        for unsupported in [false, true] {
            let mut proof_stat = stat.clone();
            proof_stat.reuse_supported &= !unsupported;
            let observed_ns = stat.modified_ns as u128;
            memo.digest_proofs.lock().insert(
                &path,
                DigestProof {
                    stat: proof_stat,
                    observed_ns,
                    digest: digest.clone(),
                },
            );
            crate::search_hot_path_measurements::reset();
            memo.check_file_digest(&path, &digest).unwrap();
            assert_eq!(
                crate::search_hot_path_measurements::counts().memo_digest_reads,
                1
            );
        }
        memo.check_file_digest(&path, &digest).unwrap();
        let observed = memo
            .digest_proofs
            .lock()
            .files
            .get(&path)
            .map(|p| p.observed_ns);
        memo.check_file_digest(&path, &digest).unwrap();
        assert_eq!(
            memo.digest_proofs
                .lock()
                .files
                .get(&path)
                .map(|p| p.observed_ns),
            observed
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_memo_proofs_do_not_follow_symlinks_or_rebind_raced_reads() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        write_old_file(&a.join("file.rs"), b"old");
        write_old_file(&b.join("file.rs"), b"new");
        let link = root.path().join("link.rs");
        symlink(a.join("file.rs"), &link).unwrap();
        let memo = ExactMemoStore::new();
        memo.check_file_digest(&link, &compute_content_digest(b"old"))
            .unwrap();
        assert!(!memo.digest_proofs.lock().files.contains_key(&link));
        write_old_file(&a.join("file.rs"), b"new");
        assert!(memo
            .check_file_digest(&link, &compute_content_digest(b"old"))
            .is_err());

        write_old_file(&a.join("file.rs"), b"old");
        let directory = root.path().join("directory");
        symlink(&a, &directory).unwrap();
        let path = directory.join("file.rs");
        let digest = compute_content_digest(b"new");
        memo.check_file_digest_with_read_hooks(
            &path,
            &digest,
            || {
                fs::remove_file(&directory).unwrap();
                symlink(&b, &directory).unwrap();
            },
            || {
                fs::remove_file(&directory).unwrap();
                symlink(&a, &directory).unwrap();
            },
        )
        .unwrap();
        assert!(!memo.digest_proofs.lock().files.contains_key(&path));
        assert!(memo.check_file_digest(&path, &digest).is_err());
    }

    #[test]
    fn exact_memo_digest_proof_retention_is_bounded_without_skipping_reads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        write_old_file(&path, b"old");
        let memo = ExactMemoStore::new();
        let proof = DigestProof {
            stat: regular_file_stat(&path).unwrap(),
            observed_ns: now_ns(),
            digest: compute_content_digest(b"old"),
        };
        for index in 0..10_000 {
            memo.digest_proofs
                .lock()
                .insert(&root.path().join(format!("{index}.rs")), proof.clone());
        }
        assert!(memo.digest_proofs.lock().payload_bytes <= MAX_DIGEST_PROOF_BYTES);
        assert!(memo.digest_proofs.lock().files.len() < 10_000);
        let oversized = DigestProof {
            digest: "x".repeat(MAX_DIGEST_PROOF_BYTES + 1),
            ..proof
        };
        memo.digest_proofs.lock().insert(&path, oversized);
        crate::search_hot_path_measurements::reset();
        memo.check_file_digest(&path, &compute_content_digest(b"old"))
            .unwrap();
        assert_eq!(
            crate::search_hot_path_measurements::counts().memo_digest_reads,
            1
        );
    }

    #[test]
    fn exact_memo_lifecycle_is_bounded_after_ten_thousand_queries() {
        let memo = ExactMemoStore::new();
        for index in 0..10_000 {
            let key = MemoKey::new(
                format!("/project-{index}"),
                GenerationToken::new_with_str(&format!("generation-{index}")),
                &format!("query-{index}"),
                false,
            );
            if index % 2 == 0 {
                retire_poisoned_key(&memo, &key);
            } else {
                memo.get_or_verify(&key, 0, 1, audit01_empty_set).unwrap();
            }
        }
        assert_eq!(memo.entry_count(), 128);
        assert_eq!(memo.lifecycle.read().len(), 128);
        assert!(memo.lifecycle.read().values().all(|state| state.poisoned
            && state.current_epoch.is_none()
            && state.retired_at.is_some()));
        assert!(memo.in_flight.lock().is_empty());
    }

    #[test]
    fn exact_memo_forgets_oldest_poison_on_129th_retirement() {
        let memo = ExactMemoStore::new();
        for index in 0..128 {
            retire_poisoned_key(&memo, &audit01_key(&format!("poison-{index}")));
        }
        assert_eq!(memo.lifecycle.read().len(), 128);
        assert!(memo.is_key_poisoned(&audit01_key("poison-0")));
        retire_poisoned_key(&memo, &audit01_key("poison-128"));
        assert_eq!(memo.lifecycle.read().len(), 128);
        assert!(!memo.is_key_poisoned(&audit01_key("poison-0")));
        for index in 1..=128 {
            assert!(memo.is_key_poisoned(&audit01_key(&format!("poison-{index}"))));
        }
    }

    #[test]
    fn exact_memo_cached_poison_survives_retired_history_churn() {
        let memo = ExactMemoStore::new();
        let cached = audit01_key("cached poison");
        retire_poisoned_key(&memo, &cached);
        assert!(
            memo.get_or_verify(&cached, 0, 1, audit01_empty_set)
                .unwrap()
                .stability_void
        );
        for index in 0..129 {
            retire_poisoned_key(&memo, &audit01_key(&format!("retired-{index}")));
        }
        assert_eq!(memo.lifecycle.read().len(), 128);
        assert!(!memo.lifecycle.read().contains_key(&cached));
        assert!(memo.has_live_entry(&cached));
        assert!(memo.is_key_poisoned(&cached));
        assert!(
            memo.get_or_verify(&cached, 0, 1, || panic!("cached poison must not reverify"))
                .unwrap()
                .stability_void
        );
    }

    #[test]
    fn exact_memo_returning_key_rejects_old_verifier_publication() {
        let memo = Arc::new(ExactMemoStore::new());
        let key = audit01_key("returning");
        let (old_started_tx, old_started_rx) = std::sync::mpsc::channel();
        let (old_release_tx, old_release_rx) = std::sync::mpsc::channel();
        let old = {
            let memo = memo.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                memo.get_or_verify(&key, 0, 1, || {
                    old_started_tx.send(()).unwrap();
                    old_release_rx.recv().unwrap();
                    audit01_empty_set()
                })
            })
        };
        old_started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let old_epoch = memo
            .lifecycle
            .read()
            .get(&key)
            .unwrap()
            .current_epoch
            .unwrap();
        memo.clear();
        assert!(!memo.lifecycle.read().contains_key(&key));
        let (new_started_tx, new_started_rx) = std::sync::mpsc::channel();
        let (new_release_tx, new_release_rx) = std::sync::mpsc::channel();
        let new = {
            let memo = memo.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                memo.get_or_verify(&key, 0, 1, || {
                    new_started_tx.send(()).unwrap();
                    new_release_rx.recv().unwrap();
                    audit01_empty_set()
                })
            })
        };
        new_started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let new_epoch = memo
            .lifecycle
            .read()
            .get(&key)
            .unwrap()
            .current_epoch
            .unwrap();
        old_release_tx.send(()).unwrap();
        let old_result = old.join().unwrap();
        new_release_tx.send(()).unwrap();
        let new_result = new.join().unwrap();
        assert!(
            old_result.is_err(),
            "a retired epoch published over a returning key"
        );
        assert!(new_result.is_ok());
        assert!(new_epoch > old_epoch);
        assert_eq!(memo.get_epoch(&key), Some(new_epoch));
        assert!(!memo.is_key_poisoned(&key));
    }

    #[test]
    fn audit01_memo_rechecks_publication_after_initial_miss() {
        let memo = Arc::new(ExactMemoStore::new());
        let key = audit01_key("race");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let pending = {
            let memo = Arc::clone(&memo);
            let key = key.clone();
            std::thread::spawn(move || {
                memo.get_or_verify_with_miss_hook(
                    &key,
                    0,
                    1,
                    || panic!("a published entry must suppress a second verifier"),
                    || {
                        reached_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                )
            })
        };
        reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        memo.get_or_verify(&key, 0, 1, audit01_empty_set).unwrap();
        assert!(memo.in_flight.lock().is_empty());
        release_tx.send(()).unwrap();
        assert!(pending
            .join()
            .expect("pending request must serve publication")
            .is_ok());
        assert_eq!(memo.verifier_call_count(), 1);
    }

    #[test]
    fn audit01_capacity_keeps_poison_and_active_epoch() {
        let memo = Arc::new(ExactMemoStore::new());
        for index in 0..MAX_EXACT_MEMO_ENTRIES {
            memo.get_or_verify(
                &audit01_key(&format!("fill-{index}")),
                0,
                1,
                audit01_empty_set,
            )
            .unwrap();
        }
        let poisoned = memo.entries.read().keys().next().unwrap().clone();
        let poisoned_epoch = memo.get_epoch(&poisoned).unwrap();
        memo.entries.write().insert(
            poisoned.clone(),
            Arc::new(MemoEntry::new(
                Vec::new(),
                HashMap::new(),
                None,
                false,
                poisoned_epoch,
                true,
            )),
        );
        let active = audit01_key("active");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = {
            let memo = Arc::clone(&memo);
            let active = active.clone();
            std::thread::spawn(move || {
                memo.get_or_verify(&active, 0, 1, || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    audit01_empty_set()
                })
            })
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let initial_active_epoch = memo
            .lifecycle
            .read()
            .get(&active)
            .unwrap()
            .current_epoch
            .unwrap();
        memo.get_or_verify(&audit01_key("overflow"), 0, 1, audit01_empty_set)
            .unwrap();
        let active_epoch = memo
            .lifecycle
            .read()
            .get(&active)
            .and_then(|state| state.current_epoch);
        let poison_state = memo.lifecycle.read().get(&poisoned).cloned();
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(active_epoch, Some(initial_active_epoch));
        let poison_state = poison_state.expect("eviction must retain recent retired poison");
        assert!(poison_state.poisoned);
        assert_eq!(poison_state.current_epoch, None);
        assert!(
            memo.get_or_verify(&poisoned, 0, 1, audit01_empty_set)
                .unwrap()
                .stability_void
        );
        assert!(memo.get_epoch(&poisoned).unwrap() > initial_active_epoch);
        assert!(memo.entry_count() <= MAX_EXACT_MEMO_ENTRIES);
    }

    #[test]
    fn audit01_epoch_replaced_before_publication_is_rejected() {
        let memo = ExactMemoStore::new();
        let key = audit01_key("epoch");
        assert!(memo
            .get_or_verify(&key, 0, 1, || {
                memo.clear();
                audit01_empty_set()
            })
            .is_err());
        assert!(!memo.has_live_entry(&key));
        assert!(memo.in_flight.lock().is_empty());
    }

    #[test]
    fn audit01_stale_reader_cannot_erase_replacement() {
        let memo = ExactMemoStore::new();
        for index in 0..MAX_EXACT_MEMO_ENTRIES {
            memo.get_or_verify(
                &audit01_key(&format!("fill-{index}")),
                0,
                1,
                audit01_empty_set,
            )
            .unwrap();
        }
        let observed = memo.entries.read().keys().next().unwrap().clone();
        let old = memo.entries.read().get(&observed).unwrap().clone();
        memo.get_or_verify(&audit01_key("overflow"), 0, 1, audit01_empty_set)
            .unwrap();
        memo.get_or_verify(&observed, 0, 1, audit01_empty_set)
            .unwrap();
        let replacement = memo.entries.read().get(&observed).unwrap().clone();
        memo.invalidate_observed_entry(&observed, &old);
        assert!(memo
            .entries
            .read()
            .get(&observed)
            .is_some_and(|entry| Arc::ptr_eq(entry, &replacement)));
        assert!(replacement.epoch > old.epoch);
        assert_eq!(memo.get_epoch(&observed), Some(replacement.epoch));
        assert!(!memo.is_key_poisoned(&observed));
    }

    #[test]
    #[ignore = "opt-in warm exact-memo scaling measurement"]
    fn warm_exact_memo_cost_by_file_count() {
        use crate::search_hot_path_measurements::{counts, reset};
        for file_count in [10, 100, 1_000] {
            let temp = tempfile::tempdir().unwrap();
            let bytes = format!("pub fn symbol() {{}}\n{}", "// source payload\n".repeat(60));
            let paths = (0..file_count)
                .map(|ordinal| {
                    let path = temp.path().join(format!("file_{ordinal}.rs"));
                    fs::write(&path, &bytes).unwrap();
                    path
                })
                .collect::<Vec<_>>();
            let digests = paths
                .iter()
                .map(|path| (path.clone(), compute_content_digest(bytes.as_bytes())))
                .collect::<HashMap<_, _>>();
            for candidates_per_file in [1, 3] {
                let memo = ExactMemoStore::new();
                let key = MemoKey::new(
                    temp.path(),
                    GenerationToken::new_with_str("ready"),
                    "symbol",
                    true,
                );
                let results = paths
                    .iter()
                    .flat_map(|path| {
                        (0..candidates_per_file).map(move |ordinal| {
                            CandidateResult::new_exact(
                                path.clone(),
                                Some(SymbolOffsetRange::new(ordinal, ordinal + 1)),
                                EvidenceDescriptor::for_e1(1, true, false),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                memo.get_or_verify(&key, 0, usize::MAX, || {
                    Ok(VerifiedExactSet {
                        results,
                        file_digests: digests.clone(),
                        bound_disclosure: None,
                        stability_void: false,
                    })
                })
                .unwrap();
                let mut times = Vec::new();
                for _ in 0..30 {
                    reset();
                    let start = std::time::Instant::now();
                    let hit = memo
                        .get_or_verify(&key, 0, usize::MAX, || panic!("warm hit reverified"))
                        .unwrap();
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                    assert_eq!(hit.results.len(), file_count * candidates_per_file as usize);
                    assert!(!hit.stability_void);
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "WARM_EXACT_MEMO {}",
                    serde_json::json!({
                        "files": file_count, "candidates_per_file": candidates_per_file,
                        "bytes_per_file": bytes.len(), "samples": times.len(),
                    "p50_ms": (times[14] + times[15]) / 2.0, "p95_ms": times[28],
                    "repeat_exec_ms": times,
                    "verifier_calls": memo.verifier_call_count(), "work": counts(),
                    })
                );
            }
        }
    }

    #[test]
    fn exact_memo_hit_clones_only_the_served_page() {
        let memo = ExactMemoStore::new();
        let key = MemoKey::new(
            "/project",
            GenerationToken::new_with_str("generation"),
            "symbol",
            false,
        );
        let results = (0..8_000)
            .map(|ordinal| {
                CandidateResult::new_exact(
                    PathBuf::from(format!("/project/src/file_{ordinal}.rs")),
                    Some(SymbolOffsetRange::new(ordinal, ordinal + 1)),
                    EvidenceDescriptor::for_e1(1, true, false),
                )
            })
            .collect::<Vec<_>>();
        // Digests are exercised by the lifecycle integration suite. This fixture
        // isolates payload copying from filesystem allocations on a memo hit.
        let expected = results[100..110].to_vec();
        memo.get_or_verify(&key, 0, 10, || {
            Ok(VerifiedExactSet {
                results,
                file_digests: HashMap::new(),
                bound_disclosure: None,
                stability_void: false,
            })
        })
        .unwrap();
        let (outcome, allocations) = crate::test_allocations::count(|| {
            memo.get_or_verify(&key, 100, 10, || panic!("a hit must not reverify"))
                .unwrap()
        });
        assert_eq!(outcome.results, expected);
        assert_eq!(memo.verifier_call_count(), 1);
        let (full, full_allocations) = crate::test_allocations::count(|| {
            memo.get_or_verify(&key, 0, usize::MAX, || panic!("a hit must not reverify"))
                .unwrap()
        });
        assert_eq!(full.results.len(), 8_000);
        assert_eq!(full.results[100..110], expected);
        println!("SEMANTIC_WORK memo: entries=8000 page=10 allocations={allocations} full_page_allocations={full_allocations}");
        assert!(
            allocations < 100,
            "memo hit copied the full payload: {allocations} allocations"
        );
        assert!(
            full_allocations < 8_100,
            "full memo payload was copied twice: {full_allocations} allocations"
        );
    }

    #[test]
    fn exact_memo_digest_work_covers_every_served_file_once() {
        let temp = tempfile::tempdir().unwrap();
        let paths = (0..200)
            .map(|ordinal| {
                let path = temp.path().join(format!("file_{ordinal}.rs"));
                fs::write(&path, b"fn symbol() {}\n").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let file_digests = paths
            .iter()
            .map(|path| (path.clone(), compute_file_content_digest(path).unwrap()))
            .collect();
        let candidates = (0..8_000)
            .map(|ordinal| {
                CandidateResult::new_exact(
                    paths[ordinal % 200].clone(),
                    Some(SymbolOffsetRange::new(ordinal, ordinal + 1)),
                    EvidenceDescriptor::for_e1(1, true, false),
                )
            })
            .collect::<Vec<_>>();
        for (page, expected_reads) in [(&candidates[100..110], 10), (candidates.as_slice(), 200)] {
            crate::search_hot_path_measurements::reset();
            ExactMemoStore::check_served_page_digests(page, &file_digests).unwrap();
            let reads = crate::search_hot_path_measurements::counts().file_reads;
            println!(
                "SEMANTIC_WORK memo_digests: page={} files=200 reads_and_hashes={reads}",
                page.len()
            );
            assert_eq!(reads, expected_reads);
        }
        fs::write(&paths[0], b"changed").unwrap();
        assert_eq!(
            ExactMemoStore::check_served_page_digests(&candidates, &file_digests),
            Err(paths[0].clone())
        );
    }

    #[test]
    fn exact_memo_resets_before_exceeding_its_entry_bound() {
        let memo = ExactMemoStore::new();
        for index in 0..=MAX_EXACT_MEMO_ENTRIES {
            let key = MemoKey::new(
                "/project",
                GenerationToken::new_with_str("generation"),
                &format!("query-{index}"),
                false,
            );
            memo.get_or_verify(&key, 0, 1, || {
                Ok(VerifiedExactSet {
                    results: Vec::new(),
                    file_digests: HashMap::new(),
                    bound_disclosure: None,
                    stability_void: false,
                })
            })
            .expect("memoize exact result");
        }

        assert!(memo.entry_count() <= MAX_EXACT_MEMO_ENTRIES);
    }
    #[test]
    fn exact_memo_releases_old_generations_of_the_same_query() {
        let memo = ExactMemoStore::new();
        for revision in 0..32 {
            let key = MemoKey::new(
                "/project",
                GenerationToken::new_with_str(&format!("generation-{revision}")),
                "same query",
                false,
            );
            let results = vec![CandidateResult::new_exact(
                PathBuf::from("/project/file.rs"),
                None,
                EvidenceDescriptor::for_e1(revision + 1, true, false),
            )];
            let expected = results.clone();
            let page = memo
                .get_or_verify(&key, 0, 10, || {
                    Ok(VerifiedExactSet {
                        results,
                        file_digests: HashMap::new(),
                        bound_disclosure: None,
                        stability_void: false,
                    })
                })
                .unwrap();
            assert_eq!(page.results, expected);
        }
        assert_eq!(
            memo.entry_count(),
            1,
            "old index generations must not retain historical candidate sets"
        );
    }

    #[test]
    fn exact_memo_oversized_entry_is_served_in_full_but_not_retained() {
        let memo = ExactMemoStore::new();
        let key = audit01_key("oversized");
        let mut results =
            Vec::with_capacity(MAX_EXACT_MEMO_BYTES / std::mem::size_of::<CandidateResult>() + 1);
        for n in 0..20 {
            results.push(CandidateResult::new_exact(
                PathBuf::from(format!("/project/{n}.rs")),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
            ));
        }
        let expected = results.clone();
        let page = memo
            .get_or_verify(&key, 0, usize::MAX, || {
                Ok(VerifiedExactSet {
                    results,
                    file_digests: HashMap::new(),
                    bound_disclosure: None,
                    stability_void: false,
                })
            })
            .unwrap();
        assert_eq!(
            page.results, expected,
            "a cache budget must never cut the exact ranking input"
        );
        assert!(
            !memo.has_live_entry(&key),
            "oversized payload remains retained"
        );
    }

    #[test]
    fn exact_memo_payload_budget_counts_capacity_not_just_result_count() {
        let memo = ExactMemoStore::new();
        for query in 0..7 {
            let key = audit01_key(&format!("different-{query}"));
            let mut results =
                Vec::with_capacity(2 * 1024 * 1024 / std::mem::size_of::<CandidateResult>());
            results.push(CandidateResult::new_exact(
                PathBuf::from("/project/file.rs"),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
            ));
            let page = memo
                .get_or_verify(&key, 0, 10, || {
                    Ok(VerifiedExactSet {
                        results,
                        file_digests: HashMap::new(),
                        bound_disclosure: None,
                        stability_void: false,
                    })
                })
                .unwrap();
            assert_eq!(page.results.len(), 1);
        }
        assert!(memo.entry_count() <= 3, "seven two-MiB backing vectors exceed the cache budget despite containing only seven results");
        assert!(memo.estimated_memory().estimated_bytes.unwrap() < MAX_EXACT_MEMO_BYTES as u64);
        assert!(memo.try_clear_idle());
        assert_eq!(memo.estimated_memory().estimated_bytes, Some(0));
    }

    #[test]
    fn exact_memo_literal_presence_has_a_byte_budget() {
        let memo = ExactMemoStore::new();
        let key = (
            PathBuf::from("/project"),
            GenerationToken::new_with_str("generation"),
            "literal".into(),
        );
        memo.store_literal_presence(
            key.clone(),
            LiteralPresenceEntry {
                presence: LiteralPresence::Absent,
                read: Vec::new(),
                candidates: Vec::with_capacity(
                    MAX_LITERAL_PRESENCE_BYTES / std::mem::size_of::<PathBuf>() + 1,
                ),
            },
        );
        assert!(memo.literal_presence(&key).is_none());
        assert_eq!(memo.estimated_memory().estimated_bytes, Some(0));
    }
}
