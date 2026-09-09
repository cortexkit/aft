use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::commands::semantic_search::comparator::{CandidateResult, SymbolOffsetRange};
use crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use crate::commands::semantic_search::generation_token::GenerationToken;

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

/// Memo key K = (project_root, snapshot_generation, normalized_query, include_tests).
///
/// Per spec:
/// Equality is the only permitted operation on snapshot_generation.
/// Neither epoch nor poisoned is part of this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoKey {
    pub project_root: PathBuf,
    pub snapshot_generation: GenerationToken,
    pub normalized_query: String,
    pub include_tests: bool,
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
        }
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
    current_epoch: usize,
    poisoned: bool,
    verified_epochs: HashSet<usize>,
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
    entries: RwLock<HashMap<MemoKey, MemoEntry>>,
    lifecycle: RwLock<HashMap<MemoKey, KeyLifecycleState>>,
    pub verifier_counter: Arc<AtomicUsize>,
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
            let live_bytes = fs::read(&candidate.path).map_err(|_| candidate.path.clone())?;
            let live_digest = compute_content_digest(&live_bytes);
            if &live_digest != expected_digest {
                return Err(candidate.path.clone());
            }
        }
        Ok(())
    }

    /// Serve a requested [offset, offset + top_k) interval from memo or run verifier.
    ///
    /// Lifecycle invariants:
    /// - At most one verification per (K, epoch).
    /// - Served-page digest mismatch: prints "content changed - page stability void",
    ///   sets stability_void: true, drops the entry and poisons K without re-verifying.
    /// - Rebuild at epoch 1: preserves the poisoned flag (does NOT clear poison on rebuild).
    /// - Poisoned key is served with stability_void: true and disclosure.
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
                self.entries.write().remove(key);
                {
                    let mut lifecycle = self.lifecycle.write();
                    let state = lifecycle.entry(key.clone()).or_default();
                    state.poisoned = true;
                    state.current_epoch += 1;
                }

                return Ok(ServeOutcome {
                    results: page.to_vec(),
                    bound_disclosure: entry.bound_disclosure,
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
                bound_disclosure: entry.bound_disclosure,
                void_disclosure,
                stability_void: entry.stability_void || entry.poisoned,
                served_page_digest_mismatch: false,
            });
        }

        // 2. Cache miss: determine epoch and poison state from lifecycle
        let (epoch, is_poisoned) = {
            let mut lifecycle = self.lifecycle.write();
            let state = lifecycle.entry(key.clone()).or_default();
            let ep = state.current_epoch;
            let poisoned = state.poisoned;
            if state.verified_epochs.contains(&ep) {
                return Err(MemoError::ReverificationRejected(ep));
            }
            state.verified_epochs.insert(ep);
            (ep, poisoned)
        };

        // Run verifier (increments instrumented counter)
        self.verifier_counter.fetch_add(1, Ordering::SeqCst);
        let verified_set = verifier()?;

        let entry = MemoEntry::new(
            verified_set.results,
            verified_set.file_digests,
            verified_set.bound_disclosure,
            verified_set.stability_void,
            epoch,
            is_poisoned,
        );

        // Store into entries
        self.entries.write().insert(key.clone(), entry.clone());

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
            bound_disclosure: entry.bound_disclosure,
            void_disclosure,
            stability_void: entry.stability_void || is_poisoned,
            served_page_digest_mismatch: false,
        })
    }

    /// Drop all entries and reset state.
    pub fn clear(&self) {
        self.entries.write().clear();
        self.lifecycle.write().clear();
    }
}
