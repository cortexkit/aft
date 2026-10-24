use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::commands::semantic_search::comparator::{CandidateResult, SymbolOffsetRange};
use crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use crate::commands::semantic_search::generation_token::GenerationToken;
use crate::search_index::ExactEvidenceScope;

const MAX_EXACT_MEMO_ENTRIES: usize = 128;
const MAX_RETIRED_POISONED_KEYS: usize = 128;

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
        Self {
            results,
            file_digests,
            descriptors,
            bound_disclosure,
            stability_void,
            epoch,
            poisoned,
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
        }
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

    fn make_room_for(&self, key: &MemoKey) {
        let mut entries = self.entries.write();
        if entries.len() < MAX_EXACT_MEMO_ENTRIES || entries.contains_key(key) {
            return;
        }
        let victim = entries
            .keys()
            .next()
            .cloned()
            .expect("full memo has a payload");
        let entry = entries.remove(&victim).expect("selected payload exists");
        if entry.poisoned {
            let mut lifecycle = self.lifecycle.write();
            let state = lifecycle.entry(victim).or_default();
            state.poisoned = true;
            state.retired_at = None;
        }
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
        for candidate in page {
            let Some(expected_digest) = file_digests.get(&candidate.path) else {
                continue;
            };
            #[cfg(test)]
            crate::search_hot_path_measurements::record_file_read();
            let live_bytes = fs::read(&candidate.path).map_err(|_| candidate.path.clone())?;
            let live_digest = compute_content_digest(&live_bytes);
            if &live_digest != expected_digest {
                return Err(candidate.path.clone());
            }
        }
        Ok(())
    }

    /// Serve the requested page from a cached exact set, or run its verifier.
    ///
    /// One verifier is active per key/epoch; failed attempts remain retryable.
    /// A digest mismatch retires only the observed payload. Detached old pages
    /// report void stability without invalidating a newer replacement payload.
    /// Poison survives rebuilds while the key is cached or among the last 128
    /// retired poisons, because pages across changed contents are not one stable set.
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
        let existing_entry = self.entries.read().get(key).cloned();

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
                Self::check_served_page_digests(page, &entry.file_digests)
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
        let mut in_flight = self.in_flight.lock();
        if self.entries.read().contains_key(key) {
            drop(in_flight);
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
            return self.get_or_verify(key, offset, top_k, verifier);
        }
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
            let admission = self.in_flight.lock();
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
            self.make_room_for(key);
            self.entries.write().insert(key.clone(), entry.clone());
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
        let _admission = self.in_flight.lock();
        self.entries.write().clear();
        self.lifecycle.write().clear();
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
    fn exact_memo_digest_work_still_covers_every_served_candidate() {
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
        for page in [&candidates[100..110], candidates.as_slice()] {
            crate::search_hot_path_measurements::reset();
            ExactMemoStore::check_served_page_digests(page, &file_digests).unwrap();
            let reads = crate::search_hot_path_measurements::counts().file_reads;
            println!(
                "SEMANTIC_WORK memo_digests: page={} files=200 reads_and_hashes={reads}",
                page.len()
            );
            assert_eq!(reads, page.len());
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
}
