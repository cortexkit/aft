use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[cfg(test)]
static VARIABLE_SPAN_REGEX_COMPILATIONS: AtomicUsize = AtomicUsize::new(0);

use super::comparator::{score_free_r3_cmp, CandidateResult};
use super::evidence_descriptor::EvidenceDescriptor;
use super::plan_table::SearchLaneKind;
use super::{LaneExecution, LaneInput, SearchLane};
use crate::search_index::{decompose_regex, SearchIndex};

/// Maximum occurrences considered per run, applied in ascending-offset order.
pub const MAX_RUN_OCCURRENCES: usize = 64;

/// A verbatim occurrence of a retained run within source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Occurrence {
    /// 1-based index of the retained run in query order.
    pub run_idx: usize,
    /// Start byte offset in the source file.
    pub start: usize,
    /// End byte offset in the source file.
    pub end: usize,
    /// Byte length of the retained run contributing to matched evidence.
    pub len: usize,
}

/// A candidate alignment mapping a subset of retained runs to pairwise non-overlapping occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alignment {
    pub occurrences: Vec<Occurrence>,
    pub matched_total: usize,
    pub gap_chars: usize,
}

impl Alignment {
    pub fn single(occ: Occurrence) -> Self {
        Self {
            occurrences: vec![occ],
            matched_total: occ.len,
            gap_chars: 0,
        }
    }

    pub fn extended_with(&self, occ: Occurrence) -> Self {
        let mut occs = self.occurrences.clone();
        let last = occs.last().expect("alignment has at least one occurrence");
        assert!(
            last.end <= occ.start,
            "occurrences must be pairwise non-overlapping"
        );
        assert!(
            last.run_idx < occ.run_idx,
            "run indices must be strictly increasing"
        );
        let added_gap = occ.start - last.end;
        occs.push(occ);
        Self {
            occurrences: occs,
            matched_total: self.matched_total + occ.len,
            gap_chars: self.gap_chars + added_gap,
        }
    }

    pub fn runs_matched(&self) -> usize {
        self.occurrences.len()
    }

    pub fn first_start_offset(&self) -> usize {
        self.occurrences.first().map(|o| o.start).unwrap_or(0)
    }

    pub fn last_end_offset(&self) -> usize {
        self.occurrences.last().map(|o| o.end).unwrap_or(0)
    }

    /// Occurrence list: sequence of (run_idx, start_offset) pairs in query order.
    pub fn occurrence_list(&self) -> Vec<(usize, usize)> {
        self.occurrences
            .iter()
            .map(|o| (o.run_idx, o.start))
            .collect()
    }
}

/// Compare two candidate alignments under the normative total order:
/// (alpha) most runs matched
/// (beta) largest matched-run character total
/// (gamma) fewest gap characters
/// (delta) smallest start offset of first occurrence
/// (epsilon) smallest end offset of last occurrence
/// (zeta) lexicographically smallest occurrence list
pub fn cmp_alignment(a: &Alignment, b: &Alignment) -> Ordering {
    // Ordering::Greater consistently means that `a` is the better alignment.
    a.runs_matched()
        .cmp(&b.runs_matched())
        .then_with(|| a.matched_total.cmp(&b.matched_total))
        .then_with(|| b.gap_chars.cmp(&a.gap_chars))
        .then_with(|| b.first_start_offset().cmp(&a.first_start_offset()))
        .then_with(|| b.last_end_offset().cmp(&a.last_end_offset()))
        .then_with(|| b.occurrence_list().cmp(&a.occurrence_list()))
}

/// Whether a relative path indicates a test file according to AFT conventions.
pub fn is_test_path(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    if normalized
        .split('/')
        .any(|segment| matches!(segment, "__tests__" | "__test__" | "tests"))
    {
        return true;
    }
    let file = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = file.to_ascii_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }
    lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rb")
        || lower.ends_with("_test.exs")
        || lower.ends_with("_spec.rb")
        || (lower.starts_with("test_") && lower.ends_with(".py"))
}

/// Check if string contains at least one alphabetic word of 3+ letters.
pub fn has_alphabetic_word_of_3_plus_letters(s: &str) -> bool {
    let mut consecutive_alpha = 0;
    for c in s.chars() {
        if c.is_alphabetic() {
            consecutive_alpha += 1;
            if consecutive_alpha >= 3 {
                return true;
            }
        } else {
            consecutive_alpha = 0;
        }
    }
    false
}

fn retain_literal_run(raw: &str, retained_runs: &mut Vec<String>) {
    let trimmed = raw.trim();
    if trimmed.len() >= 4 && has_alphabetic_word_of_3_plus_letters(trimmed) {
        retained_runs.push(trimmed.to_string());
    }
}

/// Result of splitting a query into retained literal runs and denominator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySplitResult {
    pub retained_runs: Vec<String>,
    pub retained_run_lengths: Vec<usize>,
    pub denominator: usize,
}

fn compile_variable_span_pattern() -> Regex {
    #[cfg(test)]
    VARIABLE_SPAN_REGEX_COMPILATIONS.fetch_add(1, AtomicOrdering::Relaxed);
    Regex::new(
        r##"(?x)
        \b(?i:INFO|WARN|ERROR|DEBUG|TRACE|FATAL|panicked)\b
        | \[\d+\]
        | \b\d{4}-\d{2}-\d{2}(?:[T\s]\d{2}:\d{2}:\d{2}(?:\.\d+)?)?\b
        | \b\d{2}:\d{2}:\d{2}(?:\.\d+)?\b
        | (?:[a-zA-Z]:)?[/\\][\w.\-/\\]+|[\w.\-]+(?:[/\\][\w.\-]+)+
        | "[^"]*"|'[^']*'
        | \{\}|%[sd]|\$\{[^}]*\}|\{[a-zA-Z0-9_]+\}
        | \b0x[0-9a-fA-F]+\b
        | \b[0-9a-fA-F]{6,}\b
        | \b\d{2,}\b
        "##,
    )
    .expect("compile anchored variable-span regex")
}

static VARIABLE_SPAN_PATTERN: LazyLock<Regex> = LazyLock::new(compile_variable_span_pattern);

pub fn split_query(query: &str) -> QuerySplitResult {
    // These spans are values supplied when a format string is rendered. Keeping
    // them would charge evidence for text that cannot occur in the source.
    let mut retained_runs = Vec::new();
    let mut last_end = 0;
    for span in VARIABLE_SPAN_PATTERN.find_iter(query) {
        retain_literal_run(&query[last_end..span.start()], &mut retained_runs);
        last_end = span.end();
    }
    retain_literal_run(&query[last_end..], &mut retained_runs);

    let retained_run_lengths = retained_runs.iter().map(String::len).collect::<Vec<_>>();
    let denominator = retained_run_lengths.iter().sum();

    QuerySplitResult {
        retained_runs,
        retained_run_lengths,
        denominator,
    }
}

pub fn find_run_occurrences(text: &str, run: &str, run_idx: usize) -> Vec<Occurrence> {
    if run.is_empty() || text.is_empty() {
        return Vec::new();
    }

    let matcher = compile_run_matcher(run);
    find_occurrences_with_matcher(text, &matcher, run.len(), run_idx)
}

fn compile_run_matcher(run: &str) -> Regex {
    #[cfg(test)]
    crate::search_hot_path_measurements::record(|counts| counts.regex_compilations += 1);
    regex::RegexBuilder::new(&regex::escape(run))
        .case_insensitive(true)
        .build()
        .expect("escaped retained run must compile")
}

fn find_occurrences_with_matcher(
    text: &str,
    matcher: &Regex,
    run_len: usize,
    run_idx: usize,
) -> Vec<Occurrence> {
    let mut occurrences = Vec::new();
    let mut search_start = 0;

    while search_start <= text.len() {
        let Some(found) = matcher.find_at(text, search_start) else {
            break;
        };
        occurrences.push(Occurrence {
            run_idx,
            start: found.start(),
            end: found.end(),
            len: run_len,
        });
        if occurrences.len() == MAX_RUN_OCCURRENCES {
            break;
        }

        // Advancing from the start, rather than the end, retains overlapping
        // occurrences that can participate in a later non-overlapping alignment.
        search_start = found.start()
            + text[found.start()..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
    }

    occurrences
}

/// Test-only injection for proving that canonical selection does not depend on
/// occurrence enumeration or anchor-window scan order.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AlignmentTestOrder {
    pub reverse_window_scan: bool,
    pub reverse_occurrence_enumeration: bool,
}

pub fn find_canonical_alignment(text: &str, retained_runs: &[String]) -> Option<Alignment> {
    if retained_runs.is_empty() || text.is_empty() {
        return None;
    }

    let run_occurrences = collect_run_occurrences(text, retained_runs);
    let windows = anchor_windows(text);
    canonical_alignment_from_enumerated(&run_occurrences, &windows)
}

#[doc(hidden)]
pub fn find_canonical_alignment_with_test_order(
    text: &str,
    retained_runs: &[String],
    order: AlignmentTestOrder,
) -> Option<Alignment> {
    if retained_runs.is_empty() || text.is_empty() {
        return None;
    }

    let mut run_occurrences = collect_run_occurrences(text, retained_runs);
    if order.reverse_occurrence_enumeration {
        for occurrences in &mut run_occurrences {
            occurrences.reverse();
        }
    }

    let mut windows = anchor_windows(text);
    if order.reverse_window_scan {
        windows.reverse();
    }
    canonical_alignment_from_enumerated(&run_occurrences, &windows)
}

// Matchers belong to one request, not to candidate files or a global cache.
struct CompiledRuns {
    matchers: Vec<Option<(usize, Regex)>>,
}

impl CompiledRuns {
    fn new(runs: &[String]) -> Self {
        Self {
            matchers: runs
                .iter()
                .map(|run| (!run.is_empty()).then(|| (run.len(), compile_run_matcher(run))))
                .collect(),
        }
    }

    fn alignment(&self, text: &str) -> Option<Alignment> {
        if text.is_empty() || self.matchers.is_empty() {
            return None;
        }
        let occurrences = self
            .matchers
            .iter()
            .enumerate()
            .map(|(index, compiled)| {
                compiled
                    .as_ref()
                    .map(|(len, matcher)| {
                        find_occurrences_with_matcher(text, matcher, *len, index + 1)
                    })
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let windows = occupied_anchor_windows(&anchor_windows(text), &occurrences);
        canonical_alignment_from_enumerated(&occurrences, &windows)
    }
}

fn collect_run_occurrences(text: &str, retained_runs: &[String]) -> Vec<Vec<Occurrence>> {
    retained_runs
        .iter()
        .enumerate()
        .map(|(idx, run)| find_run_occurrences(text, run, idx + 1))
        .collect()
}

fn anchor_windows(text: &str) -> Vec<(usize, usize)> {
    let mut line_starts = vec![0];
    let mut line_ends = Vec::new();
    for (offset, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            line_ends.push(offset + 1);
            line_starts.push(offset + 1);
        }
    }
    while line_ends.len() < line_starts.len() {
        line_ends.push(text.len());
    }

    (0..line_starts.len())
        .map(|first_line| {
            let last_line = (first_line + 2).min(line_starts.len() - 1);
            (line_starts[first_line], line_ends[last_line])
        })
        .collect()
}

/// Empty windows cannot produce an alignment. Each occurrence can belong to at
/// most three consecutive-line windows, so evaluate their union rather than
/// allocating filtered occurrence vectors for every source line.
fn occupied_anchor_windows(
    windows: &[(usize, usize)],
    occurrences: &[Vec<Occurrence>],
) -> Vec<(usize, usize)> {
    let mut occupied = BTreeSet::new();
    for occurrence in occurrences.iter().flatten() {
        let first = windows.partition_point(|(_, end)| *end < occurrence.end);
        let last = windows.partition_point(|(start, _)| *start <= occurrence.start);
        occupied.extend(first..last);
    }
    occupied.into_iter().map(|index| windows[index]).collect()
}

fn canonical_alignment_from_enumerated(
    run_occurrences: &[Vec<Occurrence>],
    windows: &[(usize, usize)],
) -> Option<Alignment> {
    let mut best_overall = None;

    for &(window_start, window_end) in windows {
        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| counts.anchor_windows_evaluated += 1);
        let window_runs = run_occurrences
            .iter()
            .map(|occurrences| {
                occurrences
                    .iter()
                    .copied()
                    .filter(|occ| occ.start >= window_start && occ.end <= window_end)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        if let Some(candidate) = best_alignment_in_window(&window_runs) {
            if best_overall
                .as_ref()
                .is_none_or(|best| cmp_alignment(&candidate, best) == Ordering::Greater)
            {
                best_overall = Some(candidate);
            }
        }
    }

    best_overall
}

fn best_alignment_in_window(window_runs: &[Vec<Occurrence>]) -> Option<Alignment> {
    // One state is retained for each occurrence. For one run, predecessor
    // states are swept by end offset and summarized by the exact ordering that
    // applies when all of them are extended by the same current occurrence.
    // This keeps the dynamic program O(runs x occurrences) rather than trying
    // every occurrence pair.
    let mut previous_states = Vec::<Alignment>::new();

    for current_occurrences in window_runs {
        let mut predecessor_order = (0..previous_states.len()).collect::<Vec<_>>();
        predecessor_order.sort_by_key(|&idx| previous_states[idx].last_end_offset());

        let mut current_order = (0..current_occurrences.len()).collect::<Vec<_>>();
        current_order.sort_by_key(|&idx| current_occurrences[idx].start);

        let mut predecessor_cursor = 0;
        let mut best_predecessor: Option<usize> = None;
        let mut current_states = vec![None; current_occurrences.len()];

        for current_idx in current_order {
            let current = current_occurrences[current_idx];
            while predecessor_cursor < predecessor_order.len()
                && previous_states[predecessor_order[predecessor_cursor]].last_end_offset()
                    <= current.start
            {
                let candidate_idx = predecessor_order[predecessor_cursor];
                if best_predecessor.is_none_or(|best_idx| {
                    cmp_predecessor_for_extension(
                        &previous_states[candidate_idx],
                        &previous_states[best_idx],
                    ) == Ordering::Greater
                }) {
                    best_predecessor = Some(candidate_idx);
                }
                predecessor_cursor += 1;
            }

            let mut best = Alignment::single(current);
            if let Some(predecessor_idx) = best_predecessor {
                let extended = previous_states[predecessor_idx].extended_with(current);
                if cmp_alignment(&extended, &best) == Ordering::Greater {
                    best = extended;
                }
            }
            current_states[current_idx] = Some(best);
        }

        previous_states.extend(current_states.into_iter().flatten());
    }

    previous_states
        .into_iter()
        .max_by(|a, b| cmp_alignment(a, b))
}

fn cmp_predecessor_for_extension(a: &Alignment, b: &Alignment) -> Ordering {
    let a_adjusted_gap = a.gap_chars as i128 - a.last_end_offset() as i128;
    let b_adjusted_gap = b.gap_chars as i128 - b.last_end_offset() as i128;

    a.runs_matched()
        .cmp(&b.runs_matched())
        .then_with(|| a.matched_total.cmp(&b.matched_total))
        .then_with(|| b_adjusted_gap.cmp(&a_adjusted_gap))
        .then_with(|| b.first_start_offset().cmp(&a.first_start_offset()))
        .then_with(|| b.occurrence_list().cmp(&a.occurrence_list()))
}

/// Evaluate 60%/2-runs-or-12-chars threshold conjunction.
///
/// Minimum evidence:
/// matched run characters >= 60% of retained-run character total AND
/// (2+ runs matched in order, or one run of 12+ chars).
pub fn evaluate_threshold(
    matched_run_char_total: usize,
    runs_matched: usize,
    denominator: usize,
) -> bool {
    if denominator == 0 {
        return false;
    }

    let ratio_ok = (matched_run_char_total as u128) * 5 >= (denominator as u128) * 3;
    let structure_ok = runs_matched >= 2 || (runs_matched == 1 && matched_run_char_total >= 12);

    ratio_ok && structure_ok
}

/// Detailed verification result for an anchored query against a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchoredVerification {
    pub retained_runs: Vec<String>,
    pub retained_run_lengths: Vec<usize>,
    pub matched_occurrences: Vec<(usize, usize)>,
    pub matched_total: usize,
    pub denominator: usize,
    pub gap_chars: usize,
    pub threshold_outcome: bool,
}

pub fn verify_anchored_text(query: &str, source_text: &str) -> AnchoredVerification {
    let split = split_query(query);
    let canonical = find_canonical_alignment(source_text, &split.retained_runs);

    let (matched_occurrences, matched_total, gap_chars, runs_matched) = match canonical {
        Some(ref alignment) => (
            alignment.occurrence_list(),
            alignment.matched_total,
            alignment.gap_chars,
            alignment.runs_matched(),
        ),
        None => (Vec::new(), 0, 0, 0),
    };

    AnchoredVerification {
        retained_runs: split.retained_runs,
        retained_run_lengths: split.retained_run_lengths,
        matched_occurrences,
        matched_total,
        denominator: split.denominator,
        gap_chars,
        threshold_outcome: evaluate_threshold(matched_total, runs_matched, split.denominator),
    }
}

pub fn discover_anchored_candidate_files(
    index: &SearchIndex,
    search_root: &Path,
    retained_runs: &[String],
    include_tests: bool,
) -> Vec<PathBuf> {
    if retained_runs.is_empty() {
        return Vec::new();
    }

    let mut candidate_file_ids = BTreeSet::new();
    for run in retained_runs {
        let run_query = decompose_regex(&format!("(?i:{})", regex::escape(run)));
        candidate_file_ids.extend(index.candidates(&run_query));
    }

    let canonical_root =
        fs::canonicalize(search_root).unwrap_or_else(|_| search_root.to_path_buf());
    candidate_file_ids
        .into_iter()
        .filter_map(|file_id| index.files.get(file_id as usize))
        .filter(|file| !file.path.as_os_str().is_empty())
        .filter(|file| {
            let canonical_path =
                fs::canonicalize(&file.path).unwrap_or_else(|_| file.path.to_path_buf());
            canonical_path.starts_with(&canonical_root)
        })
        .filter(|file| include_tests || !is_test_path(&file.path.to_string_lossy()))
        .map(|file| file.path.clone())
        .collect()
}

pub fn verify_file_for_anchored(
    file_path: &Path,
    retained_runs: &[String],
    denominator: usize,
) -> Option<CandidateResult> {
    verify_file_with_alignment(file_path, denominator, |content| {
        find_canonical_alignment(content, retained_runs)
    })
    .ok()
    .flatten()
}

fn verify_file_with_alignment(
    file_path: &Path,
    denominator: usize,
    align: impl FnOnce(&str) -> Option<Alignment>,
) -> Result<Option<CandidateResult>, ()> {
    let crate::search_index::SearchCorpusEligibility::Eligible(file) =
        crate::search_index::read_search_corpus_file(
            file_path,
            crate::search_index::DEFAULT_MAX_FILE_SIZE,
        )
    else {
        return Err(());
    };
    let Ok(content) = std::str::from_utf8(&file.bytes) else {
        return Err(());
    };
    let Some(canonical) = align(content) else {
        return Ok(None);
    };
    if !evaluate_threshold(
        canonical.matched_total,
        canonical.runs_matched(),
        denominator,
    ) {
        return Ok(None);
    }

    let descriptor =
        EvidenceDescriptor::for_anchored(canonical.matched_total, canonical.gap_chars, true, false);
    Ok(Some(CandidateResult::new_exact(
        file_path.to_path_buf(),
        None,
        descriptor,
    )))
}

/// Anchored lane implementation of SearchLane.
pub fn sort_anchored_canonical(results: &mut [CandidateResult]) {
    results.sort_by(score_free_r3_cmp);
}

pub struct AnchoredLane;

impl Default for AnchoredLane {
    fn default() -> Self {
        Self::new()
    }
}

impl AnchoredLane {
    pub fn new() -> Self {
        Self
    }

    /// Search an already initialized trigram index and return verified anchored candidates.
    pub fn execute_ready_mode(
        &self,
        index: &SearchIndex,
        search_root: &Path,
        query: &str,
        include_tests: bool,
    ) -> Vec<CandidateResult> {
        self.execute_with_admission_report(index, search_root, query, include_tests)
            .0
    }

    pub(crate) fn execute_with_admission_report(
        &self,
        index: &SearchIndex,
        search_root: &Path,
        query: &str,
        include_tests: bool,
    ) -> (Vec<CandidateResult>, usize, usize) {
        let split = split_query(query);
        if split.retained_runs.is_empty() {
            return (Vec::new(), 0, 0);
        }

        let candidate_files = discover_anchored_candidate_files(
            index,
            search_root,
            &split.retained_runs,
            include_tests,
        );
        let examined = candidate_files.len();
        if candidate_files.is_empty() {
            return (Vec::new(), 0, 0);
        }
        let compiled = CompiledRuns::new(&split.retained_runs);
        let mut excluded = 0;
        let mut results = candidate_files
            .into_iter()
            .filter_map(|path| {
                match verify_file_with_alignment(&path, split.denominator, |content| {
                    compiled.alignment(content)
                }) {
                    Ok(candidate) => candidate,
                    Err(()) => {
                        excluded += 1;
                        None
                    }
                }
            })
            .collect::<Vec<_>>();

        sort_anchored_canonical(&mut results);
        (results, examined, excluded)
    }
}

impl SearchLane for AnchoredLane {
    fn kind(&self) -> SearchLaneKind {
        SearchLaneKind::Anchored
    }

    fn execute(&self, input: &LaneInput<'_>) -> LaneExecution {
        LaneExecution {
            kind: self.kind(),
            candidates: self.execute_ready_mode(
                input.index,
                input.root,
                input.query,
                input.include_tests,
            ),
        }
    }
}

#[cfg(test)]
mod hot_path_tests {
    use super::*;

    #[test]
    fn audit_growth_repeated_query_splitting_compiles_the_fixed_pattern_once() {
        let before = VARIABLE_SPAN_REGEX_COMPILATIONS.load(AtomicOrdering::Relaxed);
        let first = split_query("AlphaSymbol {} BetaSymbol");
        let second = split_query("GammaSymbol {} DeltaSymbol");
        assert_eq!(
            first,
            QuerySplitResult {
                retained_runs: vec!["AlphaSymbol".to_owned(), "BetaSymbol".to_owned()],
                retained_run_lengths: vec![11, 10],
                denominator: 21,
            }
        );
        assert_eq!(
            second,
            QuerySplitResult {
                retained_runs: vec!["GammaSymbol".to_owned(), "DeltaSymbol".to_owned()],
                retained_run_lengths: vec![11, 11],
                denominator: 22,
            }
        );
        let compilations = VARIABLE_SPAN_REGEX_COMPILATIONS.load(AtomicOrdering::Relaxed) - before;
        assert_eq!(compilations, 1, "fixed split pattern should compile once");
    }

    #[test]
    fn anchored_regex_compilations_scale_with_runs_not_files() {
        let temp = tempfile::tempdir().unwrap();
        let query = "AlphaSymbol {} BetaSymbol";
        let split = split_query(query);
        assert_eq!(split.retained_runs.len(), 2);
        let mut index = SearchIndex::new();
        let mut paths = Vec::new();
        for ordinal in 0..20 {
            let path = temp.path().join(format!("file_{ordinal}.rs"));
            let content = "AlphaSymbol BetaSymbol\n";
            fs::write(&path, content).unwrap();
            index.index_file(&path, content.as_bytes());
            paths.push(path);
            if ordinal == 0 || ordinal == 19 {
                crate::search_hot_path_measurements::reset();
                let actual =
                    AnchoredLane::new().execute_ready_mode(&index, temp.path(), query, true);
                assert_eq!(
                    crate::search_hot_path_measurements::counts().regex_compilations,
                    split.retained_runs.len()
                );
                assert_eq!(actual.len(), paths.len());
                let mut reference = paths
                    .iter()
                    .filter_map(|path| {
                        verify_file_for_anchored(path, &split.retained_runs, split.denominator)
                    })
                    .collect::<Vec<_>>();
                sort_anchored_canonical(&mut reference);
                assert_eq!(
                    serde_json::to_vec(&actual).unwrap(),
                    serde_json::to_vec(&reference).unwrap()
                );
            }
        }
    }

    #[test]
    fn occupied_window_selection_matches_full_scan_across_dense_and_sparse_lines() {
        let runs = vec![
            "AlphaSymbol".to_string(),
            "BetaSymbol".to_string(),
            "GammaSymbol".to_string(),
        ];
        let compiled = CompiledRuns::new(&runs);
        for seed in 0u64..100 {
            let mut state = seed + 1;
            let mut text = String::new();
            for _ in 0..80 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(
                    [
                        "AlphaSymbol\n",
                        "BetaSymbol\r\n",
                        "GammaSymbol\n",
                        "unrelated\n",
                        "AlphaSymbol BetaSymbol GammaSymbol\n",
                        "\n",
                    ][(state >> 32) as usize % 6],
                );
            }
            assert_eq!(
                compiled.alignment(&text),
                find_canonical_alignment(&text, &runs),
                "seed={seed}"
            );
        }
    }

    #[test]
    fn anchored_sparse_hits_only_evaluate_occupied_windows() {
        let text = format!(
            "{}AlphaSymbol BetaSymbol\n{}",
            "unrelated\n".repeat(10_000),
            "unrelated\n".repeat(10_000)
        );
        let runs = vec!["AlphaSymbol".to_string(), "BetaSymbol".to_string()];
        let expected = find_canonical_alignment(&text, &runs).unwrap();
        let compiled = CompiledRuns::new(&runs);
        crate::search_hot_path_measurements::reset();
        let actual = compiled.alignment(&text).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            crate::search_hot_path_measurements::counts().anchor_windows_evaluated,
            3
        );
    }

    #[test]
    fn compiled_runs_preserve_unicode_overlap_and_empty_run_alignment() {
        for text in [
            "",
            "ababa ABAba",
            "Kelvin kelvin\nİstanbul Istanbul",
            "alpha\r\nbeta\ngamma",
            "abcdef ",
        ] {
            for runs in [
                vec![],
                vec!["".to_string()],
                vec!["aba".to_string(), "ba".to_string()],
                vec!["kelvin".to_string(), "Istanbul".to_string()],
                vec!["alpha".to_string(), "".to_string(), "gamma".to_string()],
            ] {
                assert_eq!(
                    CompiledRuns::new(&runs).alignment(text),
                    find_canonical_alignment(text, &runs),
                    "text={text:?}, runs={runs:?}"
                );
            }
        }
    }
}
