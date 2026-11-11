//! Shared file-search helpers and the bounded unpublished-pattern corpus.
//!
//! Published roots are queried through the data layer's index store. The mutable
//! trigram corpus remains only for explicit patterns against an unpublished
//! external repository; grep/glob also reuse the snapshot's file scanner over
//! candidates selected by the store. Unbounded index construction is test-only.

// Only the debug-build trigram counter and test hooks use `Cell`.
#[cfg(any(debug_assertions, test))]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, RwLock, RwLockReadGuard, TryLockError,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use regex::bytes::Regex;
use regex_syntax::hir::{Hir, HirKind};

use crate::cache_freshness;
use crate::commands::semantic_search::request_files::{
    Fetched, FileStat, RequestFile, RequestFiles,
};
use crate::pattern_compile::{self, CompileOpts, CompileResult, CompiledPattern, LiteralSearch};
use crate::root_identity::key::{artifact_cache_key_for_build, canonicalize_or_normalize};

pub use crate::commands::semantic_search::{exact_lane, memo};

#[derive(Debug, Clone, PartialEq)]
pub struct ExactPassMatch {
    pub path: PathBuf,
    pub symbol_range: Option<crate::commands::semantic_search::comparator::SymbolOffsetRange>,
    pub evidence: crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor,
    pub content_digest: String,
}

/// Which evidence a caller keeps from an exact pass.
///
/// The pass reads every file that could hold kept evidence, so the scope sets
/// how much of the corpus it reads. A declaration named after a query word can
/// sit in any file that mentions that word, while a verbatim phrase or an
/// all-words window needs every one of its words in the file. A caller that
/// drops declaration evidence therefore lets the pass skip the files that only
/// mention one or two query words, which for a prose query is most of the
/// corpus. Files that are read are verified exactly as under [`Self::All`], so
/// the evidence a caller keeps is the same under any scope that keeps it.
///
/// Defined here rather than in the exact lane module because that module is
/// compiled at two paths, and both must name the same type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExactEvidenceScope {
    /// Declarations named after a query word, verbatim phrase evidence (E1)
    /// and all-words window evidence (E2).
    #[default]
    All,
    /// Phrase and window evidence; the caller drops declaration evidence.
    PhraseAndWindow,
    /// Phrase evidence only; the caller drops window and declaration evidence.
    Phrase,
    /// Phrase and window evidence, plus declaration evidence only where the
    /// declaration's text contains one of these words, which the caller takes
    /// from the identifier-shaped words of the query (`snake_case`,
    /// `camelCase`, `a::b`).
    DefinitionsMentioning(Vec<String>),
}

pub(crate) const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;
const EOF_SENTINEL: u8 = 0;
#[cfg(debug_assertions)]
thread_local! {
    static POSTINGS_FOR_TRIGRAM_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_postings_for_trigram_count_for_debug() {
    POSTINGS_FOR_TRIGRAM_CALLS.with(|calls| calls.set(0));
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn postings_for_trigram_count_for_debug() -> usize {
    POSTINGS_FOR_TRIGRAM_CALLS.with(Cell::get)
}

pub(crate) const INTERACTIVE_ARTIFACT_READ_BUDGET: Duration = Duration::from_millis(250);
const BORROWED_INDEX_CHECKPOINT_INTERVAL: usize = 64;

/// Read an artifact pointer without allowing a writer to strand an interactive request.
///
/// Index refreshes publish through `RwLock`s and can legitimately hold a write guard while
/// validating or replacing a large artifact. Search must treat that contention as temporary
/// unavailability and use its bounded fallback rather than waiting for transport timeout.
pub(crate) fn try_read_with_budget<T>(
    lock: &RwLock<T>,
    budget: Duration,
) -> Option<RwLockReadGuard<'_, T>> {
    let deadline = Instant::now() + budget;
    loop {
        match lock.try_read() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return None;
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(1)));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchIndex {
    delta: Arc<DeltaState>,
    // This reverse lookup is writer-only. Snapshots never read it, so exclusive
    // SearchIndex write access keeps it synchronized with the versioned postings.
    delta_file_trigrams: HashMap<u32, Vec<u32>>,
    // Removal erases every posting for an ID before it can be reused;
    // snapshots retain the old tables through copy-on-write.
    free_delta_file_ids: Vec<u32>,
    pub files: Arc<Vec<FileEntry>>,
    pub path_to_id: Arc<HashMap<PathBuf, u32>>,
    pub ready: bool,
    project_root: PathBuf,
    max_file_size: u64,
    pub file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
}

// A query observes postings and file tables from one copy-on-write version.
#[derive(Clone, Debug, Default)]
struct DeltaState {
    postings: HashMap<u32, Vec<Posting>>,
}

#[derive(Clone, Debug)]
pub struct SearchIndexSnapshot {
    delta: Arc<DeltaState>,
    files: Arc<Vec<FileEntry>>,
    path_to_id: Arc<HashMap<PathBuf, u32>>,
    ready: bool,
    project_root: PathBuf,
    max_file_size: u64,
    file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
}

#[derive(Clone, Debug, Default)]
pub struct LexicalRankResult {
    pub files: Vec<(PathBuf, f32)>,
    pub engine_capped: bool,
}

impl SearchIndex {
    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of unique trigrams in the in-memory corpus.
    pub fn trigram_count(&self) -> usize {
        self.delta.postings.len()
    }

    /// Returns an immutable snapshot for queries. Callers must obtain the
    /// snapshot while holding the RwLock that protects the SearchIndex, then
    /// drop the guard before running expensive operations such as grep, glob, or
    /// lexical ranking.
    pub fn snapshot(&self) -> SearchIndexSnapshot {
        SearchIndexSnapshot {
            delta: Arc::clone(&self.delta),
            files: Arc::clone(&self.files),
            path_to_id: Arc::clone(&self.path_to_id),
            ready: self.ready,
            project_root: self.project_root.clone(),
            max_file_size: self.max_file_size,
            file_trigram_count: Arc::clone(&self.file_trigram_count),
            unindexed_files: Arc::clone(&self.unindexed_files),
        }
    }

    /// Compute distinct query trigrams from literal tokens.
    pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
        query_trigrams_from_tokens(tokens)
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Whole-corpus exact pass over the trigram index in ready mode.
    pub fn whole_corpus_exact_pass(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> std::io::Result<Vec<ExactPassMatch>> {
        self.snapshot()
            .whole_corpus_exact_pass(query, search_root, candidate_filter)
    }
}

impl SearchIndexSnapshot {
    /// The files this snapshot answers for, with the size and content hash it
    /// records for each and the modification time observed when it was built.
    #[cfg(test)]
    pub(crate) fn file_entries(&self) -> &[FileEntry] {
        &self.files
    }

    /// Reuses the query engine over already verified, in-memory posting rows.
    /// This is a read projection, not a mutable index: it has no cache file or
    /// persistence path. Missing postings require direct verification by grep.
    pub(crate) fn from_verified_rows(
        root: &Path,
        max_file_size: u64,
        rows: impl IntoIterator<Item = (FileEntry, Option<Vec<(u32, u8, u8)>>)>,
    ) -> Self {
        let mut files = Vec::new();
        let mut paths = HashMap::new();
        let mut counts = Vec::new();
        let mut unindexed = HashSet::new();
        let mut delta = DeltaState::default();
        for (file, records) in rows {
            let id = u32::try_from(files.len()).expect("view file id exceeds u32");
            paths.insert(file.path.clone(), id);
            files.push(file);
            counts.push(records.as_ref().map_or(0, |records| records.len() as u32));
            if let Some(records) = records {
                for (trigram, next_mask, loc_mask) in records {
                    delta.postings.entry(trigram).or_default().push(Posting {
                        file_id: id,
                        next_mask,
                        loc_mask,
                    });
                }
            } else {
                unindexed.insert(id);
            }
            crate::run_tool_call::progress::report();
        }
        Self {
            delta: Arc::new(delta),
            files: Arc::new(files),
            path_to_id: Arc::new(paths),
            ready: true,
            project_root: root.to_path_buf(),
            max_file_size,
            file_trigram_count: Arc::new(counts),
            unindexed_files: Arc::new(unindexed),
        }
    }

    pub(crate) fn file_modified(&self, path: &Path) -> Option<SystemTime> {
        self.path_to_id
            .get(path)
            .and_then(|id| self.files.get(*id as usize))
            .map(|file| file.modified)
    }

    /// Number of unique trigrams in the in-memory corpus.
    pub fn trigram_count(&self) -> usize {
        self.delta.postings.len()
    }

    /// How this snapshot holds `path`, for the benchmark-only search recall
    /// audit: `indexed` (its trigrams are searchable), `unindexed` (the file
    /// is known but its content was not indexed, for example because it is
    /// too large), or `absent`. The audit needs this to tell "never indexed"
    /// apart from "indexed but not retrieved".
    pub(crate) fn audit_index_state(&self, path: &Path) -> &'static str {
        match self.path_to_id.get(path) {
            Some(file_id) if !self.is_active_file(*file_id) => "absent",
            Some(file_id) if self.unindexed_files.contains(file_id) => "unindexed",
            Some(_) => "indexed",
            None => "absent",
        }
    }

    pub(crate) fn has_file_in_scope(&self, search_root: &Path) -> bool {
        let search_root = canonicalize_for_search_membership(search_root);
        self.files.iter().any(|file| {
            !file.path.as_os_str().is_empty() && is_within_search_root(&search_root, &file.path)
        })
    }

    /// Evaluates lexical candidates and scores them up to the specified depth limit, applying an optional candidate filter.
    pub fn lexical_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> std::io::Result<LexicalRankResult> {
        if query_trigrams.is_empty() || depth == 0 {
            return Ok(LexicalRankResult::default());
        }

        let mut non_zero: Vec<(u32, usize)> = query_trigrams
            .iter()
            .filter_map(|trigram| {
                let posting_count = self.posting_count(*trigram);
                (posting_count > 0).then_some((*trigram, posting_count))
            })
            .collect();
        if non_zero.is_empty() {
            return Ok(LexicalRankResult::default());
        }

        non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
        let postings_by_trigram = materialize_query_postings(self, query_trigrams)?;

        let initial_selected = non_zero.len().min(3);
        let mut candidate_ids = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(initial_selected) {
            if let Some(postings) = postings_by_trigram.get(trigram) {
                candidate_ids.extend(postings.iter().copied());
            }
        }
        if candidate_ids.len() < depth {
            for (trigram, _) in non_zero.iter().skip(initial_selected) {
                if candidate_ids.len() >= depth {
                    break;
                }
                if let Some(postings) = postings_by_trigram.get(trigram) {
                    candidate_ids.extend(postings.iter().copied());
                }
            }
        }

        let candidate_cap = depth;
        let pre_filter_candidate_count = candidate_ids.len();
        let engine_capped = pre_filter_candidate_count > candidate_cap;

        let filtered_candidates = candidate_ids
            .into_iter()
            .filter_map(|file_id| {
                self.files
                    .get(file_id as usize)
                    .map(|entry| (file_id, entry))
            })
            .filter(|(_, entry)| {
                if let Some(filter) = candidate_filter {
                    filter(&entry.path)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        let mut ranked = Vec::new();
        for (file_id, entry) in filtered_candidates.into_iter().take(candidate_cap) {
            let score =
                lexical_score_from_postings(self, query_trigrams, &postings_by_trigram, file_id);
            if score > 0.0 {
                ranked.push((entry.path.clone(), score));
            }
        }

        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.candidates_sorted += ranked.len()
        });
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(depth);
        Ok(LexicalRankResult {
            files: ranked,
            engine_capped,
        })
    }

    /// Score the union of the three rarest nonempty postings without ranking
    /// each posting merely to discover its membership. Trigram-value ties and
    /// candidate filtering match the canonical lexical lane's discovery rules.
    pub(crate) fn lexical_selected_pool(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> std::io::Result<Vec<(PathBuf, f32)>> {
        let mut seen = HashSet::with_capacity(query_trigrams.len());
        let unique = query_trigrams
            .iter()
            .copied()
            .filter(|trigram| seen.insert(*trigram))
            .collect::<Vec<_>>();
        let postings = materialize_query_postings(self, &unique)?;
        let mut memberships = postings
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .collect::<Vec<_>>();
        memberships.sort_unstable_by_key(|(trigram, ids)| (ids.len(), **trigram));
        let selected = memberships
            .iter()
            .take(3)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect::<BTreeSet<_>>();
        let mut ranked = selected
            .into_iter()
            .filter_map(|id| {
                let entry = self.files.get(id as usize)?;
                if candidate_filter.is_some_and(|include| !include(&entry.path)) {
                    return None;
                }
                let score = lexical_score_from_postings(self, &unique, &postings, id);
                (score > 0.0).then(|| (entry.path.clone(), score))
            })
            .collect::<Vec<_>>();
        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.candidates_sorted += ranked.len()
        });
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(ranked)
    }

    /// Indexed files under `search_root` that may hold `phrase`, compared
    /// ASCII case-insensitively with runs of whitespace treated alike. A file
    /// holding the phrase holds each of its whitespace-separated pieces, so
    /// the trigram postings of those pieces bound the set; no file is read.
    /// `None` when no piece is long enough to have a trigram, so any file
    /// could hold it.
    pub fn files_possibly_holding_phrase(
        &self,
        phrase: &str,
        search_root: &Path,
    ) -> std::io::Result<Option<Vec<PathBuf>>> {
        let mut ids: Option<Vec<u32>> = None;
        for piece in phrase.split_whitespace().filter(|piece| piece.len() >= 3) {
            let piece_ids = self.candidates(&decompose_regex(&regex::escape(piece)))?;
            ids = Some(match ids {
                Some(ids) => intersect_sorted_ids(&ids, &piece_ids),
                None => piece_ids,
            });
        }
        let search_root_canon = canonicalize_for_search_membership(search_root);
        let Some(ids) = ids else { return Ok(None) };
        Ok(Some(
            ids.into_iter()
                .filter_map(|id| self.files.get(id as usize))
                .map(|file| &file.path)
                .filter(|path| !path.as_os_str().is_empty())
                .filter(|path| {
                    is_within_search_root(search_root, path)
                        || is_within_search_root(&search_root_canon, path)
                })
                .cloned()
                .collect(),
        ))
    }

    /// Whole-corpus exact pass over the trigram index in ready mode.
    /// Intersects trigrams for the verbatim phrase (E1) and for content tokens within a 3-line window (E2),
    /// verifies matches against file content, and records content digests.
    pub fn whole_corpus_exact_pass(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> std::io::Result<Vec<ExactPassMatch>> {
        self.whole_corpus_exact_pass_scoped(
            query,
            search_root,
            candidate_filter,
            &ExactEvidenceScope::All,
        )
    }

    /// [`Self::whole_corpus_exact_pass`] that skips the files unable to hold
    /// any evidence `scope` keeps. Every remaining file is verified as before,
    /// so the kept evidence is identical to the unscoped pass.
    pub fn whole_corpus_exact_pass_scoped(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        scope: &ExactEvidenceScope,
    ) -> std::io::Result<Vec<ExactPassMatch>> {
        #[cfg(test)]
        let measured_start = Instant::now();
        let phrase = exact_lane::exact_phrase(query);
        let norm_phrase = exact_lane::normalize_exact_phrase(phrase);
        let content_tokens = exact_lane::exact_verification_tokens(query);
        let literal = crate::search_b2::router::is_hyphenated_literal(query);
        let mut literal_postings = HashMap::new();
        let member = exact_lane::qualified_member_name(query);

        let mut candidate_file_ids = BTreeSet::new();

        // (a) Verbatim phrase trigrams
        if phrase.len() >= 3 {
            let phrase_query = decompose_regex(&regex::escape(phrase));
            candidate_file_ids.extend(self.candidates(&phrase_query)?);
        }

        // (b) Content tokens trigrams (for E2 window match)
        if content_tokens.len() >= 2 {
            for token in &content_tokens {
                if token.len() >= 3 {
                    match self.files_possibly_containing(token, &mut literal_postings)? {
                        Some(ids) => candidate_file_ids.extend(ids.iter().copied()),
                        None => candidate_file_ids.extend(self.active_file_ids()),
                    }
                }
            }
        }

        // (c) A `receiver.member` query: the member's declaration does not
        // contain the receiver, so files holding the member name are
        // candidates too. Verification gives them definition evidence only
        // when they declare it.
        let member_file_ids = if let Some(member) = member.as_ref() {
            let member_query = decompose_regex(&regex::escape(&member.member));
            Some(
                self.candidates(&member_query)?
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            )
        } else {
            None
        };
        if let Some(ids) = &member_file_ids {
            candidate_file_ids.extend(ids.iter().copied());
        }

        // If no candidate trigrams (e.g. short query), check active files. A
        // hyphenated literal can only match through its own phrase trigrams,
        // so an empty candidate set already proves no file contains it.
        if candidate_file_ids.is_empty()
            && (phrase.len() < 3 || (content_tokens.is_empty() && !literal))
        {
            candidate_file_ids.extend(self.active_file_ids());
        }

        let unscoped_candidates = candidate_file_ids.len();
        if let Some(bound) =
            self.exact_evidence_bound(scope, &norm_phrase, &content_tokens, &mut literal_postings)?
        {
            // The evidence bound is built from the query's own words, which a
            // member's declaration does not contain, so member files stay.
            candidate_file_ids.retain(|file_id| {
                bound.binary_search(file_id).is_ok()
                    || member_file_ids
                        .as_ref()
                        .is_some_and(|ids| ids.contains(file_id))
            });
        }

        let started = Instant::now();
        let candidate_count = candidate_file_ids.len();
        let mut files_read = 0usize;
        let mut bytes_read = 0usize;
        let mut files_hashed = 0usize;
        let mut matches = Vec::new();
        let search_root_canon = canonicalize_for_search_membership(search_root);
        let request_files = crate::commands::semantic_search::request_files::current();

        for file_id in candidate_file_ids {
            let Some(file_entry) = self.files.get(file_id as usize) else {
                continue;
            };
            if file_entry.path.as_os_str().is_empty() {
                continue;
            }

            // Canonicalizing costs a filesystem call per file, so it is only
            // done when the stored path alone does not place the file in scope.
            let in_scope = is_within_search_root(search_root, &file_entry.path)
                || is_within_search_root(&search_root_canon, &file_entry.path)
                || is_within_search_root(
                    &search_root_canon,
                    &canonicalize_for_search_membership(&file_entry.path),
                );
            if !in_scope {
                continue;
            }

            if let Some(filter) = candidate_filter {
                if !filter(&file_entry.path) {
                    continue;
                }
            }

            files_read += 1;
            let RequestCorpusFile::Eligible {
                file,
                generated,
                fresh,
            } = read_request_corpus_file(
                request_files.as_deref(),
                &file_entry.path,
                self.max_file_size,
            )
            else {
                continue;
            };
            bytes_read += file.bytes().len();
            let text = file.lossy_text();
            let digest = file.digest();
            files_hashed += 1;

            let verified = exact_lane::verify_cached_exact_matches_in_text_with_member(
                &file_entry.path,
                text,
                digest,
                &norm_phrase,
                &content_tokens,
                member.as_ref().map(|member| member.member.as_str()),
            );
            if verified.is_none() && fresh {
                // Read only for this check, and it found nothing: later
                // phases are not expected to want the file.
                if let Some(files) = &request_files {
                    files.release(&file_entry.path);
                }
            }
            if let Some(candidates) = verified {
                // The same digest addresses prepared text and backs memo checks.
                let digest = digest.to_hex().to_string();
                for mut cand in candidates {
                    cand.evidence.generated = generated;
                    matches.push(ExactPassMatch {
                        path: cand.path,
                        symbol_range: cand.symbol_range,
                        evidence: cand.evidence,
                        content_digest: digest.clone(),
                    });
                }
            }
        }

        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.exact_passes += 1;
            counts.exact_pass_candidates += candidate_count;
            counts.exact_pass_reads += files_read;
            counts.exact_pass_micros += measured_start.elapsed().as_micros() as usize;
        });
        crate::slog_debug!(
            "exact pass: {} of {} candidate files kept for {:?}, {} read, {} bytes, {} hashed, {} matches in {:?}",
            candidate_count,
            unscoped_candidates,
            scope,
            files_read,
            bytes_read,
            files_hashed,
            matches.len(),
            started.elapsed()
        );
        Ok(matches)
    }

    fn exact_evidence_bound(
        &self,
        scope: &ExactEvidenceScope,
        norm_phrase: &str,
        content_tokens: &[String],
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> std::io::Result<Option<Vec<u32>>> {
        let (keeps_window, declaration_mentions) = match scope {
            ExactEvidenceScope::All => return Ok(None),
            ExactEvidenceScope::Phrase => (false, None),
            ExactEvidenceScope::PhraseAndWindow => (true, None),
            ExactEvidenceScope::DefinitionsMentioning(tokens) => (true, Some(tokens.as_slice())),
        };
        let mut bound = Vec::new();
        if !norm_phrase.is_empty() {
            let Some(phrase_files) =
                self.files_possibly_containing_all(norm_phrase.split(' '), literal_postings)?
            else {
                return Ok(None);
            };
            bound = union_sorted_ids(&bound, &phrase_files);
        }
        if keeps_window && content_tokens.len() >= 2 {
            let Some(window_files) = self.files_possibly_containing_all(
                content_tokens.iter().map(String::as_str),
                literal_postings,
            )?
            else {
                return Ok(None);
            };
            bound = union_sorted_ids(&bound, &window_files);
        }
        if let Some(mentions) = declaration_mentions {
            if !content_tokens.is_empty() && !mentions.is_empty() {
                let named = self.files_possibly_containing_any(
                    content_tokens.iter().map(String::as_str),
                    literal_postings,
                )?;
                let mentioning = self.files_possibly_containing_any(
                    mentions.iter().map(String::as_str),
                    literal_postings,
                )?;
                let declaration_files = match (named, mentioning) {
                    (Some(named), Some(mentioning)) => intersect_sorted_ids(&named, &mentioning),
                    (Some(files), None) | (None, Some(files)) => files,
                    (None, None) => return Ok(None),
                };
                bound = union_sorted_ids(&bound, &declaration_files);
            }
        }
        Ok(Some(bound))
    }

    fn files_possibly_containing_all<'a>(
        &self,
        words: impl Iterator<Item = &'a str>,
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> std::io::Result<Option<Vec<u32>>> {
        let mut files: Option<Vec<u32>> = None;
        for word in words {
            let Some(word_files) = self.files_possibly_containing(word, literal_postings)? else {
                continue;
            };
            files = Some(match files {
                Some(files) => intersect_sorted_ids(&files, word_files),
                None => word_files.clone(),
            });
            if files.as_ref().is_some_and(Vec::is_empty) {
                break;
            }
        }
        Ok(files)
    }

    fn files_possibly_containing_any<'a>(
        &self,
        words: impl Iterator<Item = &'a str>,
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> std::io::Result<Option<Vec<u32>>> {
        let mut files = Vec::new();
        for word in words {
            let Some(word_files) = self.files_possibly_containing(word, literal_postings)? else {
                return Ok(None);
            };
            files = union_sorted_ids(&files, word_files);
        }
        Ok(Some(files))
    }

    fn files_possibly_containing<'a>(
        &self,
        literal: &str,
        literal_postings: &'a mut HashMap<String, Option<Vec<u32>>>,
    ) -> std::io::Result<Option<&'a Vec<u32>>> {
        if !literal_postings.contains_key(literal) {
            let query = decompose_regex(&regex::escape(literal));
            let postings = if !query.and_trigrams.is_empty() || !query.or_groups.is_empty() {
                Some(self.candidates(&query)?)
            } else {
                None
            };
            literal_postings.insert(literal.to_string(), postings);
        }
        Ok(literal_postings.get(literal).and_then(Option::as_ref))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file_id: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub content_hash: blake3::Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
    pub match_text: String,
}

#[derive(Clone, Debug)]
pub struct GrepResult {
    /// Index sources whose posting evidence could not be read. Successful
    /// sources may still supply matches, but their merged reply is incomplete.
    pub(crate) index_sources_unavailable: bool,
    pub index_read_gaps: Vec<String>,
    pub matches: Vec<GrepMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub files_with_matches: usize,
    pub index_status: IndexStatus,
    pub truncated: bool,
    pub fully_degraded: bool,
    pub engine_capped: bool,
    /// True when a fallback directory walk stopped early due to file-count or time budget.
    pub walk_truncated: bool,
    /// Foreign filesystem mounts skipped by a fallback walk before they were opened.
    pub skipped_foreign_mounts: usize,
    /// Indexed candidate files that were not on disk when grep went to read
    /// them: stale index entries. They can never produce a match; the count
    /// only discloses that the index listed them.
    pub missing_on_disk: usize,
    /// True when the scan stopped at its wall-clock budget before every
    /// candidate file was searched, so matches may be missing.
    pub scan_deadline_reached: bool,
    /// Files read straight from disk rather than through the trigram index:
    /// the files a fallback walk searched, or a file named as the search path.
    pub files_read_directly: usize,
    /// Why a fallback walk stopped before reaching every file, when it did.
    /// `None` for an index answer and for a walk that covered every file.
    pub walk_bound: Option<WalkBound>,
}

/// The limit that stopped a fallback directory walk early. Files the walk
/// never reached were not searched, so a missing match is not evidence of
/// absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkBound {
    /// The walk's wall-clock budget ran out.
    TimeBudget,
    /// The walk found more eligible files than it may search; the value is
    /// that file limit.
    FileLimit(usize),
    /// The request was cancelled while the walk was running.
    Cancelled,
}

/// Bounds on how much work [`SearchIndexSnapshot::collect_grep_matches_by_file`]
/// may do. They limit the files examined and the time spent, never the number
/// of matches found, so the caller can rank every file that was examined.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GrepExaminationLimits {
    /// Most candidate files read and verified.
    pub max_files: usize,
    /// Wall-clock time after which no further file is started.
    pub budget: Duration,
    /// Most matching lines kept per file, apart from lines the caller asks to
    /// keep. Every matching line is still counted.
    pub max_lines_per_file: usize,
}

/// Every matching line found in one examined file.
#[derive(Clone, Debug)]
pub(crate) struct GrepFileMatches {
    pub path: PathBuf,
    pub modified: SystemTime,
    /// The first `max_lines_per_file` matching lines, and any later line the
    /// caller asked to keep, in file order.
    pub matches: Vec<GrepMatch>,
    /// All matching lines in the file, including the ones not kept.
    pub matched_lines: usize,
}

/// Result of [`SearchIndexSnapshot::collect_grep_matches_by_file`].
#[derive(Clone, Debug)]
pub(crate) struct GrepFileCollection {
    /// Files with at least one match, in examination order.
    pub files: Vec<GrepFileMatches>,
    /// Candidate files the trigram index admitted inside the search scope.
    pub candidate_files: usize,
    /// Candidate files actually read and verified.
    pub files_examined: usize,
    /// True when the file-count bound, the time budget or a cancellation left
    /// candidate files unexamined.
    pub examination_capped: bool,
    /// True when the pattern yielded no trigram, so every indexed file was a
    /// candidate.
    pub fully_degraded: bool,
    pub index_status: IndexStatus,
    pub missing_on_disk: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GrepQueryPhaseTimings {
    pub trigram_lookup: Duration,
    pub pread_verify: Duration,
    pub post_filter: Duration,
    pub candidate_count: usize,
    pub bytes_verified: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexStatus {
    Ready,
    Building,
    Fallback,
    Disabled,
}

impl IndexStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IndexStatus::Ready => "Ready",
            IndexStatus::Building => "Building",
            IndexStatus::Fallback => "Fallback",
            IndexStatus::Disabled => "Disabled",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RegexQuery {
    pub and_trigrams: Vec<u32>,
    pub or_groups: Vec<Vec<u32>>,
    pub(crate) and_filters: HashMap<u32, PostingFilter>,
    pub(crate) or_filters: Vec<HashMap<u32, PostingFilter>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PostingFilter {
    next_mask: u8,
    loc_mask: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct SearchFileMetadata {
    size: u64,
    modified: SystemTime,
}

struct PreparedIndexedFile {
    metadata: SearchFileMetadata,
    content_hash: blake3::Hash,
    trigram_map: BTreeMap<u32, PostingFilter>,
}

pub(crate) struct SearchCorpusFile {
    metadata: SearchFileMetadata,
    pub(crate) bytes: Vec<u8>,
    pub(crate) generated: bool,
}

pub(crate) enum SearchCorpusEligibility {
    Eligible(SearchCorpusFile),
    Unindexed(SearchFileMetadata),
    Skipped,
}

enum PreparedSearchPath {
    Indexed(PreparedIndexedFile),
    Unindexed(SearchFileMetadata),
    Skipped,
}

/// Prepared outside the index write lock. Applying this value only changes RAM
/// tables; path resolution, metadata, bytes, hashing and trigram extraction have
/// already finished.
pub(crate) struct PreparedSearchUpdate {
    path: PathBuf,
    file: PreparedSearchPath,
}

#[derive(Clone, Debug, Default)]
struct QueryBuild {
    and_runs: Vec<Vec<u8>>,
    or_groups: Vec<Vec<Vec<u8>>>,
}

pub type GrepPathExclusion = fn(&Path, &Path) -> bool;

/// Which matching lines a grep keeps once it passes its result cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrepScanOrder {
    /// Files are searched in parallel and the first lines found are kept:
    /// fast, but which lines are kept past the cap depends on scheduling.
    #[default]
    Arrival,
    /// Files are sorted by path and searched in batches of
    /// [`PATH_ORDER_BATCH_FILES`], in parallel within a batch. The scan stops
    /// after the first batch that brings the kept lines to the cap, and the
    /// kept lines are the first ones by path, then line. Whole batches finish
    /// before the cut, so the same lines are kept whatever order files finish
    /// in; only a scan stopped early (its deadline, cancellation, or a
    /// caller's file limit) can keep different lines.
    Path,
}

/// Files per batch of a path-ordered grep scan. Each batch ends in a barrier
/// where the scan checks the cap, so a batch should hold many files per
/// worker thread (one slow file then idles the others for only a small part
/// of the batch) while a scan past the cap reads at most one batch of files
/// it does not need. 256 is the batch the fallback walk already hands to its
/// parallel search: 16 or more files per thread on machines with up to 16
/// threads.
pub(crate) const PATH_ORDER_BATCH_FILES: usize = 256;

/// Rows the files of one path-ordered batch may hold together, on top of the
/// lines already kept. Every file of a batch may keep at most its share of
/// this budget (and never more than the cap still needs), so a batch of files
/// that each match on most of their lines cannot hold hundreds of times the
/// rows the cap keeps before the batch is cut. A file that has more matching
/// lines than its share, at a point where the cap still needs more than that
/// share from it, is searched again on its own for exactly the lines still
/// needed; the kept lines are the same as if every file had kept all the
/// cap needs. A share is never below one line, so a budget smaller than the
/// batch can be passed by up to one row per file.
///
/// 1,048,576 rows is 4,096 lines per file in a full batch of 256. Measured
/// on release builds: a capped grep of 256 files of 100,000 matching lines
/// peaks at about 460 MB (walk) and 200 MB (index) with this budget, against
/// 8.7 GB and 3.0 GB when every file kept all the cap needs, and it is no
/// slower on real projects, where only the few files matching on more than
/// 4,096 lines are ever searched twice. Keeping batches 256 files wide (rather
/// than narrowing them while the cap still needs many lines) keeps a scan of
/// many files with few matches each as fast as before.
pub(crate) const PATH_ORDER_BATCH_ROW_BUDGET: usize = 1 << 20;

/// Most lines one file of a path-ordered batch of `batch_files` files keeps:
/// its share of `row_budget` (at least one line), and never more than the
/// `remaining` lines the cap still needs.
pub(crate) fn path_order_file_allowance(
    remaining: usize,
    batch_files: usize,
    row_budget: usize,
) -> usize {
    (row_budget / batch_files.max(1)).max(1).min(remaining)
}

/// Adds one path-ordered batch to `kept`, which holds the lines kept from
/// earlier batches, until `kept` holds `max_results` lines.
///
/// `per_file` holds, for each file of the batch in path order, the rows it
/// kept under `allowance` and whether it has matching lines past them. When
/// such a file is reached while the cap needs more lines from it than it
/// kept, `search_again(file, need)` searches it on its own for at most
/// `need` lines and returns the same pair. `held(rows)` is told how many rows
/// are held at once (kept plus not yet taken) whenever that number grows.
///
/// Returns whether matching lines exist past the ones kept from this batch.
pub(crate) fn keep_path_ordered_batch<T>(
    kept: &mut Vec<T>,
    max_results: usize,
    allowance: usize,
    per_file: Vec<(Vec<T>, bool)>,
    mut search_again: impl FnMut(usize, usize) -> (Vec<T>, bool),
    mut held: impl FnMut(usize),
) -> bool {
    let mut not_taken: usize = per_file.iter().map(|(rows, _)| rows.len()).sum();
    held(kept.len() + not_taken);
    let mut lines_past = false;
    for (file, (rows, more)) in per_file.into_iter().enumerate() {
        not_taken -= rows.len();
        let need = max_results.saturating_sub(kept.len());
        if need == 0 {
            lines_past |= more || !rows.is_empty();
            continue;
        }
        let (rows, more) = if more && rows.len() == allowance && allowance < need {
            // The file stopped at its share of the batch budget, and the cap
            // needs more of its lines than that.
            drop(rows);
            let again = search_again(file, need);
            held(kept.len() + not_taken + again.0.len());
            again
        } else {
            (rows, more)
        };
        lines_past |= more || rows.len() > need;
        kept.extend(rows.into_iter().take(need));
    }
    lines_past
}

/// Records the most rows a path-ordered scan of a search root held at once,
/// so tests can prove the scan's memory stays within its row budget
/// whatever the files hold. Outside tests it does nothing.
pub(crate) fn note_path_order_held_rows(search_root: &Path, rows: usize) {
    #[cfg(test)]
    path_order_held_rows_for_test::note(search_root, rows);
    #[cfg(not(test))]
    let _ = (search_root, rows);
}

/// Test-only record of the most rows a path-ordered scan held at once, by
/// search root.
#[cfg(test)]
pub(crate) mod path_order_held_rows_for_test {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    static PEAKS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

    pub(crate) fn note(root: &Path, rows: usize) {
        if let Ok(mut peaks) = PEAKS.get_or_init(Default::default).lock() {
            let peak = peaks.entry(root.to_path_buf()).or_default();
            *peak = (*peak).max(rows);
        }
    }

    /// The most rows held at once since the last call, and resets it.
    pub(crate) fn take(root: &Path) -> usize {
        PEAKS
            .get_or_init(Default::default)
            .lock()
            .ok()
            .and_then(|mut peaks| peaks.remove(root))
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PathFilters {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScope {
    pub root: PathBuf,
    pub use_index: bool,
}

#[derive(Clone, Debug)]
struct SharedGrepMatch {
    file: Arc<PathBuf>,
    line: u32,
    column: u32,
    line_text: String,
    match_text: String,
}

#[derive(Clone, Debug)]
enum SearchMatcher {
    Literal(LiteralSearch),
    Regex(Regex),
}

impl SearchIndex {
    pub fn new() -> Self {
        SearchIndex {
            delta: Arc::new(DeltaState::default()),
            delta_file_trigrams: HashMap::new(),
            free_delta_file_ids: Vec::new(),
            files: Arc::new(Vec::new()),
            path_to_id: Arc::new(HashMap::new()),
            ready: false,
            project_root: PathBuf::new(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            file_trigram_count: Arc::new(Vec::new()),
            unindexed_files: Arc::new(HashSet::new()),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn build(root: &Path) -> Self {
        Self::build_with_limit(root, DEFAULT_MAX_FILE_SIZE)
    }

    /// An empty in-memory corpus to populate through a bounded disk walk.
    /// Marking it ready prevents query lanes from starting an unbounded build;
    /// the caller must disclose the walk's coverage separately.
    pub(crate) fn empty_ready_for_root(root: &Path) -> Self {
        Self {
            project_root: root.to_path_buf(),
            ready: true,
            ..Self::new()
        }
    }

    /// Builds a fixture for tests of the bounded external-pattern engine.
    /// Production queries may only populate a corpus through its bounded walk.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn build_with_limit(root: &Path, max_file_size: u64) -> Self {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut index = Self::empty_ready_for_root(&root);
        index.max_file_size = max_file_size;
        for entry in project_walk_builder(&root).build().flatten() {
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                index.update_file(entry.path());
            }
        }
        index
    }

    pub fn index_file(&mut self, path: &Path, content: &[u8]) {
        self.remove_file(path);
        let metadata = metadata_for_indexed_content(path, content.len() as u64);
        self.index_file_with_metadata(path, content, metadata);
    }

    fn index_file_with_metadata(
        &mut self,
        path: &Path,
        content: &[u8],
        metadata: SearchFileMetadata,
    ) -> bool {
        self.index_prepared_new_file(
            path,
            PreparedIndexedFile {
                metadata,
                content_hash: cache_freshness::hash_bytes(content),
                trigram_map: trigram_filter_map(content, true),
            },
        )
    }

    fn index_prepared_new_file(&mut self, path: &Path, file: PreparedIndexedFile) -> bool {
        let file_id = match self.allocate_file_id_with_metadata(path, file.metadata) {
            Some(file_id) => file_id,
            None => return false,
        };
        if let Some(entry) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            entry.content_hash = file.content_hash;
        }

        let mut file_trigrams = Vec::with_capacity(file.trigram_map.len());
        let delta = Arc::make_mut(&mut self.delta);
        for (trigram, filter) in file.trigram_map {
            let postings = delta.postings.entry(trigram).or_default();
            insert_delta_posting(
                postings,
                Posting {
                    file_id,
                    next_mask: filter.next_mask,
                    loc_mask: filter.loc_mask,
                },
            );
            file_trigrams.push(trigram);
        }

        let trigram_count = file_trigrams.len() as u32;
        self.delta_file_trigrams.insert(file_id, file_trigrams);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = trigram_count;
        }
        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        true
    }

    pub fn remove_file(&mut self, path: &Path) {
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        self.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
    }

    fn remove_file_with_canonicalization_memo(
        &mut self,
        path: &Path,
        canonical_parents: &mut ParentCanonicalizationMemo,
    ) {
        let canonical_path = self.respell_under_stored_root(
            canonicalize_existing_or_deleted_path_with_memo(path, canonical_parents),
        );
        self.remove_resolved_file(path, &canonical_path);
    }

    /// Spell a filesystem-canonical path the way this index's stored keys are
    /// spelled, so an exact key lookup can find it.
    ///
    /// On Windows, `fs::canonicalize` returns a verbatim (`\\?\`) path, but an
    /// index loaded by a read-only caller keeps a plain root and plain keys.
    /// Only the lookup key is respelled here; the stored root and the keys it
    /// owns keep the caller's convention. Elsewhere canonical paths already
    /// match the stored keys, so the path is returned unchanged.
    fn respell_under_stored_root(&self, path: PathBuf) -> PathBuf {
        #[cfg(windows)]
        if !self.project_root.as_os_str().is_empty() {
            let root = crate::windows_path::normalize_windows_path(&self.project_root);
            let normalized = crate::windows_path::normalize_windows_path(&path);
            if let Ok(relative) = normalized.strip_prefix(&root) {
                return self.project_root.join(relative);
            }
        }
        path
    }

    fn remove_resolved_file(&mut self, path: &Path, canonical_path: &Path) {
        let file_id = {
            let path_to_id = Arc::make_mut(&mut self.path_to_id);
            if let Some(file_id) = path_to_id.remove(path) {
                file_id
            } else if canonical_path != path {
                let Some(file_id) = path_to_id.remove(canonical_path) else {
                    return;
                };
                file_id
            } else {
                return;
            }
        };

        if let Some(trigrams) = self.delta_file_trigrams.remove(&file_id) {
            let delta = Arc::make_mut(&mut self.delta);
            for trigram in trigrams {
                let should_remove = if let Some(postings) = delta.postings.get_mut(&trigram) {
                    postings.retain(|posting| posting.file_id != file_id);
                    postings.is_empty()
                } else {
                    false
                };

                if should_remove {
                    delta.postings.remove(&trigram);
                }
            }
        }

        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        if let Some(file) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            file.path = PathBuf::new();
            file.size = 0;
            file.modified = UNIX_EPOCH;
            file.content_hash = cache_freshness::zero_hash();
        }
        self.free_delta_file_ids.push(file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
    }

    pub fn update_file(&mut self, path: &Path) {
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        self.update_file_with_canonicalization_memo(path, &mut canonical_parents);
    }

    fn update_file_with_canonicalization_memo(
        &mut self,
        path: &Path,
        canonical_parents: &mut ParentCanonicalizationMemo,
    ) {
        // Watcher paths may use an alias of the configured root (for example,
        // macOS /var versus /private/var). Keep the replacement under the same
        // canonical key as the initial corpus build so scoped queries can see it.
        let canonical_path =
            canonicalize_existing_or_deleted_path_with_memo(path, canonical_parents);
        let file = prepare_search_path(&canonical_path, self.max_file_size);
        self.apply_search_update(PreparedSearchUpdate {
            path: canonical_path,
            file,
        });
    }

    /// Whether `path` belongs to this index's corpus: it lies under the
    /// index root with a safe relative path. An index without a root
    /// admits every path.
    pub(crate) fn admits_path(&self, path: &Path) -> bool {
        self.project_root.as_os_str().is_empty()
            || storable_relative_path(&self.project_root, path).is_some()
    }

    pub(crate) fn apply_search_update(&mut self, update: PreparedSearchUpdate) {
        let PreparedSearchUpdate { path, file } = update;
        // Prepared updates use filesystem-canonical paths, but borrowed indexes
        // can have a plain Windows root. Match the stored root's spelling before
        // replacing its exact path key.
        let path = self.respell_under_stored_root(path);
        if !self.admits_path(&path) {
            // AFT's own writes report every path they touched, and a session
            // may edit files in a sibling checkout or a temporary directory.
            // Such a file is not part of this root's corpus, and one stored
            // entry outside the root would let queries escape their corpus.
            // refuse the whole cache.
            return;
        }
        self.remove_resolved_file(&path, &path);
        match file {
            PreparedSearchPath::Indexed(file) => {
                self.index_prepared_new_file(&path, file);
            }
            PreparedSearchPath::Unindexed(metadata) => {
                self.track_unindexed_file_with_metadata(&path, metadata);
            }
            PreparedSearchPath::Skipped => {}
        }
    }

    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> std::io::Result<GrepResult> {
        self.snapshot().grep(
            pattern,
            case_sensitive,
            include,
            exclude,
            search_root,
            max_results,
        )
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> std::io::Result<GrepResult> {
        self.snapshot()
            .search_grep(pattern, include, exclude, search_root, max_results)
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        self.snapshot().glob(pattern, search_root)
    }

    pub fn candidates(&self, query: &RegexQuery) -> std::io::Result<Vec<u32>> {
        self.snapshot().candidates(query)
    }

    fn allocate_file_id_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> Option<u32> {
        let file_id = match self.free_delta_file_ids.pop() {
            Some(file_id) => file_id,
            None => u32::try_from(self.files.len()).ok()?,
        };
        let file = FileEntry {
            path: path.to_path_buf(),
            size: metadata.size,
            modified: metadata.modified,
            content_hash: cache_freshness::zero_hash(),
        };
        let files = Arc::make_mut(&mut self.files);
        if let Some(slot) = files.get_mut(file_id as usize) {
            *slot = file;
        } else {
            files.push(file);
        }
        Arc::make_mut(&mut self.path_to_id).insert(path.to_path_buf(), file_id);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        Some(file_id)
    }

    fn track_unindexed_file_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> bool {
        let Some(file_id) = self.allocate_file_id_with_metadata(path, metadata) else {
            return false;
        };
        Arc::make_mut(&mut self.unindexed_files).insert(file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
        true
    }

    #[cfg(test)]
    fn postings_for_trigram(
        &self,
        trigram: u32,
        filter: Option<PostingFilter>,
    ) -> std::io::Result<Vec<u32>> {
        self.snapshot().postings_for_trigram(trigram, filter)
    }
}

impl SearchIndexSnapshot {
    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> std::io::Result<GrepResult> {
        match pattern_compile::compile(
            pattern,
            CompileOpts {
                case_insensitive: !case_sensitive,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => {
                self.search_grep(&compiled, include, exclude, search_root, max_results)
            }
            CompileResult::InvalidPattern { .. } | CompileResult::UnsupportedSyntax { .. } => {
                Ok(self.empty_grep_result())
            }
        }
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> std::io::Result<GrepResult> {
        self.search_grep_profiled(pattern, include, exclude, search_root, max_results, None)
            .map(|result| result.0)
    }

    pub(crate) fn search_grep_profiled(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        let filters = build_path_filters(include, exclude).unwrap_or_default();
        self.search_grep_profiled_with_filters(
            pattern,
            &filters,
            search_root,
            max_results,
            path_exclusion,
        )
    }

    pub(crate) fn search_grep_profiled_with_filters(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        let query_started = Instant::now();
        let query = decompose_grep_pattern(pattern);
        let query_decomposition = query_started.elapsed();
        let (result, mut timings) = self.search_grep_profiled_with_filters_and_query(
            pattern,
            &query,
            filters,
            search_root,
            max_results,
            path_exclusion,
        )?;
        timings.trigram_lookup += query_decomposition;
        Ok((result, timings))
    }

    /// Search with a query decomposed once by the caller and reused across roots.
    pub(crate) fn search_grep_profiled_with_filters_and_query(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters_and_query_in_order(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            GrepScanOrder::Arrival,
        )
    }

    /// [`Self::search_grep_profiled_with_filters_and_query`] with a choice
    /// of which lines are kept past the result cap.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_grep_profiled_with_filters_and_query_in_order(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters_and_query_in_order_with_row_budget(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            order,
            PATH_ORDER_BATCH_ROW_BUDGET,
        )
    }

    /// [`Self::search_grep_profiled_with_filters_and_query_in_order`] with
    /// the rows a path-ordered batch may hold set by the caller, for callers
    /// that run several of these scans at once and share one budget.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_grep_profiled_with_filters_and_query_in_order_with_row_budget(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters_and_query_and_limits(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            None,
            INDEXED_GREP_SCAN_BUDGET,
            order,
            row_budget,
        )
    }

    /// Indexed grep over `search_root`. `max_files` bounds how many candidate
    /// files are read; `scan_budget` bounds the wall-clock time of the scan,
    /// checked before each file and at each match occurrence inside it. A scan
    /// that runs out of budget returns what it found with `engine_capped` and
    /// `scan_deadline_reached` set, never an empty success.
    fn search_grep_profiled_with_filters_and_query_and_limits(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        max_files: Option<usize>,
        scan_budget: Duration,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            max_files,
            scan_budget,
            || GrepScanDeadline::after_with_clock(scan_budget, Instant::now),
            order,
            row_budget,
        )
    }

    fn search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        max_files: Option<usize>,
        scan_budget: Duration,
        make_deadline: impl FnOnce() -> GrepScanDeadline,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        let _io_scope = crate::bounded_io::enter_if_absent(scan_budget);
        let matcher = match pattern {
            CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
            CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
        };

        let search_root = canonicalize_for_search_membership(search_root);

        let job_cancellation = crate::executor::current_job_cancellation();
        let trigram_started = Instant::now();
        let fully_degraded = query.and_trigrams.is_empty() && query.or_groups.is_empty();
        let candidate_ids = self.candidates(query)?;
        let trigram_lookup = trigram_started.elapsed();

        let candidate_filter_started = Instant::now();
        let mut candidate_files: Vec<&FileEntry> = candidate_ids
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| {
                path_exclusion.is_none_or(|exclude| !exclude(&file.path, &self.project_root))
            })
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .collect();
        // Tests hand the candidates over reversed to show a path-ordered
        // scan does not depend on the order the index lists them in.
        #[cfg(test)]
        if crate::grep_executor::scan_order_for_test::get(&search_root) == Some(true) {
            candidate_files.reverse();
        }
        if order == GrepScanOrder::Path {
            candidate_files.sort_by(|left, right| left.path.cmp(&right.path));
        }
        let candidate_count = candidate_files.len();
        let candidate_filter = candidate_filter_started.elapsed();

        let total_matches = AtomicUsize::new(0);
        let files_searched = AtomicUsize::new(0);
        let files_with_matches = AtomicUsize::new(0);
        let bytes_verified = AtomicUsize::new(0);
        let missing_on_disk = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let engine_capped = AtomicBool::new(false);
        let stop_after = max_results.saturating_mul(2);
        let stop_scan = Arc::new(AtomicBool::new(false));
        let deadline = make_deadline();
        let io_budget = crate::bounded_io::current();
        let alloc_label = crate::alloc_trace::carry();
        let floor_disclosure = crate::context::confinement::capture();
        let verification_claims = AtomicUsize::new(0);
        let claim_verification = || {
            let Some(max_files) = max_files else {
                return true;
            };
            verification_claims.fetch_add(1, Ordering::Relaxed) < max_files
        };

        let pread_started = Instant::now();
        let mut matches = if order == GrepScanOrder::Path {
            // Path-ordered batches: each file keeps at most its share of the
            // batch's row budget, and never more than the lines still missing
            // from the cap when its batch starts, which is all it can
            // contribute to the first lines by path. Rows are added in batch
            // order, so `kept` stays sorted by path, then line.
            let mut kept: Vec<SharedGrepMatch> = Vec::new();
            let batches = candidate_files.chunks(PATH_ORDER_BATCH_FILES);
            let batch_count = batches.len();
            for (batch_index, batch) in batches.enumerate() {
                if deadline.reached()
                    || job_cancellation
                        .as_ref()
                        .is_some_and(|token| token.cancel_requested_before_commit())
                {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
                let remaining = max_results.saturating_sub(kept.len());
                let allowance = path_order_file_allowance(remaining, batch.len(), row_budget);
                let per_file: Vec<(Vec<SharedGrepMatch>, bool)> = batch
                    .par_iter()
                    .map_init(
                        || {
                            (
                                crate::bounded_io::enter(io_budget.clone()),
                                alloc_label.enter(),
                            )
                        },
                        |_scope, file| {
                            if !claim_verification() {
                                truncated.store(true, Ordering::Relaxed);
                                engine_capped.store(true, Ordering::Relaxed);
                                return (Vec::new(), false);
                            }
                            let file_matches = AtomicUsize::new(0);
                            let file_truncated = AtomicBool::new(false);
                            let rows = search_candidate_file(
                                file,
                                &floor_disclosure,
                                &matcher,
                                allowance,
                                allowance,
                                &file_matches,
                                &files_searched,
                                &files_with_matches,
                                &bytes_verified,
                                &file_truncated,
                                &engine_capped,
                                &missing_on_disk,
                                None,
                                job_cancellation.as_ref(),
                                &deadline,
                            );
                            total_matches
                                .fetch_add(file_matches.load(Ordering::Relaxed), Ordering::Relaxed);
                            (rows, file_truncated.load(Ordering::Relaxed))
                        },
                    )
                    .collect();
                let search_again = |file: usize, need: usize| {
                    // The file was already counted as searched (and as
                    // matching) by the batch; only its match count changes.
                    let file_matches = AtomicUsize::new(0);
                    let file_truncated = AtomicBool::new(false);
                    let rows = search_candidate_file(
                        batch[file],
                        &floor_disclosure,
                        &matcher,
                        need,
                        need,
                        &file_matches,
                        &AtomicUsize::new(0),
                        &AtomicUsize::new(0),
                        &AtomicUsize::new(0),
                        &file_truncated,
                        &engine_capped,
                        &AtomicUsize::new(0),
                        None,
                        job_cancellation.as_ref(),
                        &deadline,
                    );
                    // The batch counted `allowance + 1` matches for it.
                    total_matches.fetch_add(
                        file_matches
                            .load(Ordering::Relaxed)
                            .saturating_sub(allowance + 1),
                        Ordering::Relaxed,
                    );
                    (rows, file_truncated.load(Ordering::Relaxed))
                };
                let lines_past = keep_path_ordered_batch(
                    &mut kept,
                    max_results,
                    allowance,
                    per_file,
                    search_again,
                    |rows| note_path_order_held_rows(&search_root, rows),
                );
                if lines_past {
                    truncated.store(true, Ordering::Relaxed);
                }
                if kept.len() >= max_results {
                    if batch_index + 1 < batch_count {
                        // Later files were not read.
                        truncated.store(true, Ordering::Relaxed);
                        engine_capped.store(true, Ordering::Relaxed);
                    }
                    break;
                }
            }
            kept
        } else if candidate_files.len() > 10 {
            candidate_files
                .par_iter()
                .map_init(
                    || {
                        (
                            crate::bounded_io::enter(io_budget.clone()),
                            alloc_label.enter(),
                        )
                    },
                    |_scope, file| {
                        if grep_scan_should_stop(
                            Some(&stop_scan),
                            &truncated,
                            &total_matches,
                            stop_after,
                            job_cancellation.as_ref(),
                            &deadline,
                        ) {
                            engine_capped.store(true, Ordering::Relaxed);
                            return Vec::new();
                        }
                        if !claim_verification() {
                            truncated.store(true, Ordering::Relaxed);
                            engine_capped.store(true, Ordering::Relaxed);
                            stop_scan.store(true, Ordering::Relaxed);
                            return Vec::new();
                        }
                        search_candidate_file(
                            file,
                            &floor_disclosure,
                            &matcher,
                            max_results,
                            stop_after,
                            &total_matches,
                            &files_searched,
                            &files_with_matches,
                            &bytes_verified,
                            &truncated,
                            &engine_capped,
                            &missing_on_disk,
                            Some(&stop_scan),
                            job_cancellation.as_ref(),
                            &deadline,
                        )
                    },
                )
                .reduce(Vec::new, |mut left, mut right| {
                    // When concatenating partial match lists from parallel file
                    // searches, simply append the chunks. The stop checks in
                    // each worker decide whether the result cap was reached.
                    left.append(&mut right);
                    left
                })
        } else {
            let mut matches = Vec::new();
            for file in candidate_files {
                if !claim_verification() {
                    truncated.store(true, Ordering::Relaxed);
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
                matches.extend(search_candidate_file(
                    file,
                    &floor_disclosure,
                    &matcher,
                    max_results,
                    stop_after,
                    &total_matches,
                    &files_searched,
                    &files_with_matches,
                    &bytes_verified,
                    &truncated,
                    &engine_capped,
                    &missing_on_disk,
                    None,
                    job_cancellation.as_ref(),
                    &deadline,
                ));

                if should_stop_search(&truncated, &total_matches, stop_after) || deadline.reached()
                {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
            }
            matches
        };
        let scan_deadline_reached = deadline.reached();
        if scan_deadline_reached && max_files.is_some() {
            // Bounded callers read `truncated` as "more may exist"; running out
            // of time leaves candidate files unread just as the file bound does.
            truncated.store(true, Ordering::Relaxed);
        }
        let pread_verify = pread_started.elapsed();

        let post_filter_started = Instant::now();
        sort_shared_grep_matches_by_cached_mtime_desc(&mut matches, &self.project_root, |path| {
            self.path_to_id
                .get(path)
                .and_then(|file_id| self.files.get(*file_id as usize))
                .map(|file| file.modified)
        });

        let matches = matches
            .into_iter()
            .map(|matched| GrepMatch {
                file: matched.file.as_ref().clone(),
                line: matched.line,
                column: matched.column,
                line_text: matched.line_text,
                match_text: matched.match_text,
            })
            .collect();

        let result = GrepResult {
            index_sources_unavailable: false,
            index_read_gaps: Vec::new(),
            total_matches: total_matches.load(Ordering::Relaxed),
            matches,
            files_searched: files_searched.load(Ordering::Relaxed),
            files_with_matches: files_with_matches.load(Ordering::Relaxed),
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: truncated.load(Ordering::Relaxed),
            fully_degraded,
            engine_capped: engine_capped.load(Ordering::Relaxed),
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: missing_on_disk.load(Ordering::Relaxed),
            scan_deadline_reached,
            files_read_directly: 0,
            walk_bound: None,
        };
        let post_filter = candidate_filter + post_filter_started.elapsed();
        let phases = GrepQueryPhaseTimings {
            trigram_lookup,
            pread_verify,
            post_filter,
            candidate_count,
            bytes_verified: bytes_verified.load(Ordering::Relaxed),
        };
        Ok((result, phases))
    }

    /// Find every matching line in the candidate files the trigram index admits,
    /// grouped by file, for a caller that ranks files before cutting a page.
    ///
    /// Candidates come from the whole index: the trigram prefilter, with one
    /// posting union per alternation branch, admits files; the pattern itself
    /// decides every match, so the result has no false matches. Unlike
    /// [`Self::search_grep`], nothing stops at a match count. The work is
    /// bounded instead by `limits`: candidates are sorted by
    /// `examine_priority` (lower first) and then by path, and at most
    /// `limits.max_files` of them are read, none started after the time
    /// budget. When a bound leaves files unexamined, `examination_capped`
    /// reports it, so the caller can say what its ranking covered.
    pub(crate) fn collect_grep_matches_by_file(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        path_exclusion: Option<GrepPathExclusion>,
        limits: GrepExaminationLimits,
        examine_priority: &(dyn Fn(&Path) -> u8 + Sync),
        keep_past_limit: &(dyn Fn(&GrepMatch) -> bool + Sync),
    ) -> std::io::Result<GrepFileCollection> {
        let matcher = match pattern {
            CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
            CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
        };
        let query = decompose_grep_pattern(pattern);
        let search_root = canonicalize_for_search_membership(search_root);
        let fully_degraded = query.and_trigrams.is_empty() && query.or_groups.is_empty();

        let mut candidates: Vec<(u8, &FileEntry)> = self
            .candidates(&query)?
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| {
                path_exclusion.is_none_or(|exclude| !exclude(&file.path, &self.project_root))
            })
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .map(|file| (examine_priority(&file.path), file))
            .collect();
        candidates.sort_by(|(left_priority, left), (right_priority, right)| {
            left_priority
                .cmp(right_priority)
                .then_with(|| left.path.cmp(&right.path))
        });
        let candidate_files = candidates.len();
        let examined_slice = &candidates[..candidate_files.min(limits.max_files)];

        let job_cancellation = crate::executor::current_job_cancellation();
        let started = Instant::now();
        let stopped_early = AtomicBool::new(false);
        let io_budget = crate::bounded_io::current();
        let alloc_label = crate::alloc_trace::carry();
        let files_examined = AtomicUsize::new(0);
        let missing_on_disk = AtomicUsize::new(0);
        // Rayon tasks run on other threads, where the request's file set is
        // not current: take it here and hand it to them.
        let request_files = crate::commands::semantic_search::request_files::current();
        let request_files = request_files.as_deref();
        let files: Vec<GrepFileMatches> = examined_slice
            .par_iter()
            .map_init(
                || {
                    (
                        crate::bounded_io::enter(io_budget.clone()),
                        alloc_label.enter(),
                    )
                },
                |_scope, (_, file)| {
                    if started.elapsed() >= limits.budget
                        || job_cancellation
                            .as_ref()
                            .is_some_and(|token| token.cancel_requested_before_commit())
                    {
                        stopped_early.store(true, Ordering::Relaxed);
                        return None;
                    }
                    let deadline = started.checked_add(limits.budget);
                    let fetched = crate::commands::semantic_search::request_files::read_through(
                        request_files,
                        &file.path,
                        |path| Ok((read_indexed_file_bytes(path, deadline)?, None)),
                    );
                    let fetched = match fetched {
                        Ok(fetched) => fetched,
                        Err(error) => {
                            if error.kind() == std::io::ErrorKind::TimedOut {
                                stopped_early.store(true, Ordering::Relaxed);
                            }
                            if io_error_means_missing_on_disk(&error) {
                                missing_on_disk.fetch_add(1, Ordering::Relaxed);
                            }
                            files_examined.fetch_add(1, Ordering::Relaxed);
                            return None;
                        }
                    };
                    files_examined.fetch_add(1, Ordering::Relaxed);
                    let content = fetched.file.bytes();
                    let release = || {
                        // Read only for this check, and it found nothing:
                        // later phases are not expected to want the file.
                        if let (Some(files), true) = (request_files, fetched.fresh) {
                            files.release(&file.path);
                        }
                    };
                    if is_binary_bytes(content) {
                        release();
                        return None;
                    }
                    let (matches, matched_lines) = matching_lines_in_content(
                        &file.path,
                        content,
                        &matcher,
                        limits.max_lines_per_file,
                        keep_past_limit,
                    );
                    if matched_lines == 0 {
                        release();
                    }
                    (matched_lines > 0).then(|| GrepFileMatches {
                        path: file.path.clone(),
                        modified: file.modified,
                        matches,
                        matched_lines,
                    })
                },
            )
            .filter_map(|file| file)
            .collect();

        Ok(GrepFileCollection {
            files,
            candidate_files,
            files_examined: files_examined.load(Ordering::Relaxed),
            examination_capped: candidate_files > examined_slice.len()
                || stopped_early.load(Ordering::Relaxed),
            fully_degraded,
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            missing_on_disk: missing_on_disk.load(Ordering::Relaxed),
        })
    }

    fn empty_grep_result(&self) -> GrepResult {
        GrepResult {
            index_sources_unavailable: false,
            index_read_gaps: Vec::new(),
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: false,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
        }
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        self.glob_profiled(pattern, search_root, true).0
    }

    pub(crate) fn glob_profiled(
        &self,
        pattern: &str,
        search_root: &Path,
        sort_by_mtime: bool,
    ) -> (Vec<PathBuf>, bool, usize) {
        let filters = match build_path_filters(&[pattern.to_string()], &[]) {
            Ok(filters) => filters,
            Err(_) => return (Vec::new(), false, 0),
        };
        // Indexed entries carry the project root's spelling as configured,
        // which on Windows can differ from the canonical form in ways
        // normalize_path does not reconcile (8.3 short names such as
        // RUNNER~1 against runneradmin). Containment is therefore checked
        // against both the root as passed and its canonical form, and the
        // pattern is matched relative to whichever root contained the entry.
        let raw_root = search_root.to_path_buf();
        let canonical_root = canonicalize_for_search_membership(search_root);
        let file_scope = search_root.is_file();
        let containing_root = |path: &Path| -> Option<&Path> {
            if is_within_search_root(&raw_root, path) {
                Some(raw_root.as_path())
            } else if is_within_search_root(&canonical_root, path) {
                Some(canonical_root.as_path())
            } else {
                None
            }
        };
        let entries_visited = self.files.len();
        let mut scope_has_files = false;
        let mut entries = self
            .files
            .iter()
            .inspect(|_| crate::run_tool_call::progress::report())
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter_map(|file| {
                let root = containing_root(&file.path)?;
                Some((file, root))
            })
            // Match the glob pattern relative to the search root, not the
            // project root. When a caller passes `{path: "src", pattern:
            // "a.rs"}`, the pattern must match `a.rs` (the path relative to
            // `src`), not `src/a.rs` (the path relative to the project root).
            .filter(|(file, root)| {
                // A file scope must match its basename, not the empty path
                // relative to itself. Containment above still excludes siblings.
                let filter_root = if file_scope {
                    root.parent().unwrap_or(root)
                } else {
                    root
                };
                filters.matches(filter_root, &file.path)
            })
            .map(|(file, _)| {
                scope_has_files = true;
                (file.path.clone(), file.modified)
            })
            .collect::<Vec<_>>();

        if sort_by_mtime {
            entries.sort_by(|(left_path, left_mtime), (right_path, right_mtime)| {
                right_mtime
                    .cmp(left_mtime)
                    .then_with(|| left_path.cmp(right_path))
            });
        }

        (
            entries.into_iter().map(|(path, _)| path).collect(),
            scope_has_files,
            entries_visited,
        )
    }

    pub fn candidates(&self, query: &RegexQuery) -> std::io::Result<Vec<u32>> {
        if query.and_trigrams.is_empty() && query.or_groups.is_empty() {
            return Ok(self.active_file_ids());
        }

        let mut and_trigrams = query.and_trigrams.clone();
        // Each count may read the base lookup table, so compute it once per
        // trigram rather than once per comparison.
        and_trigrams.sort_by_cached_key(|trigram| self.posting_count(*trigram));

        let mut current: Option<Vec<u32>> = None;

        for trigram in and_trigrams {
            let filter = query.and_filters.get(&trigram).copied();
            let matches = self.postings_for_trigram(trigram, filter)?;
            current = Some(match current.take() {
                Some(existing) => intersect_sorted_ids(&existing, &matches),
                None => matches,
            });

            if current.as_ref().is_some_and(|ids| ids.is_empty()) {
                break;
            }
        }

        let mut current = current.unwrap_or_else(|| self.active_file_ids());

        for (index, group) in query.or_groups.iter().enumerate() {
            let mut group_matches = Vec::new();
            let filters = query.or_filters.get(index);

            for trigram in group {
                let filter = filters.and_then(|filters| filters.get(trigram).copied());
                let matches = self.postings_for_trigram(*trigram, filter)?;
                if group_matches.is_empty() {
                    group_matches = matches;
                } else {
                    group_matches = union_sorted_ids(&group_matches, &matches);
                }
            }

            current = intersect_sorted_ids(&current, &group_matches);
            if current.is_empty() {
                break;
            }
        }

        let mut unindexed = self
            .unindexed_files
            .iter()
            .copied()
            .filter(|file_id| self.is_active_file(*file_id))
            .collect::<Vec<_>>();
        if !unindexed.is_empty() {
            unindexed.sort_unstable();
            current = union_sorted_ids(&current, &unindexed);
        }

        Ok(current)
    }

    fn posting_count(&self, trigram: u32) -> usize {
        #[cfg(test)]
        POSTING_COUNT_CALLS.with(|calls| calls.set(calls.get() + 1));
        self.delta.postings.get(&trigram).map_or(0usize, Vec::len)
    }

    fn active_file_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.path_to_id.values().copied().collect();
        ids.retain(|file_id| self.is_active_file(*file_id));
        ids.sort_unstable();
        ids
    }

    fn is_active_file(&self, file_id: u32) -> bool {
        #[cfg(test)]
        SUPERSEDED_LOOKUPS.with(|count| count.set(count.get() + 1));
        self.files
            .get(file_id as usize)
            .map(|file| !file.path.as_os_str().is_empty())
            .unwrap_or(false)
    }

    fn postings_for_trigram(
        &self,
        trigram: u32,
        filter: Option<PostingFilter>,
    ) -> std::io::Result<Vec<u32>> {
        #[cfg(debug_assertions)]
        POSTINGS_FOR_TRIGRAM_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
        let mut matches = Vec::new();
        if let Some(postings) = self.delta.postings.get(&trigram) {
            matches.reserve(postings.len());
            for (posting_index, posting) in postings.iter().enumerate() {
                if posting_index % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
                    && crate::executor::current_job_cancelled()
                {
                    break;
                }
                if !posting_matches_filter(posting, filter) {
                    continue;
                }
                if self.is_active_file(posting.file_id) {
                    matches.push(posting.file_id);
                }
            }
        }
        if matches.len() > 1 {
            matches.sort_unstable();
            matches.dedup();
        }
        Ok(matches)
    }
}

fn posting_matches_filter(posting: &Posting, filter: Option<PostingFilter>) -> bool {
    if let Some(filter) = filter {
        // next_mask is a bloom filter: the character following this trigram in
        // the query must also appear after this trigram somewhere in the file.
        if filter.next_mask != 0 && posting.next_mask & filter.next_mask == 0 {
            return false;
        }
        // loc_mask is persisted for future adjacency checks. It is intentionally
        // not used as a single-trigram filter because query positions do not
        // correspond to file positions.
    }
    true
}

/// What happened to one match occurrence handed to a candidate-file scan.
enum MatchVisit {
    /// The scan must end: a stop flag, the result cap, cancellation or the
    /// deadline.
    Stop,
    /// The occurrence is on a line that already produced a match.
    SameLine,
    /// The occurrence was the first on its line; skipping to the next line
    /// avoids searching the rest of that line again.
    Recorded { next_line_start: usize },
}

fn search_candidate_file(
    file: &FileEntry,
    floor_disclosure: &crate::context::confinement::Fence,
    matcher: &SearchMatcher,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    bytes_verified: &AtomicUsize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    missing_on_disk: &AtomicUsize,
    stop_scan: Option<&Arc<AtomicBool>>,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: &GrepScanDeadline,
) -> Vec<SharedGrepMatch> {
    if grep_scan_should_stop(
        stop_scan,
        truncated,
        total_matches,
        stop_after,
        job_cancellation,
        deadline,
    ) {
        engine_capped.store(true, Ordering::Relaxed);
        return Vec::new();
    }

    let content = match read_indexed_file_bytes(&file.path, deadline.read_at) {
        Ok(content) => content,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                floor_disclosure.record_matched_source_exclusion(&file.path);
            }
            if error.kind() == std::io::ErrorKind::TimedOut {
                deadline.reached.store(true, Ordering::Relaxed);
                engine_capped.store(true, Ordering::Relaxed);
            }
            if io_error_means_missing_on_disk(&error) {
                missing_on_disk.fetch_add(1, Ordering::Relaxed);
            }
            return Vec::new();
        }
    };
    bytes_verified.fetch_add(content.len(), Ordering::Relaxed);
    // Defense in depth: even though indexing tries to filter binaries via
    // The index classifies bytes at ingestion; we double-check at
    // query time. content_inspector is fast (~bytes-per-cycle on a small
    // preview) and this guarantees we never surface matches inside binary
    // files even if the indexer somehow let one through (e.g. file changed
    // between indexing and query).
    if is_binary_bytes(&content) {
        return Vec::new();
    }
    files_searched.fetch_add(1, Ordering::Relaxed);

    let shared_path = Arc::new(file.path.clone());
    let mut matches = Vec::new();
    let mut line_starts = None;
    // Grep reports one match per line: the first one. Occurrences arrive in
    // increasing offset order, so any occurrence before the start of the line
    // after the last reported one is on that same line. Checking that offset
    // first means a line with many occurrences (minified code, single-line
    // JSON, lockfiles) costs one line lookup and one line text in total, not
    // one per occurrence.
    let mut reported_line_end = 0usize;
    let mut matched_this_file = false;

    {
        let mut visit = |offset: usize, end: usize| -> MatchVisit {
            if grep_scan_should_stop(
                stop_scan,
                truncated,
                total_matches,
                stop_after,
                job_cancellation,
                deadline,
            ) {
                engine_capped.store(true, Ordering::Relaxed);
                return MatchVisit::Stop;
            }
            if offset < reported_line_end {
                return MatchVisit::SameLine;
            }

            let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
            let (line, column, line_text, next_line_start) =
                line_details_bytes(&content, line_starts, offset);
            reported_line_end = next_line_start;

            matched_this_file = true;
            let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
            if match_number > max_results {
                truncated.store(true, Ordering::Relaxed);
                signal_grep_scan_cap(stop_scan, total_matches, stop_after);
                return MatchVisit::Stop;
            }

            matches.push(SharedGrepMatch {
                file: shared_path.clone(),
                line,
                column,
                line_text,
                match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
            });
            MatchVisit::Recorded { next_line_start }
        };

        match matcher {
            SearchMatcher::Literal(literal) => {
                let needle = &literal.needle;
                if !needle.contains(&b'\n') {
                    let lowered;
                    let haystack: &[u8] = if literal.case_insensitive_ascii {
                        lowered = content.to_ascii_lowercase();
                        &lowered
                    } else {
                        &content
                    };
                    let finder = memchr::memmem::Finder::new(needle);
                    let mut start = 0;
                    while start <= haystack.len() {
                        let Some(position) = finder.find(&haystack[start..]) else {
                            break;
                        };
                        let offset = start + position;
                        start = match visit(offset, offset + needle.len()) {
                            MatchVisit::Stop => break,
                            MatchVisit::SameLine => offset + 1,
                            MatchVisit::Recorded { next_line_start } => {
                                next_line_start.max(offset + 1)
                            }
                        };
                    }
                }
            }
            SearchMatcher::Regex(regex) => {
                pattern_compile::for_each_line_match(regex, &content, |start, end| {
                    !matches!(visit(start, end), MatchVisit::Stop)
                });
            }
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

/// Every line of `content` the matcher matches, in file order: the first
/// `max_lines` of them, plus any later line `keep_past_limit` accepts, each
/// with its first match on the line, and the count of all of them. Match
/// positions and line numbering are the same as [`search_candidate_file`]
/// reports, so a line found here is a line grep finds.
fn matching_lines_in_content(
    path: &Path,
    content: &[u8],
    matcher: &SearchMatcher,
    max_lines: usize,
    keep_past_limit: &(dyn Fn(&GrepMatch) -> bool + Sync),
) -> (Vec<GrepMatch>, usize) {
    let mut matches = Vec::new();
    let mut matched_lines = 0usize;
    let mut line_starts: Option<Vec<usize>> = None;
    // Start of the line after the last recorded one. Offsets arrive in
    // increasing order, so an offset before it is on the line just recorded;
    // only that line's first match is kept, as grep does, and the repeat is
    // dropped before any line text is built for it.
    let mut reported_line_end = 0usize;
    let mut record = |start: usize, end: usize| -> Option<usize> {
        if start < reported_line_end {
            return None;
        }
        let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(content));
        let (line, column, line_text, next_line_start) =
            line_details_bytes(content, line_starts, start);
        reported_line_end = next_line_start;
        matched_lines += 1;
        let grep_match = GrepMatch {
            file: path.to_path_buf(),
            line,
            column,
            line_text,
            match_text: String::from_utf8_lossy(&content[start..end]).into_owned(),
        };
        if matched_lines <= max_lines || keep_past_limit(&grep_match) {
            matches.push(grep_match);
        }
        Some(next_line_start)
    };
    match matcher {
        SearchMatcher::Literal(literal) => {
            let needle = &literal.needle;
            if !needle.contains(&b'\n') {
                let lowered;
                let haystack: &[u8] = if literal.case_insensitive_ascii {
                    lowered = content.to_ascii_lowercase();
                    &lowered
                } else {
                    content
                };
                let finder = memchr::memmem::Finder::new(needle);
                let mut start = 0;
                while start <= haystack.len() {
                    let Some(position) = finder.find(&haystack[start..]) else {
                        break;
                    };
                    let offset = start + position;
                    start = record(offset, offset + needle.len())
                        .map_or(offset + 1, |next_line_start| {
                            next_line_start.max(offset + 1)
                        });
                }
            }
        }
        SearchMatcher::Regex(regex) => {
            pattern_compile::for_each_line_match(regex, content, |start, end| {
                record(start, end);
                true
            });
        }
    }
    (matches, matched_lines)
}

fn should_stop_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    truncated.load(Ordering::Relaxed) && total_matches.load(Ordering::Relaxed) >= stop_after
}

/// Wall-clock bound on one indexed grep scan whose caller sets none. A grep
/// holds its root's reader slot while it runs, and a configure for that root
/// waits for readers, so an unbounded scan locks every later bind on the root
/// out of its tools. It gets the same budget as the index-unavailable walk.
pub(crate) const INDEXED_GREP_SCAN_BUDGET: Duration = crate::grep_executor::FALLBACK_WALK_BUDGET;

/// The wall-clock point a grep scan must stop at, and whether the scan
/// stopped there. The scan loops check it per candidate file and per match
/// occurrence, so one huge file cannot carry a scan past it.
pub(crate) struct GrepScanDeadline {
    /// `None` when the budget is too large to represent as an instant, which
    /// leaves the scan unbounded in time.
    at: Option<Instant>,
    /// File reads use real time, even when a test injects a scan clock.
    read_at: Option<Instant>,
    now: fn() -> Instant,
    reached: AtomicBool,
}

impl GrepScanDeadline {
    fn after_with_clock(budget: Duration, now: fn() -> Instant) -> Self {
        let at = now()
            .checked_add(budget)
            .map(|at| crate::bounded_io::current().map_or(at, |budget| at.min(budget.deadline)));
        Self {
            at,
            read_at: at,
            now,
            reached: AtomicBool::new(false),
        }
    }

    /// True once the deadline has passed; records that the scan hit it.
    fn passed(&self) -> bool {
        if self.reached.load(Ordering::Relaxed) {
            return true;
        }
        if self.at.is_some_and(|at| (self.now)() >= at) {
            self.reached.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Whether a scan stopped because of this deadline.
    pub(crate) fn reached(&self) -> bool {
        self.reached.load(Ordering::Relaxed)
    }
}

fn grep_scan_should_stop(
    stop_scan: Option<&Arc<AtomicBool>>,
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: &GrepScanDeadline,
) -> bool {
    job_cancellation.is_some_and(|token| token.cancel_requested_before_commit())
        || stop_scan.is_some_and(|flag| flag.load(Ordering::Relaxed))
        || should_stop_search(truncated, total_matches, stop_after)
        || deadline.passed()
}

fn signal_grep_scan_cap(
    stop_scan: Option<&Arc<AtomicBool>>,
    total_matches: &AtomicUsize,
    stop_after: usize,
) {
    if let Some(flag) = stop_scan {
        if total_matches.load(Ordering::Relaxed) >= stop_after {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn search_file_metadata(metadata: &fs::Metadata) -> SearchFileMetadata {
    SearchFileMetadata {
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(UNIX_EPOCH),
    }
}

fn metadata_for_indexed_content(path: &Path, size_hint: u64) -> SearchFileMetadata {
    fs::metadata(path)
        .ok()
        .map(|metadata| search_file_metadata(&metadata))
        .unwrap_or(SearchFileMetadata {
            size: size_hint,
            modified: UNIX_EPOCH,
        })
}

fn prepare_search_path(path: &Path, max_file_size: u64) -> PreparedSearchPath {
    // The trigram index does not consume the generated flag. Other corpus
    // readers still request it, preserving their eligibility policy.
    match read_search_corpus_file_inner(path, max_file_size, false) {
        SearchCorpusEligibility::Eligible(file) => {
            PreparedSearchPath::Indexed(PreparedIndexedFile {
                metadata: file.metadata,
                content_hash: cache_freshness::hash_bytes(&file.bytes),
                trigram_map: trigram_filter_map(&file.bytes, true),
            })
        }
        SearchCorpusEligibility::Unindexed(metadata) => PreparedSearchPath::Unindexed(metadata),
        SearchCorpusEligibility::Skipped => PreparedSearchPath::Skipped,
    }
}

pub(crate) fn read_search_corpus_file(path: &Path, max_file_size: u64) -> SearchCorpusEligibility {
    read_search_corpus_file_inner(path, max_file_size, true)
}

/// The read error of a corpus file that held more than the size bound: the
/// file is unindexed, and its cut bytes are not kept.
#[derive(Debug)]
struct CutAtSizeBound;

impl std::fmt::Display for CutAtSizeBound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("file larger than the corpus size bound")
    }
}

impl std::error::Error for CutAtSizeBound {}

/// A corpus file one search request's exact pass may verify, read through
/// the request's file set.
pub(crate) enum RequestCorpusFile {
    Eligible {
        file: Arc<RequestFile>,
        generated: bool,
        /// The pass read the disk for it; see [`Fetched::fresh`].
        fresh: bool,
    },
    /// Too large or binary, as [`SearchCorpusEligibility::Unindexed`].
    Unindexed,
    Skipped,
}

/// [`read_search_corpus_file`] through a request's file set, with the same
/// admission: excluded paths and non-files are skipped; files over
/// `max_file_size` bytes, or binary, are unindexed. A file an earlier phase
/// of the request kept is judged from those bytes. Otherwise the file is
/// stat'ed and read as [`read_search_corpus_file`] reads it, and the set keeps
/// the bytes unless they were cut at the size bound.
pub(crate) fn read_request_corpus_file(
    files: Option<&RequestFiles>,
    path: &Path,
    max_file_size: u64,
) -> RequestCorpusFile {
    if crate::project_content::is_excluded(path) {
        return RequestCorpusFile::Skipped;
    }
    let Some(files) = files else {
        #[cfg(test)]
        crate::commands::semantic_search::request_files::record_disk_read(path);
        return match read_search_corpus_file(path, max_file_size) {
            SearchCorpusEligibility::Eligible(file) => RequestCorpusFile::Eligible {
                generated: file.generated,
                file: Arc::new(RequestFile::unshared(file.bytes, None)),
                fresh: true,
            },
            SearchCorpusEligibility::Unindexed(_) => RequestCorpusFile::Unindexed,
            SearchCorpusEligibility::Skipped => RequestCorpusFile::Skipped,
        };
    };
    let fetched = match files.kept(path) {
        Some(file) => Fetched { file, fresh: false },
        None => {
            let metadata = match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => return RequestCorpusFile::Skipped,
            };
            if metadata.len() > max_file_size {
                return RequestCorpusFile::Unindexed;
            }
            let read = files.read(path, |path| {
                #[cfg(test)]
                crate::search_hot_path_measurements::record_file_read_path(path);
                // Bound the read itself as well as admission: the file can
                // grow after stat. Bytes cut at the bound are not kept.
                let mut bytes = Vec::with_capacity(
                    usize::try_from(metadata.len().min(max_file_size)).unwrap_or(0),
                );
                crate::project_content::open(path)?
                    .take(max_file_size.saturating_add(1))
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > max_file_size {
                    return Err(std::io::Error::other(CutAtSizeBound));
                }
                Ok((bytes, Some(FileStat::of(&metadata))))
            });
            match read {
                Ok(fetched) => fetched,
                Err(error)
                    if error
                        .get_ref()
                        .is_some_and(|inner| inner.is::<CutAtSizeBound>()) =>
                {
                    return RequestCorpusFile::Unindexed
                }
                Err(_) => return RequestCorpusFile::Skipped,
            }
        }
    };
    let bytes = fetched.file.bytes();
    if bytes.len() as u64 > max_file_size || is_binary_bytes(bytes) {
        if fetched.fresh {
            files.release(path);
        }
        return RequestCorpusFile::Unindexed;
    }
    // As `crate::inspect::is_generated_file(path, path)`, from the bytes in
    // hand rather than a second read of the file's head.
    let generated = crate::file_kind::relative_shape_is_generated(path, path)
        || crate::file_kind::head_has_generated_marker(bytes);
    RequestCorpusFile::Eligible {
        file: fetched.file,
        generated,
        fresh: fetched.fresh,
    }
}

fn read_search_corpus_file_inner(
    path: &Path,
    max_file_size: u64,
    classify_generated: bool,
) -> SearchCorpusEligibility {
    if crate::project_content::is_excluded(path) {
        return SearchCorpusEligibility::Skipped;
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => search_file_metadata(&metadata),
        _ => return SearchCorpusEligibility::Skipped,
    };
    read_search_corpus_file_with_metadata(path, max_file_size, classify_generated, metadata)
}

fn read_search_corpus_file_with_metadata(
    path: &Path,
    max_file_size: u64,
    classify_generated: bool,
    metadata: SearchFileMetadata,
) -> SearchCorpusEligibility {
    #[cfg(test)]
    AUDIT_IO_PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            probe();
        }
    });
    if metadata.size > max_file_size {
        return SearchCorpusEligibility::Unindexed(metadata);
    }

    #[cfg(test)]
    crate::search_hot_path_measurements::record_file_read_path(path);
    // Bound the read itself as well as admission: the file can grow after stat.
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.size.min(max_file_size)).unwrap_or(0));
    #[cfg(test)]
    audit_record(|work| work.corpus_opens += 1);
    let read = crate::project_content::open(path).and_then(|file| {
        file.take(max_file_size.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    if read.is_err() {
        return SearchCorpusEligibility::Skipped;
    }
    if bytes.len() as u64 > max_file_size || is_binary_bytes(&bytes) {
        return SearchCorpusEligibility::Unindexed(metadata);
    }

    #[cfg(test)]
    if classify_generated {
        audit_record(|work| work.corpus_opens += 1);
    }
    SearchCorpusEligibility::Eligible(SearchCorpusFile {
        metadata,
        generated: classify_generated && crate::inspect::is_generated_file(path, path),
        bytes,
    })
}

fn ensure_count_slot(counts: &mut Vec<u32>, file_id: u32) {
    let len = file_id as usize + 1;
    if counts.len() < len {
        counts.resize(len, 0);
    }
}

/// Insert a delta posting without disturbing the sorted-id invariant required by
/// posting-list intersection. Git diff order is independent of file ID order, so
/// re-sorting the entire list after every inversion scales poorly for shared trigrams.
fn insert_delta_posting(postings: &mut Vec<Posting>, posting: Posting) {
    if postings
        .last()
        .is_none_or(|last| last.file_id < posting.file_id)
    {
        postings.push(posting);
        return;
    }

    let insertion_index = postings.partition_point(|existing| existing.file_id < posting.file_id);
    debug_assert!(postings
        .get(insertion_index)
        .is_none_or(|existing| existing.file_id != posting.file_id));
    postings.insert(insertion_index, posting);
}

#[cfg(test)]
fn insert_delta_posting_full_sort_reference(postings: &mut Vec<Posting>, posting: Posting) {
    postings.push(posting);
    if postings.len() > 1
        && postings[postings.len() - 2].file_id > postings[postings.len() - 1].file_id
    {
        postings.sort_unstable_by_key(|posting| posting.file_id);
    }
}

fn intersect_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        if (left_index + right_index) % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
            && crate::executor::current_job_cancelled()
        {
            break;
        }
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged
}

fn union_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        if (left_index + right_index) % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
            && crate::executor::current_job_cancelled()
        {
            break;
        }
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                merged.push(left[left_index]);
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right[right_index]);
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    if !crate::executor::current_job_cancelled() {
        merged.extend_from_slice(&left[left_index..]);
        merged.extend_from_slice(&right[right_index..]);
    }
    merged
}

pub(crate) fn decompose_grep_pattern(pattern: &CompiledPattern) -> RegexQuery {
    let raw_pattern = pattern.raw_pattern_for_trigrams();
    match pattern {
        CompiledPattern::Regex {
            case_insensitive: true,
            ..
        } => {
            // RegexBuilder applies this flag outside the raw pattern. Parse the
            // same effective regex so Unicode folds such as `K` matching `K` do
            // not become false mandatory trigrams.
            decompose_regex(&format!("(?i:{raw_pattern})"))
        }
        _ => decompose_regex(&raw_pattern),
    }
}

pub fn decompose_regex(pattern: &str) -> RegexQuery {
    let hir = match regex_syntax::parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return RegexQuery::default(),
    };

    let build = build_query(&hir);
    build.into_query()
}

pub fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | c as u32
}

pub fn normalize_char(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

fn scan_trigrams(content: &[u8], mut visit: impl FnMut(u32, u8, usize)) {
    if content.len() < 3 {
        return;
    }

    for start in 0..=content.len() - 3 {
        let trigram = pack_trigram(
            normalize_char(content[start]),
            normalize_char(content[start + 1]),
            normalize_char(content[start + 2]),
        );
        let next_char = content.get(start + 3).copied().unwrap_or(EOF_SENTINEL);
        visit(trigram, next_char, start);
    }
}

pub fn extract_trigrams(content: &[u8]) -> Vec<(u32, u8, usize)> {
    let mut trigrams = Vec::with_capacity(content.len().saturating_sub(2));
    scan_trigrams(content, |trigram, next_char, position| {
        trigrams.push((trigram, next_char, position));
    });
    trigrams
}

fn trigram_filter_map(content: &[u8], include_eof_next_char: bool) -> BTreeMap<u32, PostingFilter> {
    let mut filters: BTreeMap<u32, PostingFilter> = BTreeMap::new();
    scan_trigrams(content, |trigram, next_char, position| {
        let entry = filters.entry(trigram).or_default();
        if include_eof_next_char || next_char != EOF_SENTINEL {
            entry.next_mask |= mask_for_next_char(next_char);
        }
        entry.loc_mask |= mask_for_position(position);
    });
    filters
}

pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in tokens {
        scan_trigrams(token.as_bytes(), |trigram, _, _| {
            if seen.insert(trigram) {
                out.push(trigram);
            }
        });
    }
    out
}

pub fn lexical_score(
    index: &SearchIndex,
    query_trigrams: &[u32],
    file_id: u32,
) -> std::io::Result<f32> {
    lexical_score_snapshot(&index.snapshot(), query_trigrams, file_id)
}

fn materialize_query_postings(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
) -> std::io::Result<HashMap<u32, Vec<u32>>> {
    let mut postings_by_trigram = HashMap::with_capacity(query_trigrams.len());
    for &trigram in query_trigrams {
        if let std::collections::hash_map::Entry::Vacant(entry) = postings_by_trigram.entry(trigram)
        {
            entry.insert(index.postings_for_trigram(trigram, None)?);
        }
    }
    Ok(postings_by_trigram)
}

fn lexical_score_snapshot(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    file_id: u32,
) -> std::io::Result<f32> {
    let postings_by_trigram = materialize_query_postings(index, query_trigrams)?;
    Ok(lexical_score_from_postings(
        index,
        query_trigrams,
        &postings_by_trigram,
        file_id,
    ))
}

fn lexical_score_from_postings(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    postings_by_trigram: &HashMap<u32, Vec<u32>>,
    file_id: u32,
) -> f32 {
    #[cfg(test)]
    crate::search_hot_path_measurements::record(|counts| counts.score_evaluations += 1);
    if query_trigrams.is_empty() {
        return 0.0;
    }

    let mut hits = 0u32;
    for &trigram in query_trigrams {
        if postings_by_trigram
            .get(&trigram)
            .is_some_and(|postings| postings.binary_search(&file_id).is_ok())
        {
            hits += 1;
        }
    }

    if hits == 0 {
        return 0.0;
    }

    let file_trigram_count = index
        .file_trigram_count
        .get(file_id as usize)
        .copied()
        .unwrap_or(1)
        .max(1) as f32;
    (hits as f32) / (1.0 + file_trigram_count.ln())
}

/// Resolve a cache directory using configure's registered key when available.
/// Persistent writers should pass their chosen key to `resolve_cache_dir_with_key`
/// directly; callers without a configured project fall back to a Git probe.
pub fn resolve_cache_dir(project_root: &Path, storage_dir: Option<&Path>) -> PathBuf {
    resolve_cache_dir_with_key(&artifact_cache_key_for_build(project_root), storage_dir)
}

pub(crate) fn build_path_filters(
    include: &[String],
    exclude: &[String],
) -> Result<PathFilters, String> {
    Ok(PathFilters {
        includes: build_globset(include)?,
        excludes: build_globset(exclude)?,
    })
}

pub fn walk_project_files_bounded_default(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner(root, root, &PathFilters::default(), Some(max_files), true)
}

pub fn walk_project_files_bounded_default_matching<F>(
    root: &Path,
    max_files: usize,
    matches_file: F,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(
        root,
        root,
        &PathFilters::default(),
        Some(max_files),
        matches_file,
        true,
    )
}

// Only the ordering regression needs the unfiltered, mtime-sorted walk.
#[cfg(test)]
pub(crate) fn walk_project_files_from(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> Vec<PathBuf> {
    walk_project_files_from_inner(filter_root, search_root, filters, None, true)
        .expect("unbounded project walk cannot exceed a file limit")
}

/// Enumerate the same live tree and order as `walk_project_files_from`, but
/// retain only matching paths before building metadata and display sort keys.
pub(crate) fn walk_project_files_from_matching<F>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    matches_file: F,
) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(
        filter_root,
        search_root,
        filters,
        None,
        matches_file,
        true,
    )
    .expect("unbounded project walk cannot exceed a file limit")
}

fn walk_project_files_from_inner(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner_matching(
        filter_root,
        search_root,
        filters,
        max_files,
        |_| true,
        sort_by_mtime,
    )
}

pub(crate) fn project_walk_builder(search_root: &Path) -> WalkBuilder {
    project_walk_builder_with_admission(
        search_root,
        crate::project_content::is_excluded_with_file_type,
    )
}

fn project_walk_builder_with_admission(
    search_root: &Path,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
) -> WalkBuilder {
    crate::io_accounting::WORK
        .full_walks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    project_walk_builder_pruned(search_root, excludes, |_| true)
}

/// The project walk with one more condition, `descend`, on every entry below
/// the root. Every decision the walk makes about an entry it does visit is
/// unchanged: the ignore rules, the excluded directories and the content
/// exclusions are the same builder settings and the same entry predicate.
/// Only the set of entries visited shrinks.
fn project_walk_builder_pruned(
    search_root: &Path,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
    descend: impl Fn(&Path) -> bool + Send + Sync + 'static,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(search_root);
    // A disappearing child mount can make ReadDir::drop panic on ENXIO and abort
    // the daemon, so never open directories outside this walk root's filesystem.
    builder.same_file_system(true).hidden(false);
    // Nested linked worktrees of this repository index again under their own
    // roots; prune them here so every walk sharing this builder skips them.
    // The named root itself is always admitted (depth 0), so an explicitly
    // targeted walk inside a worktree still reads it.
    let worktrees = crate::project_content::NestedWorktreeBoundary::for_root(search_root);
    crate::context::apply_project_ignore_rules(&mut builder, search_root).filter_entry(
        move |entry| {
            project_walk_admits_entry(entry, excludes)
                && descend(entry.path())
                && !is_nested_worktree_entry(entry, &worktrees)
        },
    );
    builder
}

/// True when `entry` is a directory below the walk root that is itself a
/// nested linked worktree of this repository, which the walk must prune.
/// Shared by every walker built on the project walk; the depth check keeps
/// an explicitly named root (depth 0) admitted.
pub(crate) fn is_nested_worktree_entry(
    entry: &ignore::DirEntry,
    worktrees: &crate::project_content::NestedWorktreeBoundary,
) -> bool {
    entry.depth() > 0
        && entry
            .file_type()
            .as_ref()
            .is_some_and(std::fs::FileType::is_dir)
        && worktrees.is_nested_worktree_root(entry.path())
}

/// The membership rule the project walk applies to each entry on top of the
/// ignore rules: content exclusions, skipped directory names and OS metadata
/// files.
fn project_walk_admits_entry(
    entry: &ignore::DirEntry,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
) -> bool {
    let file_type = entry.file_type();
    if excludes(entry.path(), file_type.as_ref()) {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    // Git never tracks a path named `.git`. As a directory it is a repository's
    // own data; as a file it is a linked worktree's or submodule's pointer to
    // that data, holding an absolute path that differs per checkout. Neither is
    // project content, so both are skipped whatever their type.
    if name == ".git" {
        return false;
    }
    if file_type.as_ref().is_some_and(std::fs::FileType::is_dir) {
        return !matches!(
            name.as_ref(),
            "node_modules"
                | "target"
                | "venv"
                | ".venv"
                | ".git"
                | "__pycache__"
                | ".tox"
                | "dist"
                | "build"
        );
    }
    !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
}

/// `project_walk_lists` answers per file with the project walk's own rules,
/// so it must agree with `walk_project_files` on every file of a tree.
#[cfg(test)]
mod project_walk_lists_tests {}

#[cfg(all(test, unix))]
mod membership_admission_tests {}

fn walk_project_files_from_inner_matching<F>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    matches_file: F,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    let builder = project_walk_builder(search_root);

    #[cfg(test)]
    crate::search_hot_path_measurements::record(|counts| counts.project_walks += 1);
    let mut files = Vec::new();
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| counts.project_walk_entries += 1);
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if filters.matches(filter_root, &path) && matches_file(&path) {
            #[cfg(test)]
            crate::search_hot_path_measurements::record(|counts| counts.project_walk_files += 1);
            files.push(path);
            if max_files.is_some_and(|limit| files.len() > limit) {
                return Err(files.len());
            }
        }
    }

    if sort_by_mtime {
        sort_paths_by_mtime_desc(&mut files, filter_root);
    }
    Ok(files)
}

pub(crate) fn read_searchable_text(path: &Path) -> Option<String> {
    let bytes = crate::bounded_io::read(path, None).ok()?;
    if is_binary_bytes(&bytes) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Read an indexed file for grep verification. Keep the original I/O error so
/// callers distinguish a missing entry from blocked access or a scan timeout.
fn read_indexed_file_bytes(path: &Path, deadline: Option<Instant>) -> std::io::Result<Vec<u8>> {
    read_grep_file_bytes(path, deadline, false)
}

/// Reject the same binary prefix as a completed grep read before reserving
/// its body. Text keeps the original whole-buffer scan and matcher semantics.
pub(crate) fn read_grep_file_bytes(
    path: &Path,
    deadline: Option<Instant>,
    authorized_task_artifact: bool,
) -> std::io::Result<Vec<u8>> {
    let mut reader =
        crate::project_content::reader_bounded(path, deadline, authorized_task_artifact)?;
    let prefix = reader.chunk(Vec::new(), 1024)?;
    if is_binary_bytes(&prefix) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "binary grep input",
        ));
    }
    if prefix.len() < 1024 {
        return Ok(prefix);
    }
    reader.rest(prefix)
}

#[cfg(test)]
mod grep_binary_prefix_tests {
    use super::*;

    #[test]
    fn grep_binary_prefix_rejects_archive_before_returning_its_body() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("archive.tgz");
        std::fs::write(&path, [0x1f, 0x8b, 8, 0, 0, 0, 0, 0]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(100 << 20)
            .unwrap();
        assert_eq!(
            read_indexed_file_bytes(&path, None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn grep_binary_prefix_preserves_text_bytes_and_inspection_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("text.txt");
        for length in [0, 1, 1023, 1024, 1025, 3 << 20] {
            let mut content = vec![b'x'; length];
            if length > 1024 {
                content[1024] = 0;
            }
            std::fs::write(&path, &content).unwrap();
            assert!(!is_binary_bytes(&content));
            assert_eq!(read_indexed_file_bytes(&path, None).unwrap(), content);
        }
    }
}

/// Whether an I/O error says the path does not exist (including a path whose
/// parent is now a regular file). Permission and other errors do not count:
/// a path that could not be checked is never reported as missing.
fn io_error_means_missing_on_disk(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `path` is known to be absent from disk. One `stat`; an unreadable
/// path counts as present.
pub(crate) fn path_missing_on_disk(path: &Path) -> bool {
    #[cfg(test)]
    audit_record(|work| work.presence_checks += 1);
    crate::bounded_io::metadata(path).is_err_and(|error| io_error_means_missing_on_disk(&error))
}

/// Extra paths one listing may `stat` beyond its page while replacing entries
/// that turned out to be missing on disk. Keeps the check bounded even when an
/// index is badly out of date.
pub(crate) const MISSING_ON_DISK_REFILL_BUDGET: usize = 256;

/// Result of checking a ranked list against the disk, see
/// [`take_present_on_disk`].
#[derive(Debug)]
pub(crate) struct PresentOnDisk<T> {
    /// Up to `limit` items whose path exists, in their original order.
    pub present: Vec<T>,
    /// Items dropped because their path is not on disk.
    pub dropped: usize,
    /// Items after the last one examined. They were neither checked nor
    /// returned: either the page was full or the `stat` budget ran out.
    pub unexamined: usize,
    /// True when the `stat` budget ran out before the page was full, so
    /// `unexamined` items may hold present paths that were not returned.
    pub budget_exhausted: bool,
}

/// Backstop against a stale index: keep the first `limit` items whose path
/// is on disk, dropping the ones that are not. Only items that would be
/// returned are checked (one `stat` each, never a walk), plus at most
/// [`MISSING_ON_DISK_REFILL_BUDGET`] replacements. Order among present items
/// is unchanged, so ranking is unaffected. Drops are logged at debug level
/// with the reason; callers report `dropped` in their result.
pub(crate) fn take_present_on_disk<T>(
    items: impl IntoIterator<Item = T>,
    limit: usize,
    path_of: impl Fn(&T) -> &Path,
    surface: &str,
) -> PresentOnDisk<T> {
    match take_present_on_disk_with_admission(items, limit, path_of, surface, |_| {
        Ok::<_, std::convert::Infallible>(true)
    }) {
        Ok((present, _)) => present,
        Err(error) => match error {},
    }
}

/// Apply admission only to the bounded page/refill window. The untouched tail
/// remains unexamined, so callers cannot advertise it as an admitted total.
pub(crate) fn take_present_on_disk_with_admission<T, E>(
    items: impl IntoIterator<Item = T>,
    limit: usize,
    path_of: impl Fn(&T) -> &Path,
    surface: &str,
    mut admit: impl FnMut(&T) -> Result<bool, E>,
) -> Result<(PresentOnDisk<T>, usize), E> {
    let stat_budget = limit.saturating_add(MISSING_ON_DISK_REFILL_BUDGET);
    let mut present = Vec::new();
    let mut dropped = 0usize;
    let mut rejected = 0usize;
    let mut unexamined = 0usize;
    let mut stats = 0usize;
    let mut budget_exhausted = false;
    let mut dropped_examples = Vec::new();
    for item in items {
        if present.len() >= limit {
            unexamined += 1;
            continue;
        }
        if stats >= stat_budget {
            budget_exhausted = true;
            unexamined += 1;
            continue;
        }
        stats += 1;
        if path_missing_on_disk(path_of(&item)) {
            dropped += 1;
            if dropped_examples.len() < 3 {
                dropped_examples.push(path_of(&item).display().to_string());
            }
        } else if admit(&item)? {
            present.push(item);
        } else {
            rejected += 1;
        }
    }
    if dropped > 0 {
        crate::slog_debug!(
            "{surface}: dropped {dropped} indexed path(s) that are not on disk in this checkout (stale index entry), e.g. {}",
            dropped_examples.join(", ")
        );
    }
    Ok((
        PresentOnDisk {
            present,
            dropped,
            unexamined,
            budget_exhausted,
        },
        rejected,
    ))
}

pub(crate) fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn cache_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path(root);
    let normalized_path = normalize_path(path);
    let relative = normalized_path.strip_prefix(&normalized_root).ok()?;
    validate_cached_relative_path(relative)
}

/// The relative path a disk cache stores for `path` under `root`: taken as
/// spelled, or else after resolving `path` on disk (an alias spelling such as
/// macOS `/var` for `/private/var`). `None` for a path outside `root`.
/// Admission into an index and the cache writer share this one rule, so an
/// admitted path can always be written.
pub(crate) fn storable_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    cache_relative_path(root, path).or_else(|| {
        fs::canonicalize(path)
            .ok()
            .and_then(|canonical| cache_relative_path(root, &canonical))
    })
}

/// Canonical parent directories recur throughout one artifact reload. Cache only
/// that reload's results: a corpus commonly has many files per directory, while
/// retaining results across loads would need filesystem-change invalidation.
#[derive(Default)]
pub(crate) struct ParentCanonicalizationMemo {
    canonical_parents: HashMap<PathBuf, Option<PathBuf>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct CachedPathCanonicalizationWork {
    canonicalize_calls: usize,
    parent_lookups: usize,
    distinct_parents: HashSet<PathBuf>,
}

#[cfg(test)]
thread_local! {
    // Reload decoders are serial; isolate their counters from concurrent tests.
    static CACHED_PATH_CANONICALIZATION_WORK: std::cell::RefCell<Option<CachedPathCanonicalizationWork>> = const { std::cell::RefCell::new(None) };
}

impl ParentCanonicalizationMemo {
    fn canonicalize_parent_with<F>(
        &mut self,
        parent: &Path,
        canonicalize: &mut F,
    ) -> Option<PathBuf>
    where
        F: FnMut(&Path) -> std::io::Result<PathBuf>,
    {
        #[cfg(test)]
        CACHED_PATH_CANONICALIZATION_WORK.with(|slot| {
            if let Some(work) = slot.borrow_mut().as_mut() {
                work.parent_lookups += 1;
                work.distinct_parents.insert(parent.to_path_buf());
            }
        });
        self.canonical_parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| canonicalize(parent).ok())
            .clone()
    }
}

#[cfg(test)]
fn cached_path_under_root(root: &Path, relative_path: &Path) -> Option<PathBuf> {
    let mut canonical_parents = ParentCanonicalizationMemo::default();
    cached_path_under_root_with_memo(root, relative_path, &mut canonical_parents)
}

pub(crate) fn cached_path_under_root_with_memo(
    root: &Path,
    relative_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
) -> Option<PathBuf> {
    let mut canonicalize = |path: &Path| {
        #[cfg(test)]
        CACHED_PATH_CANONICALIZATION_WORK.with(|slot| {
            if let Some(work) = slot.borrow_mut().as_mut() {
                work.canonicalize_calls += 1;
            }
        });
        fs::canonicalize(path)
    };
    cached_path_under_root_with_memo_and_canonicalizer(
        root,
        relative_path,
        canonical_parents,
        &mut canonicalize,
    )
}

fn cached_path_under_root_with_memo_and_canonicalizer<F>(
    root: &Path,
    relative_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    let relative = validate_cached_relative_path(relative_path)?;
    let normalized_root = normalize_path(root);
    let full_path = normalize_path(&normalized_root.join(relative));

    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if !metadata.file_type().is_symlink() => {
            let parent = full_path.parent()?;
            let file_name = full_path.file_name()?;
            let canonical_path = canonical_parents
                .canonicalize_parent_with(parent, canonicalize)?
                .join(file_name);
            cached_path_if_contained(
                &normalized_root,
                &full_path,
                &canonical_path,
                canonical_parents,
                canonicalize,
            )
        }
        Ok(_) => {
            let canonical_path = canonicalize(&full_path).ok()?;
            cached_path_if_contained(
                &normalized_root,
                &full_path,
                &canonical_path,
                canonical_parents,
                canonicalize,
            )
        }
        Err(_) => {
            // If canonicalization fails, such as when an entry was deleted,
            // preserve the normalized lexical path as the fallback. Still
            // canonicalize its parent to seed the per-load memo so later sibling
            // entries do not repeat the realpath call for that directory.
            if let Some(parent) = full_path.parent() {
                let _ = canonical_parents.canonicalize_parent_with(parent, canonicalize);
            }
            is_within_search_root(&normalized_root, &full_path).then_some(full_path)
        }
    }
}

fn cached_path_if_contained<F>(
    normalized_root: &Path,
    full_path: &Path,
    canonical_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    // Normalize only the containment operands. The returned path remains in the
    // cache's established lexical form because path_to_id and semantic-cache
    // consumers use that exact key. The memo avoids canonicalization work only;
    // every record still takes this containment check.
    if is_within_search_root(normalized_root, canonical_path) {
        return Some(full_path.to_path_buf());
    }

    let canonical_root =
        canonical_parents.canonicalize_parent_with(normalized_root, canonicalize)?;
    is_within_search_root(&canonical_root, canonical_path).then_some(full_path.to_path_buf())
}

pub(crate) fn validate_cached_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// Sort paths newest-first by mtime, falling back to normalized display-path order.
///
/// The stable root keeps equal-mtime ordering independent of an absolute project
/// directory, so equivalent projects produce the same relative-path order. Root
/// and candidate copies are canonicalized only for key construction so Windows
/// verbatim and clean forms compare alike without changing the returned paths.
/// Metadata and display keys are snapshotted before sorting: the comparator must
/// remain a total order even if files change or disappear during the sort.
pub(crate) fn sort_paths_by_mtime_desc(paths: &mut [PathBuf], stable_root: &Path) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, true, None);
}

/// Sort paths emitted by one filesystem walk without resolving every path again.
///
/// Walk entries already share one resolved root and do not traverse symlinks, so
/// lexical normalization produces the same display tie-break key. Calling
/// `canonicalize` for every match only repeats filesystem lookups the walk just
/// performed.
pub(crate) fn sort_walked_paths_by_mtime_desc(paths: &mut [PathBuf], stable_root: &Path) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, false, None);
}

/// Indexed paths already have canonical keys and mtimes. Unindexed roots still
/// use the disk-backed ordering, so mixed-root glob retains the same contract.
pub(crate) fn sort_paths_by_cached_mtime_desc(
    paths: &mut [PathBuf],
    stable_root: &Path,
    mtimes: &HashMap<PathBuf, SystemTime>,
) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, true, Some(mtimes));
}

fn sort_paths_by_mtime_desc_with_key_normalization(
    paths: &mut [PathBuf],
    stable_root: &Path,
    canonicalize_paths: bool,
    cached_mtimes: Option<&HashMap<PathBuf, SystemTime>>,
) {
    use std::collections::HashMap;
    let stable_root = crate::inspect::job::canonicalize_normalized(stable_root);
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(paths.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(paths.len());
    for path in paths.iter() {
        mtimes.entry(path.clone()).or_insert_with(|| {
            cached_mtimes
                .and_then(|times| times.get(path))
                .copied()
                .or_else(|| path_modified_time(path))
        });
        display_paths.entry(path.clone()).or_insert_with(|| {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                stable_root.join(path)
            };
            let comparison_path = if canonicalize_paths
                && !cached_mtimes.is_some_and(|times| times.contains_key(path))
            {
                #[cfg(test)]
                audit_record(|work| work.sort_canonicalizes += 1);
                crate::inspect::job::canonicalize_normalized(&resolved)
            } else {
                crate::inspect::job::normalize_path(&resolved)
            };
            normalized_display_sort_key(Some(&stable_root), &comparison_path)
        });
    }
    paths.sort_by(|left, right| {
        let left_mtime = mtimes.get(left).and_then(|v| *v);
        let right_mtime = mtimes.get(right).and_then(|v| *v);
        let left_display = display_paths
            .get(left)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right)
            .map(String::as_bytes)
            .unwrap_or_default();
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.cmp(right))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. Same fix, applied to grep matches that share files.
pub(crate) fn sort_grep_matches_by_mtime_desc(matches: &mut [GrepMatch], project_root: &Path) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::new();
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        mtimes.entry(m.file.clone()).or_insert_with(|| {
            let resolved = resolve_match_path(project_root, &m.file);
            path_modified_time(&resolved)
        });
        display_paths
            .entry(m.file.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &m.file));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(&left.file).and_then(|v| *v);
        let right_mtime = mtimes.get(&right.file).and_then(|v| *v);
        let left_display = display_paths
            .get(&left.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(&right.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. The cached lookup function `modified_for_path` is fast (in-memory
/// table from the search index), but it can still return different values if
/// the file is modified mid-sort. Snapshot once.
fn sort_shared_grep_matches_by_cached_mtime_desc<F>(
    matches: &mut [SharedGrepMatch],
    project_root: &Path,
    modified_for_path: F,
) where
    F: Fn(&Path) -> Option<SystemTime>,
{
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(matches.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        let path = m.file.as_path().to_path_buf();
        mtimes
            .entry(path.clone())
            .or_insert_with(|| modified_for_path(&path));
        display_paths
            .entry(path.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &path));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(left.file.as_path()).and_then(|v| *v);
        let right_mtime = mtimes.get(right.file.as_path()).and_then(|v| *v);
        let left_display = display_paths
            .get(left.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

pub(crate) fn resolve_search_scope(project_root: &Path, path: Option<&str>) -> SearchScope {
    // Keep the returned scope path in the historical canonical/lexical form.
    // Only `is_within_search_root` normalizes its operands for the membership
    // comparison; callers pass this path on to filesystem and display logic.
    let resolved_project_root = canonicalize_or_normalize(project_root);
    let root = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                canonicalize_or_normalize(&path)
            } else {
                normalize_path(&resolved_project_root.join(path))
            }
        }
        None => resolved_project_root.clone(),
    };

    let use_index = is_within_search_root(&resolved_project_root, &root);
    SearchScope { root, use_index }
}

pub(crate) fn is_binary_bytes(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}

pub(crate) fn current_git_head(root: &Path) -> Option<String> {
    run_git(root, &["rev-parse", "HEAD"])
}

/// `storage_dir` is the root for rebuildable cache artifacts (see
/// [`crate::bash_background::cache_artifact_root`]), not AFT's storage root
/// for data that must survive (aft.db, backups).
pub fn resolve_cache_dir_with_key(project_key: &str, storage_dir: Option<&Path>) -> PathBuf {
    crate::bash_background::cache_artifact_root(storage_dir)
        .join("index")
        .join(project_key)
}

/// Fingerprint corpus-shaping ignore rules that are not represented by git HEAD.
///
/// The search cache stores this value next to the file mtimes. If `.gitignore`,
/// `.aftignore`, or `.git/info/exclude` changes while AFT is not running, a
/// matching HEAD + matching file mtimes is not enough to safely reuse the old
/// cache: files that are now ignored may still be indexed. Hashing the ignore
/// files themselves makes cold-start cache reuse agree with the current walker.
pub fn ignore_rules_fingerprint(project_root: &Path) -> String {
    ignore_rules_fingerprint_cancellable(project_root, &|| true)
        .expect("uncancellable ignore fingerprint")
}

fn ignore_rules_fingerprint_cancellable(
    project_root: &Path,
    keep_going: &dyn Fn() -> bool,
) -> Option<String> {
    use sha2::{Digest, Sha256};

    let root = canonicalize_or_normalize(project_root);
    let mut files = Vec::new();
    if !collect_ignore_rule_files(&root, &mut files, keep_going) {
        return None;
    }
    if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
        if global_ignore.is_file() {
            files.push(global_ignore);
        }
    }
    // Each entry pairs the name the file is hashed under with where it is
    // read from. The two differ only for the repository's `info/exclude`.
    let mut files: Vec<(PathBuf, PathBuf)> =
        files.into_iter().map(|path| (path.clone(), path)).collect();
    let info_exclude = git_info_exclude_path(&root);
    if info_exclude.is_file() {
        // `info/exclude` lives in the git directory shared by the home
        // checkout and all of its linked worktrees: inside the home checkout's
        // root but outside every worktree's. Hashing its real location would
        // make identical rules fingerprint differently per checkout, so it is
        // always hashed as `<root>/.git/info/exclude`. That is exactly the
        // name the home checkout's top level has always used, which keeps
        // fingerprints of existing top-level artifacts unchanged. Its content
        // is still read from the real file and still drives the fingerprint.
        files.push((root.join(".git").join("info").join("exclude"), info_exclude));
    }
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"aft-ignore-rules-v1\0");
    for (identity, path) in files {
        if !keep_going() {
            return None;
        }
        if let Some(relative) = cache_relative_path(&root, &identity) {
            hasher.update(relative.to_string_lossy().as_bytes());
        } else {
            hasher.update(identity.to_string_lossy().as_bytes());
        }
        hasher.update(b"\0");
        match fs::read(&path) {
            Ok(bytes) => hasher.update(&bytes),
            Err(error) => hasher.update(format!("read-error:{error}").as_bytes()),
        }
        hasher.update(b"\0");
    }

    Some(format!("{:x}", hasher.finalize()))
}

fn git_info_exclude_path(root: &Path) -> PathBuf {
    run_git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|| root.join(".git"))
    .join("info")
    .join("exclude")
}

fn collect_ignore_rule_files(
    root: &Path,
    files: &mut Vec<PathBuf>,
    keep_going: &dyn Fn() -> bool,
) -> bool {
    let mut builder = WalkBuilder::new(root);
    // Nested ignore discovery is a background recursive walk. Staying on one
    // filesystem keeps a mount that disappears mid-walk from aborting the
    // daemon when its directory listing is dropped.
    builder.same_file_system(true).hidden(false);
    // Same rules as the project walk, so rule files inside ignored trees
    // (which cannot change what is indexed) are not discovered.
    crate::context::apply_project_ignore_rules(&mut builder, root).filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            return !matches!(
                name.as_ref(),
                ".git"
                    | "node_modules"
                    | "target"
                    | "venv"
                    | ".venv"
                    | "__pycache__"
                    | ".tox"
                    | "dist"
                    | "build"
            );
        }
        true
    });

    let mut walker = builder.build();
    loop {
        if !keep_going() {
            return false;
        }
        let Some(entry) = walker.next() else {
            break;
        };
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let file_name = entry.file_name();
        if file_name == ".gitignore" || file_name == ".aftignore" {
            files.push(entry.into_path());
        }
    }
    true
}

/// Count directories visited when discovering ignore rule files (for perf regression tests).
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs(root: &Path) -> usize {
    let mut dirs = 0usize;
    let mut builder = WalkBuilder::new(root);
    builder.same_file_system(true).hidden(false);
    crate::context::apply_project_ignore_rules(&mut builder, root);
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            dirs += 1;
        }
    }
    dirs
}

/// The older stack-based discovery, kept only so perf tests can compare it.
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs_legacy_stack(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = 0usize;
    while let Some(dir) = stack.pop() {
        dirs += 1;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            if file_name == ".gitignore" || file_name == ".aftignore" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            if matches!(
                file_name.to_str().unwrap_or(""),
                ".git"
                    | "node_modules"
                    | "target"
                    | "venv"
                    | ".venv"
                    | "__pycache__"
                    | ".tox"
                    | "dist"
                    | "build"
            ) {
                continue;
            }
            stack.push(path);
        }
    }
    dirs
}

impl PathFilters {
    pub(crate) fn matches(&self, root: &Path, path: &Path) -> bool {
        if !crate::bash_background::registry::safe_task_entry(path) {
            return false;
        }
        if self.includes.is_none() && self.excludes.is_none() {
            return true;
        }
        #[cfg(test)]
        audit_record(|work| work.filter_keys += 1);
        // Compare normalized copies: the search root arrives non-verbatim while
        // indexed entries may carry the Windows `\\?\` form, and a raw
        // strip_prefix between the two forms fails, which read as "no match".
        let relative = to_glob_path(&relative_to_root(
            &normalize_path(root),
            &normalize_path(path),
        ));
        if self
            .includes
            .as_ref()
            .is_some_and(|includes| !includes.is_match(&relative))
        {
            return false;
        }
        if self
            .excludes
            .as_ref()
            .is_some_and(|excludes| excludes.is_match(&relative))
        {
            return false;
        }
        true
    }
}

fn canonicalize_for_search_membership(path: &Path) -> PathBuf {
    // Indexed files and requested scope roots meet in containment checks. Bare
    // `fs::canonicalize` yields a Windows verbatim (`\\?\`) path, while the
    // lexical fallback does not, so the two success/failure forms would silently
    // miss each other without this shared non-verbatim normalizer.
    crate::bounded_io::run(path, None, |path| {
        Ok(crate::inspect::job::canonicalize_normalized(&path))
    })
    .unwrap_or_else(|_| crate::inspect::job::normalize_path(path))
}

fn resolve_match_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn path_modified_time(path: &Path) -> Option<SystemTime> {
    #[cfg(test)]
    audit_record(|work| work.sort_stats += 1);
    crate::bounded_io::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn normalized_display_sort_key(project_root: Option<&Path>, path: &Path) -> String {
    let display_path = project_root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    to_glob_path(display_path)
}

fn normalize_path(path: &Path) -> PathBuf {
    crate::path_identity::lexical_normalize(path)
}

fn canonicalize_existing_or_deleted_path_with_memo(
    path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
) -> PathBuf {
    let mut canonicalize = |path: &Path| fs::canonicalize(path);
    canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer(
        path,
        canonical_parents,
        &mut canonicalize,
    )
}

fn canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer<F>(
    path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> PathBuf
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };

    // A file symlink needs full resolution before it can be matched to a stored
    // key. Ordinary files and known-missing entries are determined by their
    // parent, which is shared by many records during a verification pass.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        if let Ok(canonical) = canonicalize(path) {
            return canonical;
        }
    }

    canonical_parents
        .canonicalize_parent_with(parent, canonicalize)
        .map(|canonical_parent| canonical_parent.join(file_name))
        .unwrap_or_else(|| path.to_path_buf())
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    crate::inspect::job::normalize_path(path)
        .starts_with(crate::inspect::job::normalize_path(search_root))
}

impl QueryBuild {
    fn into_query(self) -> RegexQuery {
        let mut query = RegexQuery::default();

        for run in self.and_runs {
            add_run_to_and_query(&mut query, &run);
        }

        for group in self.or_groups {
            let mut trigrams = BTreeSet::new();
            let mut filters = HashMap::new();
            for run in group {
                for (trigram, filter) in trigram_filters(&run) {
                    trigrams.insert(trigram);
                    merge_filter(filters.entry(trigram).or_default(), filter);
                }
            }
            if !trigrams.is_empty() {
                query.or_groups.push(trigrams.into_iter().collect());
                query.or_filters.push(filters);
            }
        }

        query
    }
}

fn build_query(hir: &Hir) -> QueryBuild {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                QueryBuild {
                    and_runs: vec![literal.0.to_vec()],
                    or_groups: Vec::new(),
                }
            } else {
                QueryBuild::default()
            }
        }
        HirKind::Capture(capture) => build_query(&capture.sub),
        HirKind::Concat(parts) => {
            let mut build = QueryBuild::default();
            for part in parts {
                let part_build = build_query(part);
                build.and_runs.extend(part_build.and_runs);
                build.or_groups.extend(part_build.or_groups);
            }
            build
        }
        HirKind::Alternation(parts) => {
            let mut group = Vec::new();
            for part in parts {
                let Some(mut choices) = guaranteed_run_choices(part) else {
                    return QueryBuild::default();
                };
                group.append(&mut choices);
            }
            if group.is_empty() {
                QueryBuild::default()
            } else {
                QueryBuild {
                    and_runs: Vec::new(),
                    or_groups: vec![group],
                }
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                QueryBuild::default()
            } else {
                build_query(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => QueryBuild::default(),
    }
}

fn guaranteed_run_choices(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                Some(vec![literal.0.to_vec()])
            } else {
                None
            }
        }
        HirKind::Capture(capture) => guaranteed_run_choices(&capture.sub),
        HirKind::Concat(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                if let Some(mut part_runs) = guaranteed_run_choices(part) {
                    runs.append(&mut part_runs);
                }
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Alternation(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                let Some(mut part_runs) = guaranteed_run_choices(part) else {
                    return None;
                };
                runs.append(&mut part_runs);
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                None
            } else {
                guaranteed_run_choices(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => None,
    }
}

fn add_run_to_and_query(query: &mut RegexQuery, run: &[u8]) {
    for (trigram, filter) in trigram_filters(run) {
        if !query.and_trigrams.contains(&trigram) {
            query.and_trigrams.push(trigram);
        }
        merge_filter(query.and_filters.entry(trigram).or_default(), filter);
    }
}

fn trigram_filters(run: &[u8]) -> Vec<(u32, PostingFilter)> {
    trigram_filter_map(run, false).into_iter().collect()
}

fn merge_filter(target: &mut PostingFilter, filter: PostingFilter) {
    target.next_mask |= filter.next_mask;
    target.loc_mask |= filter.loc_mask;
}

fn mask_for_next_char(next_char: u8) -> u8 {
    let bit = (normalize_char(next_char).wrapping_mul(31) & 7) as u32;
    1u8 << bit
}

fn mask_for_position(position: usize) -> u8 {
    1u8 << (position % 8)
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| error.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    if !crate::developer_tools::git_usable() {
        return None;
    }
    let mut child = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GIT_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    let value = String::from_utf8(stdout).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn line_starts_bytes(content: &[u8]) -> Vec<usize> {
    #[cfg(test)]
    LINE_STARTS_BUILDS.with(|count| count.set(count.get() + 1));
    let mut starts = vec![0usize];
    for (index, byte) in content.iter().copied().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

#[cfg(test)]
thread_local! {
    pub(crate) static LINE_STARTS_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static POSTING_COUNT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SUPERSEDED_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Called on every `line_details_bytes` call on this thread, so tests can
    /// count line-detail work or act at a chosen point inside a scan.
    static LINE_DETAILS_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    static GREP_SCAN_CLOCK_ADVANCE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

#[cfg(test)]
fn grep_scan_test_now() -> Instant {
    Instant::now() + GREP_SCAN_CLOCK_ADVANCE.with(Cell::get)
}

/// Line number, 1-based character column and printable text of the line
/// holding `offset`, plus the byte offset where the next line starts (the
/// end of `content` for the last line). The text is bounded the way grep
/// prints it, see [`bounded_grep_line_text_bytes`].
pub(crate) fn line_details_bytes(
    content: &[u8],
    line_starts: &[usize],
    offset: usize,
) -> (u32, u32, String, usize) {
    line_details_bytes_with_cr_policy(content, line_starts, offset, false)
}

pub(crate) fn line_details_bytes_with_cr_policy(
    content: &[u8],
    line_starts: &[usize],
    offset: usize,
    trim_all_cr: bool,
) -> (u32, u32, String, usize) {
    #[cfg(test)]
    LINE_DETAILS_PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            probe();
        }
    });

    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    // Every entry after the first in `line_starts` follows a newline, so the
    // next entry, when there is one, is one byte past this line's newline.
    let (line_end, next_line_start) = match line_starts.get(line_index + 1) {
        Some(&next) => (next - 1, next),
        None => (content.len(), content.len()),
    };
    let mut line_slice = &content[line_start..line_end];
    // The folder-walk grep path (`trim_all_cr`) trims every trailing CR;
    // indexed grep removes only the CR of a CRLF line ending.
    while line_slice.ends_with(b"\r") {
        line_slice = &line_slice[..line_slice.len() - 1];
        if !trim_all_cr {
            break;
        }
    }
    let line_text = bounded_grep_line_text_bytes(line_slice);
    let column = String::from_utf8_lossy(&content[line_start..offset])
        .chars()
        .count() as u32
        + 1;
    (line_index as u32 + 1, column, line_text, next_line_start)
}

/// The text grep keeps for a matching line. A line of at most
/// `GREP_MAX_LINE_CHARS` characters is kept whole. A longer line is kept as
/// exactly what the grep renderer prints for it: its first
/// `GREP_MAX_LINE_CHARS` characters and the truncation marker. Every renderer
/// cuts at that length or shorter, so the printed output is the same as for
/// the whole line, while a match on a multi-megabyte minified line no longer
/// decodes and copies the whole line.
pub(crate) fn bounded_grep_line_text(line: &str) -> String {
    use crate::commands::grep::{GREP_LINE_TRUNCATED_MARKER, GREP_MAX_LINE_CHARS};
    match line.char_indices().nth(GREP_MAX_LINE_CHARS) {
        None => line.to_string(),
        Some((cut, _)) => format!("{}{}", &line[..cut], GREP_LINE_TRUNCATED_MARKER),
    }
}

/// [`bounded_grep_line_text`] for raw line bytes, decoded lossily as the
/// whole line would be. Only a prefix of the line is decoded: a UTF-8
/// character is at most 4 bytes and an invalid byte decodes to one
/// replacement character, so the first `(GREP_MAX_LINE_CHARS + 2) * 4` bytes
/// hold at least `GREP_MAX_LINE_CHARS + 1` characters that decode exactly as
/// in the whole line (only a sequence cut by the prefix end, at most its last
/// 3 bytes, can decode differently). A line longer than that prefix therefore
/// always has more than `GREP_MAX_LINE_CHARS` characters and is cut.
fn bounded_grep_line_text_bytes(line: &[u8]) -> String {
    const PREFIX_BYTES: usize = (crate::commands::grep::GREP_MAX_LINE_CHARS + 2) * 4;
    let prefix = &line[..line.len().min(PREFIX_BYTES)];
    bounded_grep_line_text(&String::from_utf8_lossy(prefix))
}

fn to_glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// Thread-local operation counters keep concurrent tests from charging each
// other's filesystem work. They observe the call sites, not elapsed time.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuditWork {
    pub cancel_probes: usize,
    pub corpus_opens: usize,
    pub sort_stats: usize,
    pub sort_canonicalizes: usize,
    pub watcher_global: usize,
    pub watcher_head: usize,
    pub filter_keys: usize,
    pub presence_checks: usize,
}

#[cfg(test)]
thread_local! {
    static AUDIT_WORK: std::cell::Cell<AuditWork> = std::cell::Cell::new(AuditWork::default());
    pub(crate) static AUDIT_IO_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AUDIT_CANCEL_ELAPSED: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct HelperStateScope(AuditWork, Option<Box<dyn FnMut()>>, Option<Duration>);
#[cfg(test)]
impl Drop for HelperStateScope {
    fn drop(&mut self) {
        AUDIT_WORK.with(|work| work.set(self.0));
        AUDIT_IO_PROBE.with(|probe| *probe.borrow_mut() = self.1.take());
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.set(self.2));
    }
}
#[cfg(test)]
pub(crate) fn helper_state_scope() -> HelperStateScope {
    HelperStateScope(
        AUDIT_WORK.with(|work| work.replace(AuditWork::default())),
        AUDIT_IO_PROBE.with(|probe| probe.replace(None)),
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.replace(None)),
    )
}

#[cfg(test)]
pub(crate) fn audit_cancel_elapsed(started: Instant) -> Duration {
    AUDIT_CANCEL_ELAPSED.with(|slot| slot.get().unwrap_or_else(|| started.elapsed()))
}

#[cfg(test)]
pub(crate) fn audit_record(record: impl FnOnce(&mut AuditWork)) {
    AUDIT_WORK.with(|slot| {
        let mut work = slot.get();
        record(&mut work);
        slot.set(work);
    });
}

#[cfg(test)]
pub(crate) fn audit_work_reset() {
    AUDIT_WORK.with(|slot| slot.set(AuditWork::default()));
}

#[cfg(test)]
pub(crate) fn audit_work() -> AuditWork {
    AUDIT_WORK.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    #[test]
    fn call_keepalive_indexed_projection_and_glob_report_completed_rows() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant, SystemTime};
        let root = tempfile::tempdir().unwrap();
        let rows = ["a.rs", "b.rs", "c.rs"].map(|name| {
            let path = root.path().join(name);
            std::fs::write(&path, "").unwrap();
            (
                super::FileEntry {
                    path,
                    size: 0,
                    modified: SystemTime::UNIX_EPOCH,
                    content_hash: blake3::hash(b""),
                },
                None,
            )
        });
        let clock = Arc::new(AtomicU64::new(0));
        let sent = Arc::new(AtomicU64::new(0));
        let origin = Instant::now();
        let token = crate::executor::JobCancellation::new();
        token.set_progress(crate::run_tool_call::progress::Progress::with_clock(
            {
                let clock = clock.clone();
                move || origin + Duration::from_secs(clock.fetch_add(30, Ordering::SeqCst))
            },
            {
                let sent = sent.clone();
                move |_| {
                    sent.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
        ));
        let _token = crate::executor::install_job_cancellation(token);
        let snapshot = super::SearchIndexSnapshot::from_verified_rows(root.path(), 1024, rows);
        assert_eq!(sent.load(Ordering::SeqCst), 3);
        let (files, has_files, visited) = snapshot.glob_profiled("*.rs", root.path(), false);
        assert!(has_files);
        assert_eq!((files.len(), visited), (3, 3));
        assert_eq!(sent.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn semantic_namespaces_share_type_identity() {
        use std::any::TypeId;
        assert_eq!(
            TypeId::of::<super::exact_lane::ExactLane>(),
            TypeId::of::<crate::commands::semantic_search::exact_lane::ExactLane>()
        );
        assert_eq!(
            TypeId::of::<super::memo::ExactMemoStore>(),
            TypeId::of::<crate::commands::semantic_search::memo::ExactMemoStore>()
        );
    }

    #[test]
    fn storage_and_sqlite_files_never_reach_corpus_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("task-storage");
        fs::create_dir_all(storage.join("nested")).unwrap();
        {
            let _env = crate::test_env::without_storage_override();
            assert_eq!(crate::bash_background::storage_dir(Some(&storage)), storage);
        }
        fs::write(root.join("source.rs"), "fn user_source() {}\n").unwrap();
        for path in [
            storage.join("aft.db"),
            storage.join("aft.db-wal"),
            storage.join("aft.db-shm"),
            storage.join("nested/internal.rs"),
            root.join("other.sqlite"),
            root.join("other.sqlite-wal"),
            root.join("other.sqlite-shm"),
        ] {
            fs::write(path, "must not open this as project content\n").unwrap();
        }
        let probe = crate::project_content::Probe::new(&root);
        let index = SearchIndex::build_with_limit(&root, DEFAULT_MAX_FILE_SIZE);
        let opened = probe.paths();
        assert!(
            opened.contains(&root.join("source.rs")),
            "source was not indexed: {opened:?}"
        );
        assert!(
            opened.iter().all(|path| path == &root.join("source.rs")),
            "indexer opened storage or SQLite files: {opened:?}"
        );
        drop(index);
        for path in [
            storage.join("aft.db-shm"),
            storage.join("nested/internal.rs"),
        ] {
            audit_work_reset();
            let _ = read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE);
            assert_eq!(
                audit_work().corpus_opens,
                0,
                "direct corpus read opened {path:?}"
            );
        }
    }

    #[test]
    fn audit10_candidate_sort_counts_each_trigram_once() {
        let index = super::SearchIndex::new();
        let query = super::RegexQuery {
            and_trigrams: (0..32).rev().collect(),
            and_filters: std::collections::HashMap::new(),
            or_groups: Vec::new(),
            or_filters: Vec::new(),
        };
        super::POSTING_COUNT_CALLS.with(|calls| calls.set(0));
        assert!(index.snapshot().candidates(&query).unwrap().is_empty());
        super::POSTING_COUNT_CALLS.with(|calls| assert_eq!(calls.get(), 32));
    }

    #[test]
    fn perf_audit_empty_filters_skip_path_keys() {
        let root = Path::new("/project");
        audit_work_reset();
        for i in 0..1000 {
            assert!(PathFilters::default().matches(root, &root.join(format!("src/file_{i}.rs"))));
        }
        assert_eq!(audit_work().filter_keys, 0);
        let filters = build_path_filters(&["**/*.rs".to_string()], &[]).unwrap();
        assert!(filters.matches(root, &root.join("src/main.rs")));
        assert!(!filters.matches(root, &root.join("src/main.txt")));
    }
    #[test]
    fn perf_audit_cancellation_probe_budget() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        let _guard = crate::executor::install_job_cancellation(token.clone());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(Some(Duration::ZERO)));
        audit_work_reset();
        for _ in 0..100_000 {
            assert!(!crate::executor::current_job_cancelled());
        }
        let work = audit_work();
        assert_eq!(work.cancel_probes, 391, "{work:?}");
        fs::remove_dir(dir.path()).unwrap();
        assert!((0..256).any(|_| crate::executor::current_job_cancelled()));
        assert!(token.cancel_already_requested());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(None));
    }

    #[test]
    fn cancellation_time_probe_and_explicit_signal_are_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        assert!(!token.cancel_requested_before_commit());
        fs::remove_dir(dir.path()).unwrap();
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(Some(Duration::from_millis(50))));
        assert!(token.cancel_requested_before_commit());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(None));
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        assert!(!token.cancel_requested_before_commit());
        token.request_cancel();
        assert!(token.cancel_requested_before_commit());
    }

    #[test]
    fn perf_audit_corpus_opens_once() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..256 {
            let path = dir.path().join(format!("source_{i:03}.rs"));
            fs::write(
                &path,
                format!("// generated by fixture\nfn marker_{i}() {{}}\n"),
            )
            .unwrap();
        }
        audit_work_reset();
        for i in 0..256 {
            let path = dir.path().join(format!("source_{i:03}.rs"));
            assert!(matches!(
                prepare_search_path(&path, DEFAULT_MAX_FILE_SIZE),
                PreparedSearchPath::Indexed(_)
            ));
        }
        let work = audit_work();
        assert_eq!(work.corpus_opens, 256, "{work:?}");
    }

    #[test]
    fn corpus_reader_preserves_generated_and_binary_eligibility() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        for bytes in [
            b"// generated by fixture\nfn marker() {}\n".as_slice(),
            b"fn marker() {}\n",
            b"\xff\n// generated by fixture\n",
        ] {
            fs::write(&path, bytes).unwrap();
            let expected_generated = crate::inspect::is_generated_file(&path, &path);
            match read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE) {
                SearchCorpusEligibility::Eligible(file) => {
                    assert_eq!(file.bytes, bytes);
                    assert_eq!(file.generated, expected_generated);
                }
                _ => panic!("text remains eligible"),
            }
        }
        fs::write(&path, b"binary\0content").unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE),
            SearchCorpusEligibility::Unindexed(_)
        ));
        fs::write(&path, "oversize content").unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, 3),
            SearchCorpusEligibility::Unindexed(_)
        ));
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE),
            SearchCorpusEligibility::Skipped
        ));
    }

    use super::*;
    use crate::root_identity::key::tests::git_command_for_test;
    use std::process::Command;

    /// Unmatched leading `..` components must survive comparison normalization:
    /// dropping them makes a path that escapes upward look like a nearby child.
    #[test]
    fn comparison_adapter_preserves_relative_parent_components() {
        for (input, expected) in [("../../x", "../../x"), ("a/../../x", "../x")] {
            assert_eq!(
                super::normalize_path(std::path::Path::new(input)),
                std::path::PathBuf::from(expected),
                "{input}"
            );
        }
    }

    /// An update for a path outside the project root must not add a sibling
    /// project's files to the index, whichever caller sends it.
    #[test]
    fn update_for_a_path_outside_the_root_is_not_admitted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let base = fs::canonicalize(dir.path()).expect("canonical temp dir");
        let project = base.join("project");
        let sibling = base.join("project-sibling");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&sibling).expect("create sibling dir");
        fs::write(project.join("inside.rs"), "fn inside() {}").expect("write inside");
        let outside = sibling.join("outside.rs");
        fs::write(&outside, "fn outside() {}").expect("write outside");

        let mut index = SearchIndex::build(&project);
        let files_before = index.path_to_id.len();
        index.update_file(&outside);
        assert!(!index.path_to_id.contains_key(&outside));
        assert_eq!(index.path_to_id.len(), files_before);
        let added = project.join("added.rs");
        fs::write(&added, "fn added() {}").expect("write added");
        index.update_file(&added);
        assert!(
            index.path_to_id.contains_key(&added),
            "in-root updates still apply"
        );
    }

    #[test]
    fn cached_path_under_root_allows_missing_lexical_child() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        let path = cached_path_under_root(&root, Path::new("future/file.rs"))
            .expect("missing child should fall back to lexical validation");

        assert_eq!(path, root.join("future/file.rs"));
    }

    #[cfg(unix)]
    fn cached_path_before_reload_memo(root: &Path, relative_path: &Path) -> Option<PathBuf> {
        // Resolve each parent afresh, as before reload-wide memo sharing, so this
        // reference checks returned spellings without using the optimized helper.
        let relative = validate_cached_relative_path(relative_path)?;
        let root = normalize_path(root);
        let full = normalize_path(&root.join(relative));
        let canonical = match fs::symlink_metadata(&full) {
            Ok(metadata) if !metadata.file_type().is_symlink() => fs::canonicalize(full.parent()?)
                .ok()?
                .join(full.file_name()?),
            Ok(_) => fs::canonicalize(&full).ok()?,
            Err(_) => return is_within_search_root(&root, &full).then_some(full),
        };
        if is_within_search_root(&root, &canonical) {
            Some(full)
        } else {
            let canonical_root = fs::canonicalize(&root).ok()?;
            is_within_search_root(&canonical_root, &canonical).then_some(full)
        }
    }

    #[cfg(unix)]
    fn assert_reload_path_identity(project: &Path) {
        let root = fs::canonicalize(project).unwrap();
        let source = root.join("MixedCase");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("File.rs"), "pub fn identity() {}\n").unwrap();
        fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(&source, root.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(source.join("File.rs"), root.join("file-link.rs")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            root.join("secret-link.rs"),
        )
        .unwrap();
        let alias_directory = tempfile::tempdir().unwrap();
        let alias = alias_directory.path().join("root-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut roots = vec![root.clone(), alias];
        #[cfg(target_os = "macos")]
        {
            let var_alias = Path::new("/").join(root.strip_prefix("/private").unwrap());
            assert!(var_alias.starts_with("/var/"));
            roots.push(var_alias);
        }
        let case_insensitive = source.join("file.rs").exists();
        if case_insensitive {
            roots.push(
                root.parent()
                    .unwrap()
                    .join(root.file_name().unwrap().to_str().unwrap().to_uppercase()),
            );
        }
        let relatives = [
            "MixedCase/File.rs",
            "mixedcase/file.rs",
            "MixedCase/./File.rs",
            "linked/File.rs",
            "linked/missing.rs",
            "escape/secret.rs",
            "escape/missing.rs",
            "file-link.rs",
            "secret-link.rs",
            "missing/child.rs",
            "../outside.rs",
            "/absolute.rs",
        ];
        let mut comparisons = 0;
        for spelling in &roots {
            let mut memo = ParentCanonicalizationMemo::default();
            // Repeat with the same memo to exercise hits as well as misses.
            for _ in 0..2 {
                for relative in relatives {
                    let old = cached_path_before_reload_memo(spelling, Path::new(relative));
                    let new =
                        cached_path_under_root_with_memo(spelling, Path::new(relative), &mut memo);
                    assert_eq!(
                        new.as_deref().map(Path::as_os_str),
                        old.as_deref().map(Path::as_os_str),
                        "returned bytes differ for root={spelling:?}, relative={relative:?}"
                    );
                    comparisons += 1;
                }
            }
        }
        assert!(cached_path_before_reload_memo(&root, Path::new("escape/secret.rs")).is_none());
        eprintln!("reload identity: {comparisons} byte-identical results, case_insensitive={case_insensitive}, roots={}", roots.len());
    }

    #[cfg(unix)]
    #[test]
    fn warm_reload_path_identity_matches_old_resolver() {
        #[cfg(target_os = "macos")]
        let project = tempfile::Builder::new()
            .prefix("aft-identity-")
            .tempdir_in("/var/tmp")
            .unwrap();
        #[cfg(not(target_os = "macos"))]
        let project = tempfile::tempdir().unwrap();
        assert_reload_path_identity(project.path());
    }

    #[test]
    fn deleted_path_uses_one_parent_canonicalization() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let source = project.join("src");
        fs::create_dir_all(&source).expect("create source dir");
        let deleted = source.join("deleted.rs");
        let expected = fs::canonicalize(&source)
            .expect("canonicalize source dir")
            .join("deleted.rs");
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        let mut canonicalize_calls = 0;
        let mut canonicalize = |path: &Path| {
            canonicalize_calls += 1;
            fs::canonicalize(path)
        };

        let resolved = canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer(
            &deleted,
            &mut canonical_parents,
            &mut canonicalize,
        );

        assert_eq!(resolved, expected);
        assert_eq!(canonicalize_calls, 1, "only the parent is canonicalized");
    }

    #[cfg(unix)]
    #[test]
    fn cached_path_memo_handles_mixed_entries_without_relaxing_symlink_checks() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let source = project.join("src");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&source).expect("create source dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        fs::write(source.join("present.rs"), "present").expect("write present file");
        fs::write(outside.join("secret.rs"), "secret").expect("write outside file");
        std::os::unix::fs::symlink(source.join("present.rs"), project.join("inside.rs"))
            .expect("create in-root symlink");
        std::os::unix::fs::symlink(outside.join("secret.rs"), project.join("outside.rs"))
            .expect("create escaping symlink");
        let root = fs::canonicalize(&project).expect("canonicalize project");
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        let mut canonicalized_paths = Vec::new();
        let mut canonicalize = |path: &Path| {
            canonicalized_paths.push(path.to_path_buf());
            fs::canonicalize(path)
        };

        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("src/present.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("src/present.rs"))
        );
        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("src/deleted.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("src/deleted.rs"))
        );
        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("inside.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("inside.rs"))
        );
        assert!(cached_path_under_root_with_memo_and_canonicalizer(
            &root,
            Path::new("outside.rs"),
            &mut canonical_parents,
            &mut canonicalize,
        )
        .is_none());

        assert_eq!(
            canonicalized_paths,
            vec![
                root.join("src"),
                root.join("inside.rs"),
                root.join("outside.rs"),
                root.clone(),
            ],
            "siblings reuse their parent while symlinks still resolve individually"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_path_under_root_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        fs::write(outside.join("secret.txt"), "secret").expect("write outside file");
        std::os::unix::fs::symlink(&outside, project.join("link")).expect("create symlink");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        assert!(cached_path_under_root(&root, Path::new("link/secret.txt")).is_none());
    }

    #[test]
    fn extract_trigrams_tracks_next_char_and_position() {
        let trigrams = extract_trigrams(b"Rust");
        assert_eq!(trigrams.len(), 2);
        assert_eq!(trigrams[0], (pack_trigram(b'r', b'u', b's'), b't', 0));
        assert_eq!(
            trigrams[1],
            (pack_trigram(b'u', b's', b't'), EOF_SENTINEL, 1)
        );
    }

    #[test]
    fn index_file_trigram_filters_match_legacy_extraction() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("sample.txt");
        let content = b"Rust rust RUST\nxy";
        fs::write(&path, content).expect("write sample");

        let mut expected = BTreeMap::new();
        for (trigram, next_char, position) in extract_trigrams(content) {
            let entry: &mut PostingFilter = expected.entry(trigram).or_default();
            entry.next_mask |= mask_for_next_char(next_char);
            entry.loc_mask |= mask_for_position(position);
        }

        let mut index = SearchIndex::new();
        index.project_root = dir.path().to_path_buf();
        index.index_file(&path, content);

        let file_id = *index.path_to_id.get(&path).expect("file indexed");
        let file_trigrams = index
            .delta_file_trigrams
            .get(&file_id)
            .expect("delta file trigrams");
        assert_eq!(file_trigrams, &expected.keys().copied().collect::<Vec<_>>());
        for (trigram, filter) in expected {
            let postings = index
                .delta
                .postings
                .get(&trigram)
                .expect("delta posting list");
            assert_eq!(postings.len(), 1);
            assert_eq!(postings[0].file_id, file_id);
            assert_eq!(postings[0].next_mask, filter.next_mask);
            assert_eq!(postings[0].loc_mask, filter.loc_mask);
        }
    }

    #[test]
    fn decompose_regex_extracts_literals_and_alternations() {
        let query = decompose_regex("abc(def|ghi)xyz");
        assert!(query.and_trigrams.contains(&pack_trigram(b'a', b'b', b'c')));
        assert!(query.and_trigrams.contains(&pack_trigram(b'x', b'y', b'z')));
        assert_eq!(query.or_groups.len(), 1);
        assert!(query.or_groups[0].contains(&pack_trigram(b'd', b'e', b'f')));
        assert!(query.or_groups[0].contains(&pack_trigram(b'g', b'h', b'i')));
    }

    #[test]
    fn candidates_intersect_posting_lists() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let alpha = dir.path().join("alpha.txt");
        let beta = dir.path().join("beta.txt");
        fs::write(&alpha, "abcdef").expect("write alpha");
        fs::write(&beta, "abcxyz").expect("write beta");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&alpha, b"abcdef");
        index.index_file(&beta, b"abcxyz");

        let query = RegexQuery {
            and_trigrams: vec![
                pack_trigram(b'a', b'b', b'c'),
                pack_trigram(b'd', b'e', b'f'),
            ],
            ..RegexQuery::default()
        };

        let candidates = index.candidates(&query).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(index.files[candidates[0] as usize].path, alpha);
    }

    #[test]
    fn candidates_apply_bloom_filters() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("sample.txt");
        fs::write(&file, "abcd efgh").expect("write sample");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&file, b"abcd efgh");

        let trigram = pack_trigram(b'a', b'b', b'c');
        let matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'd'),
            loc_mask: mask_for_position(0),
        };
        let non_matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'z'),
            loc_mask: mask_for_position(0),
        };

        assert_eq!(
            index
                .postings_for_trigram(trigram, Some(matching_filter))
                .unwrap()
                .len(),
            1
        );
        assert!(index
            .postings_for_trigram(trigram, Some(non_matching_filter))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn snapshot_started_before_edit_keeps_coherent_pre_edit_postings() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let project = fs::canonicalize(project).expect("canonicalize project");
        let file = project.join("source.txt");
        fs::write(&file, "old_generation marker").expect("write old source");

        let mut index = SearchIndex::build(&project);
        index.ready = true;
        let old_file_id = *index.path_to_id.get(&file).expect("old file id");
        let index = std::sync::RwLock::new(index);

        let before_edit = {
            let guard = index.read().expect("read index");
            let snapshot = guard.snapshot();
            assert!(Arc::ptr_eq(&snapshot.delta, &guard.delta));
            snapshot
        };

        fs::write(&file, "new_generation marker").expect("write new source");
        index.write().expect("write index").update_file(&file);

        let after_edit = index.read().expect("read updated index").snapshot();
        let new_file_id = *after_edit.path_to_id.get(&file).expect("new file id");
        assert!(!Arc::ptr_eq(&before_edit.delta, &after_edit.delta));

        let old_trigram = pack_trigram(b'o', b'l', b'd');
        let new_trigram = pack_trigram(b'n', b'e', b'w');
        assert_eq!(
            before_edit.postings_for_trigram(old_trigram, None).unwrap(),
            vec![old_file_id]
        );
        assert!(before_edit
            .postings_for_trigram(new_trigram, None)
            .unwrap()
            .is_empty());
        assert!(after_edit
            .postings_for_trigram(old_trigram, None)
            .unwrap()
            .is_empty());
        assert_eq!(
            after_edit.postings_for_trigram(new_trigram, None).unwrap(),
            vec![new_file_id]
        );

        let dirty_result = index
            .read()
            .expect("read dirty index")
            .grep("new_generation", true, &[], &[], &project, 100)
            .unwrap();
        let reference = SearchIndex::build(&project);
        let reference_result = reference
            .grep("new_generation", true, &[], &[], &project, 100)
            .unwrap();
        assert_eq!(dirty_result.matches, reference_result.matches);
        assert_eq!(dirty_result.total_matches, reference_result.total_matches);
    }

    #[test]
    fn cache_path_helpers_reject_absolute_and_parent_paths() {
        let root = PathBuf::from("/tmp/aft-project");

        assert_eq!(
            cache_relative_path(&root, &root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert!(cache_relative_path(&root, Path::new("/tmp/outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("../outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("/tmp/outside.rs")).is_none());
        assert_eq!(
            cached_path_under_root(&root, Path::new("src/./lib.rs")),
            Some(root.join("src/lib.rs"))
        );
    }

    #[test]
    fn resolve_search_scope_disables_index_for_external_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");

        let scope = resolve_search_scope(&project, outside.to_str());

        assert_eq!(
            scope.root,
            fs::canonicalize(&outside).expect("canonicalize outside")
        );
        assert!(!scope.use_index);
    }

    #[test]
    fn grep_filters_matches_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let docs = project.join("docs");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&docs).expect("create docs dir");
        fs::write(src.join("main.rs"), "pub struct SearchIndex;\n").expect("write src file");
        fs::write(docs.join("guide.md"), "SearchIndex guide\n").expect("write docs file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10).unwrap();

        assert_eq!(result.files_searched, 1);
        assert_eq!(result.files_with_matches, 1);
        assert_eq!(result.matches.len(), 1);
        // Index stores canonicalized paths; on macOS /var → /private/var
        let expected = fs::canonicalize(src.join("main.rs")).expect("canonicalize");
        assert_eq!(result.matches[0].file, expected);
    }

    #[test]
    fn grep_deduplicates_multiple_matches_on_same_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex SearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10).unwrap();

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn indexed_regex_grep_patterns_are_line_oriented() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("stderr.log");
        fs::write(&file, "\n  566 pass\nx\n").expect("write file");
        let index = SearchIndex::build(&project);
        assert!(index.is_ready());

        let whitespace = index
            .grep(r"^\s+[0-9]+ pass", true, &[], &[], &project, 10)
            .unwrap();
        assert_eq!(whitespace.matches.len(), 1);
        assert_eq!(whitespace.matches[0].line, 2);
        assert_eq!(whitespace.matches[0].line_text, "  566 pass");
        assert_eq!(whitespace.matches[0].match_text, "  566 pass");

        let negated_class = index.grep(r"[^x]+", true, &[], &[], &project, 10).unwrap();
        assert_eq!(negated_class.matches.len(), 1);
        assert_eq!(negated_class.matches[0].line, 2);
        assert_eq!(negated_class.matches[0].match_text, "  566 pass");
        assert!(negated_class
            .matches
            .iter()
            .all(|matched| !matched.match_text.contains('\n')));

        let explicit_newline = index
            .grep(r"pass\nx", true, &[], &[], &project, 10)
            .unwrap();
        assert!(explicit_newline.matches.is_empty());

        let regex = match pattern_compile::compile(r"[^x]+", CompileOpts::default()) {
            CompileResult::Ok(CompiledPattern::Regex { compiled, .. }) => compiled,
            other => panic!("compile regex: {other:?}"),
        };
        let never_keep = |_matched: &GrepMatch| false;
        let (collected, collected_lines) = matching_lines_in_content(
            &file,
            b"\n  566 pass\nx\n",
            &SearchMatcher::Regex(regex),
            10,
            &never_keep,
        );
        assert_eq!(collected_lines, 1);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].line, 2);
        assert_eq!(collected[0].match_text, "  566 pass");

        let literal_newline = match pattern_compile::compile(
            "pass\nx",
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(pattern) => pattern,
            other => panic!("compile literal newline: {other:?}"),
        };
        let result = index
            .search_grep(&literal_newline, &[], &[], &project, 10)
            .unwrap();
        assert!(result.matches.is_empty());
    }

    #[test]
    fn indexed_cross_line_regex_rescans_starting_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("cross-line.log");
        std::fs::write(&file, "foo hit\nbar\nprefix foo hit\nbar\n")
            .expect("write cross-line fixture");
        let index = SearchIndex::build(&project);
        let result = index
            .grep(r"foo.*\nbar|hit", true, &[], &[], &project, 10)
            .unwrap();

        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].line, 1);
        assert_eq!(result.matches[0].line_text, "foo hit");
        assert_eq!(result.matches[0].match_text, "hit");
        assert_eq!(result.matches[1].line, 3);
        assert_eq!(result.matches[1].line_text, "prefix foo hit");
        assert_eq!(result.matches[1].match_text, "hit");
    }

    #[test]
    fn grep_case_insensitive_unicode_literal_matches_indexed_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("unicode.txt");
        fs::write(&file, "äbc\n").expect("write unicode file");

        let index = SearchIndex::build(&project);
        let result = index.grep("Äbc", false, &[], &[], &project, 10).unwrap();

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            result.matches[0].file,
            fs::canonicalize(file).expect("canonicalize unicode file")
        );
    }

    fn assert_ready_indexed_literal_match(needle: &str) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("literal-escapes.txt");
        fs::write(&file, "foo\\.bar\na\\\\b\nfoo\\tbar\nsplit\\(\"x\"\\)\n")
            .expect("write literal escape corpus");

        let index = SearchIndex::build(&project);
        let pattern = match pattern_compile::compile(
            needle,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(CompiledPattern::Literal(pattern)) => {
                CompiledPattern::Literal(pattern)
            }
            other => panic!("expected literal pattern for {needle:?}, got {other:?}"),
        };
        let result = index.search_grep(&pattern, &[], &[], &project, 10).unwrap();

        assert_eq!(result.index_status, IndexStatus::Ready);
        assert!(
            !result.fully_degraded,
            "{needle:?} must use trigram candidates"
        );
        assert_eq!(
            result.total_matches, 1,
            "literal {needle:?} was filtered out"
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            result.matches[0].file,
            fs::canonicalize(file).expect("canonicalize corpus file")
        );
    }

    #[test]
    fn ready_index_keeps_literal_backslash_dot_candidate() {
        assert_ready_indexed_literal_match(r"foo\.bar");
    }

    thread_local! {
        static FROZEN_GREP_SCAN_START: Instant = Instant::now();
    }

    /// A scan clock that ignores real time: a fixed per-thread instant plus
    /// whatever the test has added to the injected advance.
    fn frozen_grep_scan_now() -> Instant {
        FROZEN_GREP_SCAN_START.with(|start| *start)
            + GREP_SCAN_CLOCK_ADVANCE.with(std::cell::Cell::get)
    }

    /// Resets the injected clock after a deadline test, including on panic.
    struct GrepScanClockGuard;

    impl GrepScanClockGuard {
        fn install() -> Self {
            GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(Duration::ZERO));
            Self
        }
    }

    impl Drop for GrepScanClockGuard {
        fn drop(&mut self) {
            GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(Duration::ZERO));
        }
    }

    /// Clears the line-details probe when a test ends, even on panic.
    struct LineDetailsProbeGuard;

    impl Drop for LineDetailsProbeGuard {
        fn drop(&mut self) {
            LINE_DETAILS_PROBE.with(|probe| *probe.borrow_mut() = None);
        }
    }

    fn install_line_details_probe(probe: impl FnMut() + 'static) -> LineDetailsProbeGuard {
        LINE_DETAILS_PROBE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
        LineDetailsProbeGuard
    }

    fn compile_grep_pattern(
        pattern: &str,
        literal: bool,
        case_insensitive: bool,
    ) -> CompiledPattern {
        match pattern_compile::compile(
            pattern,
            CompileOpts {
                literal,
                case_insensitive,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => compiled,
            other => panic!("compile {pattern:?}: {other:?}"),
        }
    }

    /// A real FIFO open in verification's first-access directory probe stands
    /// in for consent. Stale FIFO index entries are rejected in a separate test.
    #[cfg(unix)]
    #[test]
    fn indexed_grep_scan_deadline_bounds_blocked_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = root.join("indexed.txt");
        fs::write(&path, "needle\n").unwrap();
        let index = SearchIndex::build_with_limit(&root, 1024 * 1024);
        let fifo = root.join("consent-probe");
        crate::bounded_io::tests::fifo(&fifo);
        let probe = crate::bounded_io::tests::block_directory_probe(&root, &fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(indexed_grep_with_budget(
                &index,
                &root,
                &compile_grep_pattern("needle", true, false),
                100,
                Duration::from_millis(80),
            ))
            .unwrap();
        });
        let before_writer = rx.recv_timeout(Duration::from_millis(800));
        crate::bounded_io::tests::release_fifo(&fifo);
        thread.join().unwrap();
        let result =
            before_writer.expect("indexed verification waited in open beyond its scan deadline");
        assert!(result.matches.is_empty());
        assert_eq!(
            probe.fired.load(Ordering::Relaxed),
            1,
            "verification's boundary probe must run"
        );
        assert_eq!(result.files_searched, 0);
        assert_eq!(
            result.missing_on_disk, 0,
            "blocked access is not a deleted index entry"
        );
        assert!(result.scan_deadline_reached);
    }

    #[cfg(unix)]
    #[test]
    fn indexed_grep_never_opens_a_stale_fifo_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = root.join("indexed.txt");
        fs::write(&path, "needle\n").unwrap();
        let index = SearchIndex::build_with_limit(&root, 1024 * 1024);
        fs::remove_file(&path).unwrap();
        crate::bounded_io::tests::fifo(&path);
        let result = indexed_grep_with_budget(
            &index,
            &root,
            &compile_grep_pattern("needle", true, false),
            100,
            Duration::from_millis(800),
        );
        assert!(result.matches.is_empty());
        assert_eq!(result.files_searched, 0);
        assert!(
            !result.scan_deadline_reached,
            "a FIFO is excluded, not opened until a deadline"
        );
    }

    /// Indexed grep over `root` with an explicit scan budget, run on the
    /// calling thread when there are at most 10 candidate files.
    fn indexed_grep_with_budget(
        index: &SearchIndex,
        root: &Path,
        pattern: &CompiledPattern,
        max_results: usize,
        budget: Duration,
    ) -> GrepResult {
        indexed_grep_with_deadline(index, root, pattern, max_results, budget, || {
            GrepScanDeadline::after_with_clock(budget, Instant::now)
        })
    }

    fn indexed_grep_with_deadline(
        index: &SearchIndex,
        root: &Path,
        pattern: &CompiledPattern,
        max_results: usize,
        budget: Duration,
        make_deadline: impl FnOnce() -> GrepScanDeadline,
    ) -> GrepResult {
        let query = decompose_grep_pattern(pattern);
        index
            .snapshot()
            .search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
                pattern,
                &query,
                &PathFilters::default(),
                root,
                max_results,
                None,
                None,
                budget,
                make_deadline,
                GrepScanOrder::Arrival,
                PATH_ORDER_BATCH_ROW_BUDGET,
            )
            .unwrap()
            .0
    }

    /// A project holding one file whose first line is about 1 MiB long with
    /// 100,000 occurrences of `needle`, the first at column 7.
    fn long_line_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let mut content = String::from("start ");
        for _ in 0..100_000 {
            content.push_str("needle-pad");
        }
        content.push_str("\nlast line without the word\n");
        fs::write(project.join("minified.js"), content).expect("write long line");
        (dir, project)
    }

    #[test]
    fn long_line_with_many_occurrences_builds_line_details_once_per_arm() {
        let (_dir, project) = long_line_project();
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let arms = [
            ("literal", compile_grep_pattern("needle", true, false)),
            (
                "case-insensitive literal",
                compile_grep_pattern("NEEDLE", true, true),
            ),
            ("regex", compile_grep_pattern("ne+dle-pad", false, false)),
        ];
        match (&arms[0].1, &arms[1].1, &arms[2].1) {
            (
                CompiledPattern::Literal(sensitive),
                CompiledPattern::Literal(insensitive),
                CompiledPattern::Regex { .. },
            ) => {
                assert!(!sensitive.case_insensitive_ascii);
                assert!(insensitive.case_insensitive_ascii);
            }
            other => panic!("patterns must cover all three matcher arms: {other:?}"),
        }

        for (arm_index, (arm, pattern)) in arms.iter().enumerate() {
            let budget = Duration::from_secs(2);
            // Only the scan clock is frozen. Bounded file reads retain a real
            // deadline with enough slack for a runner pause between arms.
            let read_budget = Duration::from_secs(60);
            let _io_scope = crate::bounded_io::enter_if_absent(read_budget);
            let _clock = GrepScanClockGuard::install();
            let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let probe_calls = calls.clone();
            // The scan clock is frozen and moves only when line details are
            // built: half the budget per build. One build, the correct work,
            // never reaches the deadline however slow the runner is; a
            // regressed per-occurrence scan reaches it on its second build and
            // ends quickly, failing the count below.
            let _probe = install_line_details_probe(move || {
                probe_calls.set(probe_calls.get() + 1);
                GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(advance.get() + budget / 2));
            });
            let result = indexed_grep_with_deadline(&index, &project, pattern, 100, budget, || {
                let mut deadline = GrepScanDeadline::after_with_clock(budget, frozen_grep_scan_now);
                deadline.read_at = Instant::now().checked_add(read_budget);
                if arm_index == 1 {
                    // The second read must survive a pause longer than the
                    // scan budget while the scan clock remains frozen.
                    std::thread::sleep(Duration::from_secs(3));
                }
                deadline
            });

            assert_eq!(
                calls.get(),
                1,
                "{arm}: line details must be built once per matched line, not once per occurrence"
            );
            assert!(!result.scan_deadline_reached, "{arm}: scan hit its budget");
            assert_eq!(result.total_matches, 1, "{arm}");
            assert_eq!(result.matches.len(), 1, "{arm}");
            let matched = &result.matches[0];
            assert_eq!((matched.line, matched.column), (1, 7), "{arm}");
            assert!(
                matched.line_text.starts_with("start needle-pad"),
                "{arm}: {:?}",
                &matched.line_text[..40]
            );
            assert_eq!(
                matched.line_text,
                bounded_grep_line_text(&format!("start {}", "needle-pad".repeat(100_000))),
                "{arm}: the kept line text is the bounded form of the whole line"
            );
        }
    }

    #[test]
    fn matching_lines_in_content_builds_line_details_once_per_matched_line() {
        let mut content = String::from("start ");
        content.push_str(&"needle-pad".repeat(100_000));
        content.push_str("\nsecond needle line needle\n");
        let content = content.into_bytes();
        let patterns = [
            compile_grep_pattern("needle", true, false),
            compile_grep_pattern("NEEDLE", true, true),
            compile_grep_pattern("ne+dle", false, false),
        ];
        for pattern in &patterns {
            let matcher = match pattern {
                CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
                CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
            };
            let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let probe_calls = calls.clone();
            let _probe = install_line_details_probe(move || probe_calls.set(probe_calls.get() + 1));

            let (lines, count) =
                matching_lines_in_content(Path::new("m.js"), &content, &matcher, 10, &|_| false);

            assert_eq!(calls.get(), 2, "one line-details call per matched line");
            assert_eq!(count, 2);
            let found: Vec<(u32, u32)> =
                lines.iter().map(|line| (line.line, line.column)).collect();
            assert_eq!(found, vec![(1, 7), (2, 8)]);
        }
    }

    #[test]
    fn grep_scan_stops_at_its_deadline_across_many_large_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let body = "let value = needle_marker + 1;\n".repeat(8_000);
        for file in 0..24 {
            fs::write(project.join(format!("large_{file}.js")), &body).expect("write file");
        }
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle_marker", true, false);

        let started = Instant::now();
        let result =
            indexed_grep_with_budget(&index, &project, &pattern, 1_000_000, Duration::ZERO);

        assert!(
            result.scan_deadline_reached,
            "a spent budget must be reported by name, not as a complete result"
        );
        assert!(result.engine_capped);
        assert!(
            result.files_searched < 24,
            "no candidate file may be read after the deadline: searched {}",
            result.files_searched
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // The same scan with room to finish reads every file and is complete.
        let full = indexed_grep_with_budget(
            &index,
            &project,
            &pattern,
            1_000_000,
            INDEXED_GREP_SCAN_BUDGET,
        );
        assert!(!full.scan_deadline_reached);
        assert_eq!(full.files_searched, 24);
        assert_eq!(full.total_matches, 24 * 8_000);
    }

    #[test]
    fn grep_scan_stops_at_its_deadline_inside_one_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        fs::write(project.join("many.txt"), "needle line\n".repeat(1_000)).expect("write file");
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle", true, false);
        let budget = Duration::from_secs(60);
        let _clock = GrepScanClockGuard::install();
        // Keep real file reads well inside the budget, even on a slow runner.
        // Advancing the injected clock while the first line is rendered makes
        // the next occurrence stop the scan without relying on a real sleep.
        let mut advanced_clock = false;
        let _probe = install_line_details_probe(move || {
            if !advanced_clock {
                advanced_clock = true;
                GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(budget * 2));
            }
        });

        let result = indexed_grep_with_deadline(&index, &project, &pattern, 10_000, budget, || {
            GrepScanDeadline::after_with_clock(budget, grep_scan_test_now)
        });

        assert!(result.scan_deadline_reached);
        assert!(result.engine_capped);
        assert_eq!(
            result.matches.len(),
            1,
            "the scan must stop inside the file"
        );
    }

    #[test]
    fn grep_scan_stops_on_cancellation_inside_one_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        fs::write(project.join("many.txt"), "needle line\n".repeat(1_000)).expect("write file");
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle", true, false);
        let token = crate::executor::JobCancellation::new();
        let _job = crate::executor::install_job_cancellation(token.clone());
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let probe_calls = calls.clone();
        // Cancel the way a client does, while the scan is in the middle of
        // the file: after its third matched line.
        let _probe = install_line_details_probe(move || {
            probe_calls.set(probe_calls.get() + 1);
            if probe_calls.get() == 3 {
                token.request_cancel();
            }
        });

        let result =
            indexed_grep_with_budget(&index, &project, &pattern, 10_000, INDEXED_GREP_SCAN_BUDGET);

        assert_eq!(calls.get(), 3, "no line is examined after the cancel");
        assert_eq!(result.matches.len(), 3);
        assert!(result.engine_capped);
        assert!(!result.scan_deadline_reached);
    }

    #[test]
    fn bounded_line_text_renders_exactly_like_the_whole_line() {
        use crate::commands::grep::{truncate_grep_line, truncate_line_text, GREP_MAX_LINE_CHARS};

        let mut invalid = "é".repeat(GREP_MAX_LINE_CHARS - 1).into_bytes();
        invalid.extend_from_slice(&[0xff, 0xe2, 0x82, b'x']);
        invalid.extend_from_slice(&"tail".repeat(2_000).into_bytes());
        let lines: Vec<Vec<u8>> = vec![
            b"short line".to_vec(),
            Vec::new(),
            "a".repeat(GREP_MAX_LINE_CHARS).into_bytes(),
            "a".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "é".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "€".repeat(5_000).into_bytes(),
            "needle-pad".repeat(100_000).into_bytes(),
            invalid,
        ];
        for line in &lines {
            let whole = String::from_utf8_lossy(line).into_owned();
            let bounded = bounded_grep_line_text_bytes(line);
            assert_eq!(truncate_grep_line(&bounded), truncate_grep_line(&whole));
            assert_eq!(truncate_line_text(&bounded), truncate_line_text(&whole));
            if whole.chars().count() <= GREP_MAX_LINE_CHARS {
                assert_eq!(bounded, whole, "a line within the bound is kept whole");
            }
            assert!(bounded.chars().count() <= GREP_MAX_LINE_CHARS + 20);
        }
    }

    #[test]
    fn ready_index_keeps_literal_double_backslash_candidate() {
        assert_ready_indexed_literal_match(r"a\\b");
    }

    #[test]
    fn ready_index_keeps_literal_backslash_t_candidate() {
        assert_ready_indexed_literal_match(r"foo\tbar");
    }

    #[test]
    fn ready_index_keeps_literal_escaped_parentheses_candidate() {
        assert_ready_indexed_literal_match(r#"split\("x"\)"#);
    }

    #[test]
    fn grep_reports_total_matches_before_truncation() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex\nSearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 1).unwrap();

        assert_eq!(result.total_matches, 2);
        assert_eq!(result.matches.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn glob_filters_results_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let scripts = project.join("scripts");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&scripts).expect("create scripts dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");
        fs::write(scripts.join("tool.rs"), "pub fn tool() {}\n").expect("write scripts file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.rs", &src);

        assert_eq!(
            files,
            vec![fs::canonicalize(src.join("main.rs")).expect("canonicalize src file")]
        );
    }

    #[test]
    fn snapshot_reports_file_presence_without_a_filesystem_walk() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let empty = project.join("empty");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&empty).expect("create empty dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let snapshot = index.snapshot();

        assert!(snapshot.has_file_in_scope(&project));
        assert!(snapshot.has_file_in_scope(&src));
        assert!(!snapshot.has_file_in_scope(&empty));
    }

    #[test]
    fn glob_includes_hidden_and_binary_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let hidden_dir = project.join(".hidden");
        fs::create_dir_all(&hidden_dir).expect("create hidden dir");
        let hidden_file = hidden_dir.join("data.bin");
        fs::write(&hidden_file, [0u8, 159, 146, 150]).expect("write binary file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.bin", &project);

        assert_eq!(
            files,
            vec![fs::canonicalize(hidden_file).expect("canonicalize binary file")]
        );
    }

    #[test]
    fn ignore_rule_discovery_respects_gitignore() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join("src")).expect("mkdir src");
        fs::write(project.join("src/.gitignore"), "data/\n").expect("write gitignore");
        let data = project.join("src/data");
        fs::create_dir_all(&data).expect("mkdir data");
        for index in 0..200 {
            fs::create_dir_all(data.join(format!("d{index}"))).expect("mkdir nested");
            fs::write(data.join(format!("d{index}/f.rs")), "fn ignored() {}\n")
                .expect("write ignored file");
        }

        let mut init = Command::new("git");
        crate::test_env::apply_hermetic_git_env(init.arg("init").arg(&project))
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            git_command_for_test(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .expect("git add");
        git_command_for_test(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit");

        let legacy_dirs = count_ignore_rule_discovery_dirs_legacy_stack(&project);
        let walker_dirs = count_ignore_rule_discovery_dirs(&project);
        assert!(
            legacy_dirs > walker_dirs,
            "legacy stack should descend into gitignored data/ (legacy={legacy_dirs}, walker={walker_dirs})"
        );
        assert!(
            walker_dirs < 50,
            "ignore walker should not descend deeply into ignored tree (dirs={walker_dirs})"
        );
    }

    #[test]
    fn matching_project_walk_preserves_filtered_mtime_order() {
        let dir = tempfile::tempdir().unwrap();
        for (name, age) in [
            ("src/a/retry.rs", 0),
            ("src/b/retry.rs", 0),
            ("src/c/retry.rs", 1),
            ("tests/retry.rs", 2),
            ("src/unrelated.rs", 3),
        ] {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "pub fn fixture() {}\n").unwrap();
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(1_700_000_000 + age, 0),
            )
            .unwrap();
        }
        for suffix in ["retry.rs", "a/retry.rs", "missing.rs"] {
            for include_tests in [false, true] {
                let matches = |path: &Path| {
                    let relative = path.strip_prefix(dir.path()).unwrap();
                    (include_tests || !relative.starts_with("tests"))
                        && relative
                            .to_string_lossy()
                            .replace('\\', "/")
                            .ends_with(suffix)
                };
                let expected =
                    walk_project_files_from(dir.path(), dir.path(), &PathFilters::default())
                        .into_iter()
                        .filter(|path| matches(path))
                        .collect::<Vec<_>>();
                let actual = walk_project_files_from_matching(
                    dir.path(),
                    dir.path(),
                    &PathFilters::default(),
                    matches,
                );
                assert_eq!(
                    actual, expected,
                    "suffix={suffix}, include_tests={include_tests}"
                );
            }
        }
    }

    #[test]
    fn sort_paths_by_mtime_desc_uses_root_relative_tiebreak() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let mut paths = ["z-last.rs", "a-first.rs", "m-middle.rs"]
            .map(|name| {
                let path = dir.path().join(name);
                fs::write(&path, format!("// {name}\n")).expect("write fixture");
                filetime::set_file_mtime(&path, tied_mtime).expect("pin fixture mtime");
                path
            })
            .to_vec();

        sort_paths_by_mtime_desc(&mut paths, dir.path());

        let expected = ["a-first.rs", "m-middle.rs", "z-last.rs"]
            .map(|name| dir.path().join(name))
            .to_vec();
        assert_eq!(paths, expected);
    }

    #[test]
    fn walked_path_sort_matches_canonical_sort_for_regular_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths = Vec::new();
        for index in (0..40).rev() {
            let path = dir.path().join(format!("module-{index:02}.ts"));
            fs::write(&path, format!("export const value = {index};\n")).expect("write fixture");
            let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + i64::from(index % 4), 0);
            filetime::set_file_mtime(&path, mtime).expect("pin fixture mtime");
            paths.push(path);
        }

        let mut canonicalized = paths.clone();
        sort_paths_by_mtime_desc(&mut canonicalized, dir.path());
        sort_walked_paths_by_mtime_desc(&mut paths, dir.path());

        assert_eq!(paths, canonicalized);
    }

    /// Manual release-mode probe for the sort paid by one fallback glob walk.
    #[test]
    #[ignore = "manual release-mode fallback glob sort performance probe"]
    fn walked_path_sort_perf_probe() {
        const DIRECTORIES: usize = 100;
        const FILES_PER_DIRECTORY: usize = 100;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 3;

        let temp = tempfile::tempdir().expect("create tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let mut paths = Vec::with_capacity(DIRECTORIES * FILES_PER_DIRECTORY);
        for directory in 0..DIRECTORIES {
            let path = temp.path().join(format!("package-{directory:03}/src"));
            fs::create_dir_all(&path).expect("create package directory");
            for file in 0..FILES_PER_DIRECTORY {
                let file_path = path.join(format!("module-{file:03}.ts"));
                fs::write(&file_path, b"export const value = 1;\n").expect("write fixture");
                filetime::set_file_mtime(&file_path, tied_mtime).expect("pin fixture mtime");
                paths.push(file_path);
            }
        }

        let mut micros_per_operation = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                let mut sample = paths.clone();
                sort_walked_paths_by_mtime_desc(&mut sample, temp.path());
                std::hint::black_box(sample);
            }
            micros_per_operation.push(started.elapsed().as_micros() / ITERATIONS as u128);
        }
        micros_per_operation.sort_unstable();
        let median = micros_per_operation[SAMPLES / 2];

        eprintln!(
            "fallback glob sort: files={} samples={SAMPLES} iterations={ITERATIONS}",
            paths.len()
        );
        eprintln!("microseconds per glob operation: {micros_per_operation:?}");
        eprintln!("median: {median}us per glob operation");
    }

    #[cfg(windows)]
    #[test]
    fn sort_paths_by_mtime_desc_normalizes_comparison_paths_without_rewriting_results() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let canonical_root = fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let canonical_first = canonical_root.join("a-first.rs");
        let clean_last = dir.path().join("z-last.rs");
        for path in [&canonical_first, &clean_last] {
            fs::write(path, "// tied\n").expect("write fixture");
            filetime::set_file_mtime(path, tied_mtime).expect("pin fixture mtime");
        }
        let mut paths = vec![clean_last.clone(), canonical_first.clone()];

        sort_paths_by_mtime_desc(&mut paths, &canonical_root);

        assert_eq!(paths, vec![canonical_first, clean_last]);
    }

    /// Regression: v0.15.2 — sort_paths_by_mtime_desc panicked when files
    /// changed between cmp() calls.
    ///
    /// Pre-fix, the sort closure called `path_modified_time(path)` directly,
    /// which does a `stat()` syscall. If the file was deleted, modified, or
    /// touched mid-sort, the comparator returned different values for the
    /// same input pair on different invocations. Rust's slice::sort detects
    /// this and panics with "user-provided comparison function does not
    /// correctly implement a total order".
    ///
    /// CI hit this on a Pi e2e test (workflow run 24887807972) where the
    /// bridge invalidated files in parallel with grep's sort path. This
    /// test simulates the worst case: most paths don't exist (Err from
    /// fs::metadata) and sort still completes successfully.
    #[test]
    fn sort_paths_by_mtime_desc_does_not_panic_on_missing_files() {
        // Mix of existing and non-existing paths in deliberately
        // non-monotonic order — pre-fix, the sort would call stat() at
        // least N log N times and any flakiness would trigger the panic.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..30 {
            // Half exist, half don't.
            let path = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            paths.push(path);
        }

        // Run the sort many times to maximise the chance of catching any
        // residual non-determinism. Pre-fix: panic. Post-fix: stable.
        for _ in 0..50 {
            let mut copy = paths.clone();
            sort_paths_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), paths.len());
        }
    }

    /// Regression: the indexed parallel search's reduce() combine closure must
    /// NOT set engine_capped. reduce runs on every partial-result merge in a
    /// multi-chunk parallel search (>10 candidate files), capped or not — an
    /// unconditional store there falsely reported every such grep as capped,
    /// lying to the agent that results were truncated.
    #[test]
    fn uncapped_indexed_grep_over_many_files_is_not_engine_capped() {
        let dir = tempfile::tempdir().expect("create tempdir");
        // >10 files so the parallel (reduce) branch is taken, each with exactly
        // one match, and a generous cap so the search is NOT actually capped.
        for i in 0..40 {
            fs::write(
                dir.path().join(format!("file-{i}.rs")),
                format!("fn unique_marker_{i}() {{ let _ = \"needle_token\"; }}\n"),
            )
            .expect("write");
        }
        let index = SearchIndex::build_with_limit(dir.path(), DEFAULT_MAX_FILE_SIZE);
        let result = index
            .grep("needle_token", false, &[], &[], dir.path(), 1000)
            .unwrap();
        assert!(
            result.matches.len() >= 40,
            "expected a match per file, got {}",
            result.matches.len()
        );
        assert!(
            !result.engine_capped,
            "an uncapped grep over >10 files must not report engine_capped"
        );
        assert!(!result.truncated, "uncapped grep must not be truncated");
    }

    /// Regression: v0.15.2 — sort_grep_matches_by_mtime_desc panicked under
    /// the same conditions as sort_paths_by_mtime_desc. See the
    /// sort_paths_... test above for the full rationale.
    #[test]
    fn sort_grep_matches_by_mtime_desc_does_not_panic_on_missing_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut matches: Vec<GrepMatch> = Vec::new();
        for i in 0..30 {
            let file = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            matches.push(GrepMatch {
                file,
                line: u32::try_from(i).unwrap_or(0),
                column: 0,
                line_text: format!("match {i}"),
                match_text: format!("match {i}"),
            });
        }

        for _ in 0..50 {
            let mut copy = matches.clone();
            sort_grep_matches_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), matches.len());
        }
    }

    #[test]
    fn out_of_order_delta_refresh_matches_full_sort_reference() {
        const FILES: u32 = 1_024;
        let shared_trigram = pack_trigram(b's', b'h', b'r');
        let mut optimized = Vec::new();
        let mut reference = Vec::new();

        for file_id in (0..FILES).rev() {
            let posting = Posting {
                file_id,
                next_mask: 0,
                loc_mask: 0,
            };
            insert_delta_posting(&mut optimized, posting.clone());
            insert_delta_posting_full_sort_reference(&mut reference, posting);
        }

        assert_eq!(
            optimized, reference,
            "delta postings must stay file-id sorted"
        );

        let mut index = SearchIndex::new();
        let files = Arc::make_mut(&mut index.files);
        for file_id in 0..FILES {
            files.push(FileEntry {
                path: PathBuf::from(format!("/delta/file-{file_id:04}.rs")),
                size: 0,
                modified: UNIX_EPOCH,
                content_hash: cache_freshness::zero_hash(),
            });
        }
        Arc::make_mut(&mut index.delta)
            .postings
            .insert(shared_trigram, optimized);

        let actual = index
            .candidates(&RegexQuery {
                and_trigrams: vec![shared_trigram],
                ..RegexQuery::default()
            })
            .unwrap();
        let expected = reference
            .into_iter()
            .map(|posting| posting.file_id)
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_vec(&actual).expect("serialize candidates"),
            serde_json::to_vec(&expected).expect("serialize reference candidates"),
            "candidate IDs must match the full-sort reference byte-for-byte"
        );
    }

    #[test]
    #[ignore = "manual release-mode issue #219 delta insertion performance probe"]
    fn issue_219_delta_insertion_perf_probe() {
        const FILES: u32 = 1_024;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 8;

        let reference_once = || {
            let mut postings = Vec::with_capacity(FILES as usize);
            for file_id in (0..FILES).rev() {
                insert_delta_posting_full_sort_reference(
                    &mut postings,
                    Posting {
                        file_id,
                        next_mask: 0,
                        loc_mask: 0,
                    },
                );
            }
            std::hint::black_box(postings);
        };
        let optimized_once = || {
            let mut postings = Vec::with_capacity(FILES as usize);
            for file_id in (0..FILES).rev() {
                insert_delta_posting(
                    &mut postings,
                    Posting {
                        file_id,
                        next_mask: 0,
                        loc_mask: 0,
                    },
                );
            }
            std::hint::black_box(postings);
        };

        let mut reference_ns = Vec::with_capacity(SAMPLES);
        let mut optimized_ns = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let measure = |operation: &dyn Fn()| {
                let started = Instant::now();
                for _ in 0..ITERATIONS {
                    operation();
                }
                started.elapsed().as_nanos() / ITERATIONS as u128
            };
            if sample % 2 == 0 {
                reference_ns.push(measure(&reference_once));
                optimized_ns.push(measure(&optimized_once));
            } else {
                optimized_ns.push(measure(&optimized_once));
                reference_ns.push(measure(&reference_once));
            }
        }
        reference_ns.sort_unstable();
        optimized_ns.sort_unstable();
        let reference_median = reference_ns[SAMPLES / 2];
        let optimized_median = optimized_ns[SAMPLES / 2];
        let speedup = reference_median as f64 / optimized_median as f64;

        eprintln!(
            "issue #219 delta insertion: files={FILES} samples={SAMPLES} iterations={ITERATIONS}"
        );
        eprintln!("full-sort ns/refresh samples: {reference_ns:?}");
        eprintln!("binary-insert ns/refresh samples: {optimized_ns:?}");
        eprintln!(
            "median: full-sort={reference_median}ns binary-insert={optimized_median}ns speedup={speedup:.2}x"
        );
    }
}
#[cfg(test)]
mod interactive_artifact_read_budget_tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn contended_read_returns_within_interactive_budget() {
        let lock = Arc::new(RwLock::new(()));
        let writer = lock.write().expect("acquire test writer");
        let reader_lock = Arc::clone(&lock);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let started = Instant::now();
            let result = try_read_with_budget(&reader_lock, Duration::from_millis(20));
            result_tx
                .send((result.is_some(), started.elapsed()))
                .unwrap();
        });

        // Keep the writer live until the reader reports degradation. A delayed
        // reader must still encounter contention, not acquire a released lock.
        let reported = result_rx.recv_timeout(Duration::from_secs(30));
        drop(writer);
        reader.join().expect("join bounded reader");
        let (acquired, elapsed) =
            reported.expect("bounded reader must return while the writer is held");
        assert!(!acquired, "a live writer must force bounded degradation");
        // The read itself must honour its 20 ms budget. The bound is 100 times
        // the budget so runner scheduling noise cannot trip it, while a read
        // that ignored the budget and waited on the writer (held up to 30 s)
        // still fails.
        assert!(
            elapsed < Duration::from_secs(2),
            "contended read ignored its 20 ms budget: {elapsed:?}"
        );
    }
}

/// The search index, its glob, and the project walker it shares with the
/// semantic collector and views membership apply `.gitignore` files the same
/// way in a plain folder as in a git repository.
#[cfg(test)]
mod project_ignore_rule_tests {}
