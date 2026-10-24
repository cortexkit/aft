use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::memo::{
    compute_content_digest, ExactMemoStore, MemoError, MemoKey, ServeOutcome, VerifiedExactSet,
};
use crate::commands::semantic_search::comparator::{
    score_free_r3_cmp, CandidateResult, SymbolOffsetRange,
};
use crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use crate::commands::semantic_search::generation_token::GenerationToken;
use crate::commands::semantic_search::plan_table::SearchLaneKind;
use crate::commands::semantic_search::{LaneExecution, LaneInput, SearchLane};
use crate::query_shape::{extract_content_tokens, minimum_content_token_window};
use crate::search_index::{
    read_search_corpus_file, SearchCorpusEligibility, SearchIndex, SearchIndexSnapshot,
    DEFAULT_MAX_FILE_SIZE,
};

/// Disable the default file-count cutoff; the fallback walk has a time limit.
pub const DEFAULT_FALLBACK_FILE_LIMIT: usize = usize::MAX;
pub const DEFAULT_FALLBACK_RESULT_LIMIT: usize = 100;

/// Assemble response text containing optional bounded disclosure and trailer.
pub fn assemble_bounded_reply(bounded_line: Option<&str>, trailer: &str) -> String {
    if let Some(line) = bounded_line {
        format!("{line}\n{trailer}")
    } else {
        trailer.to_string()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailerReason {
    Exhausted,
    DepthCap,
    MoreAtGreaterDepth,
}

/// Format page trailer per R14 semantics.
pub fn format_trailer(shown: usize, total_or_bound: usize, reason: TrailerReason) -> String {
    match reason {
        TrailerReason::Exhausted => format!("shown {shown} of {total_or_bound} (exhausted)"),
        TrailerReason::DepthCap => format!("shown {shown} of {total_or_bound}+ (depth cap)"),
        TrailerReason::MoreAtGreaterDepth => {
            format!("shown {shown} of {total_or_bound}+ (more at greater depth)")
        }
    }
}

/// Exact phrase extraction: strips surrounding single/double quotes if balanced.
pub fn exact_phrase(query: &str) -> &str {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return trimmed;
    }
    let first = trimmed.as_bytes()[0];
    let last = trimmed.as_bytes()[trimmed.len() - 1];
    if matches!(first, b'\'' | b'"') && first == last {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Normalized exact phrase for verbatim comparison.
pub fn normalize_exact_phrase(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Return whether a file can possibly contain an E2 window for all query tokens.
pub fn e2_window_scan_needed(normalized_text: &str, content_tokens: &[String]) -> bool {
    content_tokens.len() >= 2
        && content_tokens
            .iter()
            .all(|token| normalized_text.contains(token))
}

/// The query words that may stand in for the whole query as exact evidence: a
/// declaration named after one of them, or all of them within a short window.
///
/// A single hyphenated token is a literal name, so it gets none: its parts are
/// not words the user typed, and only a file containing the whole string (E1)
/// is exact evidence for it.
pub fn exact_verification_tokens(query: &str) -> Vec<String> {
    if crate::search_b2::router::is_hyphenated_literal(query) {
        Vec::new()
    } else {
        extract_content_tokens(query)
    }
}

pub use crate::search_index::ExactEvidenceScope;

/// Whether a declaration-shaped line in `path` may count as a definition.
///
/// Only source files of a language with a parser qualify. Documents and data
/// (Markdown notes, JSON, YAML, TOML, HTML, stylesheets, or a file with no
/// recognised language) often quote a declaration or carry a `name:` key, but
/// they do not define the name; giving them definition credit let a planning
/// note outrank the source file that declares the identifier.
pub fn is_definition_source(path: &Path) -> bool {
    use crate::parser::LangId;
    crate::parser::detect_language(path).is_some_and(|language| {
        !matches!(
            language,
            LangId::Markdown
                | LangId::Json
                | LangId::Yaml
                | LangId::Toml
                | LangId::Html
                | LangId::Scss
        )
    })
}

/// The member name a `receiver.member` query asks for, lowercased.
///
/// A query such as `ask.list_pending_for_user` is how a call site reads, so
/// its exact phrase only ever matches call sites: the declaration says
/// `fn list_pending_for_user`, without the receiver. The member name is
/// returned only when the whole exact phrase is one dotted name and the last
/// segment is itself identifier-shaped (snake_case or camelCase); a plain
/// word such as `lock` or `check` names too many unrelated declarations to
/// rank as the one the query means.
pub fn qualified_member_name(query: &str) -> Option<QualifiedMember> {
    let phrase = exact_phrase(query);
    let segments = phrase.split('.').collect::<Vec<_>>();
    let is_name = |segment: &str| {
        segment
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
    };
    if segments.len() < 2 || !segments.iter().all(|segment| is_name(segment)) {
        return None;
    }
    let member = segments[segments.len() - 1];
    if member.len() < 3 || !crate::search_b2::router::is_identifier_shaped_token(member) {
        return None;
    }
    Some(QualifiedMember {
        receiver: segments[segments.len() - 2].to_ascii_lowercase(),
        member: member.to_ascii_lowercase(),
    })
}

/// The parts of a `receiver.member` query that the exact lane uses; see
/// [`qualified_member_name`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedMember {
    /// The segment before the member, lowercased (`ask` in `ask.list_pending`).
    pub receiver: String,
    /// The member name, lowercased.
    pub member: String,
}

impl QualifiedMember {
    /// Whether `path` names the receiver, as a cheap stand-in for resolving
    /// the receiver's type: a directory or file stem that contains the
    /// receiver (`ask` in `ask/store.rs`) or abbreviates it the way variable
    /// names do (`ctx` for `context.rs`: same first letter, the receiver's
    /// letters appear in order). `self` and `this` name no type and never match.
    pub fn path_names_receiver(&self, path: &Path) -> bool {
        let receiver = self.receiver.as_str();
        if receiver.len() < 2 || matches!(receiver, "self" | "this") {
            return false;
        }
        let stem = path.file_stem();
        path.parent()
            .into_iter()
            .flat_map(Path::components)
            .map(|component| component.as_os_str())
            .chain(stem)
            .filter_map(|component| component.to_str())
            .map(str::to_ascii_lowercase)
            .any(|component| component.contains(receiver) || is_abbreviation(receiver, &component))
    }
}

/// `short` starts with the same letter as `long` and its letters appear in
/// `long` in order (`ctx` abbreviates `context`, `cfg` abbreviates `config`).
fn is_abbreviation(short: &str, long: &str) -> bool {
    if short.len() >= long.len() || short.as_bytes().first() != long.as_bytes().first() {
        return false;
    }
    let mut letters = long.bytes();
    short
        .bytes()
        .all(|wanted| letters.by_ref().any(|letter| letter == wanted))
}

/// Options controlling fallback execution.
#[derive(Clone, Default)]
pub struct FallbackExactOptions {
    pub file_limit: Option<usize>,
    pub result_limit: Option<usize>,
    pub time_limit: Option<Duration>,
    pub delay_hook: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    pub force_directory_order: bool,
}

/// Result of a fallback walk.
#[derive(Clone, Debug)]
pub struct FallbackExactResult {
    pub verified_set: VerifiedExactSet,
    pub files_visited: usize,
    pub entries_examined: usize,
    pub bound_reason: Option<String>,
}

/// Exact lane implementation of SearchLane.
pub struct ExactLane {
    pub memo: Arc<ExactMemoStore>,
    pub fallback_file_limit: usize,
    pub fallback_result_limit: usize,
}

impl Default for ExactLane {
    fn default() -> Self {
        Self::new()
    }
}

impl ExactLane {
    pub fn new() -> Self {
        Self {
            memo: Arc::new(ExactMemoStore::new()),
            fallback_file_limit: DEFAULT_FALLBACK_FILE_LIMIT,
            fallback_result_limit: DEFAULT_FALLBACK_RESULT_LIMIT,
        }
    }

    pub fn with_memo(memo: Arc<ExactMemoStore>) -> Self {
        Self {
            memo,
            fallback_file_limit: DEFAULT_FALLBACK_FILE_LIMIT,
            fallback_result_limit: DEFAULT_FALLBACK_RESULT_LIMIT,
        }
    }

    /// Run whole-corpus exact pass in ready mode over the index snapshot.
    pub fn execute_ready_mode(
        &self,
        snapshot: &SearchIndexSnapshot,
        project_root: &Path,
        query: &str,
        include_tests: bool,
    ) -> VerifiedExactSet {
        self.execute_ready_mode_scoped(
            snapshot,
            project_root,
            query,
            include_tests,
            &ExactEvidenceScope::All,
        )
    }

    /// [`Self::execute_ready_mode`] that reads only the files able to hold the
    /// evidence kinds `scope` keeps.
    pub fn execute_ready_mode_scoped(
        &self,
        snapshot: &SearchIndexSnapshot,
        project_root: &Path,
        query: &str,
        include_tests: bool,
        scope: &ExactEvidenceScope,
    ) -> VerifiedExactSet {
        let matches = snapshot.whole_corpus_exact_pass_scoped(
            query,
            project_root,
            Some(&|path| super::path_allowed_by_include_tests(path, project_root, include_tests)),
            scope,
        );

        let mut results = Vec::new();
        let mut file_digests = HashMap::new();

        for m in matches {
            file_digests
                .entry(m.path.clone())
                .or_insert(m.content_digest);
            results.push(CandidateResult::new_exact(
                m.path,
                m.symbol_range,
                m.evidence,
            ));
        }

        // Exact-tier canonical order: score-free R3 order
        results.sort_by(score_free_r3_cmp);

        VerifiedExactSet {
            results,
            file_digests,
            bound_disclosure: None,
            stability_void: false,
        }
    }

    /// Run R17 fallback mode walk over the project filesystem.
    pub fn execute_fallback_mode(
        &self,
        project_root: &Path,
        query: &str,
        include_tests: bool,
        options: &FallbackExactOptions,
    ) -> FallbackExactResult {
        let file_limit = options.file_limit.unwrap_or(self.fallback_file_limit);
        let result_limit = options.result_limit.unwrap_or(self.fallback_result_limit);
        let budget = options
            .time_limit
            .unwrap_or(crate::grep_executor::FALLBACK_WALK_BUDGET);
        let deadline = Some(Instant::now() + budget);

        // Discovery and verification share one deadline. The shared walkers
        // reject directory symlinks and foreign mounts.
        let walk = if file_limit == usize::MAX {
            crate::grep_executor::source_first_fallback_walk_files(
                project_root,
                deadline.expect("fallback has a deadline"),
            )
        } else {
            // Callers and tests can still request explicit file-count bounds.
            crate::grep_executor::bounded_fallback_walk_files_with_limits(
                project_root,
                project_root,
                &crate::search_index::PathFilters::default(),
                file_limit.saturating_add(1).max(1024),
                budget,
            )
        };
        let mut files = walk.files;

        // Filter tests if needed
        if !include_tests {
            files.retain(|p| super::path_allowed_by_include_tests(p, project_root, false));
        }

        // Prefer source over documentation/data; retain byte-wise path order
        // within each group so bounded pages do not depend on directory order.
        if options.force_directory_order {
            // Mutation red test: do not sort, keep directory order
        } else {
            files.sort_by(|a, b| {
                let rel_a = a.strip_prefix(project_root).unwrap_or(a);
                let rel_b = b.strip_prefix(project_root).unwrap_or(b);
                crate::grep_executor::is_fallback_source_path(b)
                    .cmp(&crate::grep_executor::is_fallback_source_path(a))
                    .then_with(|| {
                        rel_a
                            .as_os_str()
                            .as_encoded_bytes()
                            .cmp(rel_b.as_os_str().as_encoded_bytes())
                    })
            });
        }

        let discovered_files = files.len();
        let phrase = exact_phrase(query);
        let norm_phrase = normalize_exact_phrase(phrase);
        let content_tokens = exact_verification_tokens(query);
        let member = qualified_member_name(query);

        let mut results = Vec::new();
        let mut file_digests = HashMap::new();
        let mut files_visited = 0;
        let timed_out = deadline.is_some_and(|dl| Instant::now() >= dl);
        let mut bound_reason = timed_out.then(|| "time limit".to_string());
        let mut stability_void = timed_out;

        for file_path in files {
            // Check watchdog timer
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    bound_reason = Some("time limit".to_string());
                    stability_void = true;
                    break;
                }
            }

            // Check file limit
            if files_visited >= file_limit {
                bound_reason = Some("file limit".to_string());
                break;
            }

            // Check result limit
            if results.len() >= result_limit {
                bound_reason = Some("result limit".to_string());
                break;
            }

            // Only files eligible for the trigram corpus can contribute exact evidence.
            let SearchCorpusEligibility::Eligible(file) =
                read_search_corpus_file(&file_path, DEFAULT_MAX_FILE_SIZE)
            else {
                continue;
            };
            files_visited += 1;
            if let Some(delay_fn) = &options.delay_hook {
                delay_fn(&file_path);
            }
            let digest = compute_content_digest(&file.bytes);
            file_digests.insert(file_path.clone(), digest);

            let text = String::from_utf8_lossy(&file.bytes);
            if let Some(mut candidates) = verify_exact_matches_in_text_with_member(
                &file_path,
                &text,
                &norm_phrase,
                &content_tokens,
                member.as_ref().map(|member| member.member.as_str()),
            ) {
                for candidate in &mut candidates {
                    candidate.evidence.generated = file.generated;
                }
                results.extend(candidates);
            }
        }

        if bound_reason.is_none() && (walk.walk_truncated || walk.skipped_foreign_mounts > 0) {
            bound_reason = Some("enumeration limit or filesystem boundary".into());
            stability_void = true;
        }
        let bound_disclosure = bound_reason.as_ref().map(|reason| {
            let coverage = if walk.walk_truncated {
                format!("{files_visited} files; total unknown because enumeration stopped")
            } else {
                format!("{files_visited} of {discovered_files} discovered files")
            };
            let stability = if stability_void { " - page stability void" } else { "" };
            format!(
                "exact pass: bounded ({files_visited} files, {reason}){stability}; checked {coverage} (trigram index unavailable); source code and other files under {} not fully searched; use grep for an exhaustive check",
                project_root.display()
            )
        });

        results.sort_by(score_free_r3_cmp);

        FallbackExactResult {
            verified_set: VerifiedExactSet {
                results,
                file_digests,
                bound_disclosure,
                stability_void,
            },
            files_visited,
            entries_examined: walk.entries_visited,
            bound_reason,
        }
    }

    /// Serve an exact query using the memo table.
    pub fn search(
        &self,
        index: Option<&SearchIndex>,
        project_root: &Path,
        snapshot_generation: GenerationToken,
        query: &str,
        include_tests: bool,
        offset: usize,
        top_k: usize,
        fallback_options: Option<&FallbackExactOptions>,
    ) -> Result<ServeOutcome, MemoError> {
        self.search_scoped(
            index,
            project_root,
            snapshot_generation,
            query,
            include_tests,
            offset,
            top_k,
            fallback_options,
            &ExactEvidenceScope::All,
        )
    }

    /// [`Self::search`] for a caller that keeps only the evidence in `scope`.
    /// In ready mode the pass reads only files that can hold that evidence;
    /// the fallback walk is unchanged. The scope is part of the memo key, so a
    /// narrowly scoped entry never serves a caller that keeps more.
    pub fn search_scoped(
        &self,
        index: Option<&SearchIndex>,
        project_root: &Path,
        snapshot_generation: GenerationToken,
        query: &str,
        include_tests: bool,
        offset: usize,
        top_k: usize,
        fallback_options: Option<&FallbackExactOptions>,
        scope: &ExactEvidenceScope,
    ) -> Result<ServeOutcome, MemoError> {
        let is_ready = index.is_some_and(SearchIndex::is_ready);
        // Index loading can finish without changing snapshot_generation. Give
        // fallback results a separate memo key so they cannot be reused once
        // the full index is ready.
        let memo_generation = if is_ready {
            snapshot_generation
        } else {
            GenerationToken::new_with_str(&format!("fallback:{}", snapshot_generation.as_str()))
        };
        let key = MemoKey::new(project_root, memo_generation, query, include_tests)
            .with_evidence_scope(scope.clone());

        let default_opts = FallbackExactOptions::default();
        let fallback_opts = fallback_options.unwrap_or(&default_opts);

        self.memo.get_or_verify(&key, offset, top_k, || {
            if is_ready {
                let snapshot = index.unwrap().snapshot();
                Ok(self.execute_ready_mode_scoped(
                    &snapshot,
                    project_root,
                    query,
                    include_tests,
                    scope,
                ))
            } else {
                let mut fallback =
                    self.execute_fallback_mode(project_root, query, include_tests, fallback_opts);
                if let Some(disclosure) = &mut fallback.verified_set.bound_disclosure {
                    disclosure.push_str(&format!(
                        "; examined {} directory entries; narrow: path or query",
                        fallback.entries_examined
                    ));
                }
                Ok(fallback.verified_set)
            }
        })
    }
}

impl SearchLane for ExactLane {
    fn kind(&self) -> SearchLaneKind {
        SearchLaneKind::Exact
    }

    fn execute(&self, input: &LaneInput<'_>) -> LaneExecution {
        let snapshot = input.index.snapshot();
        let keeps_definitions =
            input.shape == crate::commands::semantic_search::SearchShape::Identifier;
        let scope = if keeps_definitions {
            ExactEvidenceScope::All
        } else {
            ExactEvidenceScope::PhraseAndWindow
        };
        let mut candidates = self
            .execute_ready_mode_scoped(
                &snapshot,
                input.root,
                input.query,
                input.include_tests,
                &scope,
            )
            .results;
        if !keeps_definitions {
            candidates.retain(|candidate| {
                candidate.evidence.kind
                    != crate::commands::semantic_search::EvidenceKind::Definition
            });
        }
        LaneExecution {
            kind: self.kind(),
            candidates,
        }
    }
}

/// Verify exact matches in file text and return candidates.
pub fn verify_exact_matches_in_text(
    file_path: &Path,
    text: &str,
    norm_phrase: &str,
    content_tokens: &[String],
) -> Option<Vec<CandidateResult>> {
    verify_exact_matches_in_text_with_member(file_path, text, norm_phrase, content_tokens, None)
}

/// [`verify_exact_matches_in_text`], where a declaration of `member` (the
/// lowercased last segment of a `receiver.member` query, see
/// [`qualified_member_name`]) also counts as a definition hit.
///
/// Definition credit goes only to a file for which [`is_definition_source`]
/// holds. In any other file a declaration-shaped line is treated like any
/// other text: it is E1 evidence when it contains the phrase.
pub fn verify_exact_matches_in_text_with_member(
    file_path: &Path,
    text: &str,
    norm_phrase: &str,
    content_tokens: &[String],
    member: Option<&str>,
) -> Option<Vec<CandidateResult>> {
    let mut matches = Vec::new();
    let norm_text = normalize_exact_phrase(text);
    let definition_source = is_definition_source(file_path);

    // 1. Check symbols first (for symbol-level candidates like `cap_chars`)
    let symbols = scan_symbols_in_text(text);
    for (name, range) in &symbols {
        // The scanner guarantees boundary-aligned offsets; a non-aligned range
        // is a scanner bug, and skipping the symbol is the right failure here
        // because a panic on this path takes the whole search actor down.
        let Some(sym_text) = text.get(range.start..range.end.min(text.len())) else {
            debug_assert!(false, "symbol range {range:?} is not char-boundary aligned");
            continue;
        };
        let norm_sym_text = normalize_exact_phrase(sym_text);
        let lowercase_name = name.to_ascii_lowercase();
        // The member stand-in counts only item declarations (`fn`, `def`,
        // `function`, `class`, ...), whose line starts with a keyword. A
        // `name:` line is a struct field or an object-literal key, and for a
        // member name the keys are mostly test doubles (`clearPluginCache:
        // async () => ...`) that would outrank the real declaration.
        let names_member = member.is_some_and(|member| {
            lowercase_name == member && !sym_text.trim_start().starts_with(name.as_str())
        });
        let names_query =
            content_tokens.iter().any(|token| lowercase_name == *token) || names_member;
        if definition_source && names_query {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                Some(*range),
                EvidenceDescriptor::for_definition(true, false),
            ));
        } else if !norm_phrase.is_empty() && norm_sym_text.contains(norm_phrase) {
            let occ = norm_sym_text.matches(norm_phrase).count();
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                Some(*range),
                EvidenceDescriptor::for_e1(occ, true, false),
            ));
        }
    }

    // 2. If no symbol matched, check verbatim phrase match in whole file (E1)
    if matches.is_empty() && !norm_phrase.is_empty() {
        let occ = norm_text.matches(norm_phrase).count();
        if occ > 0 {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                None,
                EvidenceDescriptor::for_e1(occ, true, false),
            ));
        }
    }

    // 3. Check 3-line window match (E2) if no E1 match on file-level.
    // Files missing any query token cannot contain an all-token window, so reject
    // them before scanning identifiers and tracking line coverage.
    if matches.is_empty() && e2_window_scan_needed(&norm_text, content_tokens) {
        if let Some(width) = minimum_content_token_window(text, content_tokens) {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                None,
                EvidenceDescriptor::for_e2(width, true, false),
            ));
        }
    }

    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

/// Lightweight symbol scanner finding functions / structs / classes in source files.
///
/// Offsets are true byte positions into `text`. The walk uses
/// `split_inclusive('\n')` rather than `lines()` because `lines()` strips a
/// trailing `\r` as well as the `\n`, so summing `line.len() + 1` drifts one
/// byte left per CRLF line; on a large CRLF file with non-ASCII content the
/// drifted start landed inside a multibyte character and slicing it panicked
/// the search actor.
pub fn scan_symbols_in_text(text: &str) -> Vec<(String, SymbolOffsetRange)> {
    let mut symbols = Vec::new();
    let mut current_offset = 0;

    for segment in text.split_inclusive('\n') {
        let line = segment
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(segment);
        let trimmed = line.trim_start();
        let leading_spaces = line.len() - trimmed.len();
        let line_offset = current_offset + leading_spaces;

        let (name_opt, declaration_line_only) = if let Some(rest) = trimmed.strip_prefix("pub fn ")
        {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("fn ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("pub(crate) fn ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("def ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("export function ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("function ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("pub struct ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("struct ") {
            (extract_identifier(rest), false)
        } else {
            (
                [
                    "export namespace ",
                    "namespace ",
                    "export class ",
                    "class ",
                    "export interface ",
                    "interface ",
                    "export enum ",
                    "pub enum ",
                    "enum ",
                    "pub trait ",
                    "trait ",
                    "export type ",
                    "pub type ",
                    "type ",
                    "export const ",
                    "pub const ",
                    "const ",
                ]
                .into_iter()
                .find_map(|prefix| trimmed.strip_prefix(prefix).and_then(extract_identifier))
                .or_else(|| extract_field_identifier(trimmed)),
                true,
            )
        };

        if let Some(name) = name_opt {
            let start = line_offset;
            debug_assert!(text.is_char_boundary(start));
            // The lightweight span is byte-addressed because downstream ranges
            // slice UTF-8 source. Clamp the approximate end to a character boundary.
            let mut end = if declaration_line_only {
                (current_offset + line.len()).min(text.len())
            } else {
                (start + 500).min(text.len())
            };
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            symbols.push((name, SymbolOffsetRange::new(start, end)));
        }

        current_offset += segment.len();
    }

    symbols
}

#[cfg(test)]
fn audit01_visibility_counts(
    root: &Path,
    paths: &[&str],
    include_tests: bool,
) -> (usize, usize, usize) {
    std::fs::create_dir_all(root).unwrap();
    let mut index = crate::search_index::SearchIndex::new();
    for relative in paths {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = "pub fn visible() { needle_phrase(); }\n";
        std::fs::write(&path, text).unwrap();
        index.index_file(&path, text.as_bytes());
    }
    let lane = ExactLane::new();
    let ready = lane.execute_ready_mode(&index.snapshot(), root, "needle_phrase", include_tests);
    let fallback = lane.execute_fallback_mode(
        root,
        "needle_phrase",
        include_tests,
        &FallbackExactOptions::default(),
    );
    let anchored = super::anchored_lane::discover_anchored_candidate_files(
        &index,
        root,
        &["needle_phrase".to_string()],
        include_tests,
    );
    (
        ready.file_digests.len(),
        fallback.verified_set.file_digests.len(),
        anchored.len(),
    )
}

#[cfg(test)]
#[test]
fn audit01_nested_checkout_visibility() {
    for ancestor in ["tests", "__tests__"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(ancestor).join("product");
        assert_eq!(
            audit01_visibility_counts(&root, &["src/lib.rs"], false),
            (1, 1, 1)
        );
    }
}

#[cfg(test)]
#[test]
fn audit01_shared_test_support_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let paths = [
        "src/lib.rs",
        "src/FooTest.java",
        "src/FooTest.kt",
        "fixtures/helper.rs",
    ];
    assert_eq!(
        audit01_visibility_counts(temp.path(), &paths, false),
        (1, 1, 1)
    );
    assert_eq!(
        audit01_visibility_counts(temp.path(), &paths, true),
        (4, 4, 4)
    );
}

fn extract_field_identifier(line: &str) -> Option<String> {
    let (candidate, suffix) = line.split_once(':')?;
    if suffix.starts_with(':') {
        return None;
    }
    let candidate = candidate.trim();
    if candidate.is_empty()
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    Some(candidate.to_string())
}

fn extract_identifier(s: &str) -> Option<String> {
    let ident: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        None
    } else {
        Some(ident)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::commands::semantic_search::EvidenceKind;
    use std::path::PathBuf;

    /// A project where only `scripts/check.py` contains `residue-source-hash`.
    /// The other two files hold its parts: a declared field named `source`,
    /// and all three words within three lines.
    fn hyphenated_literal_project() -> (tempfile::TempDir, Vec<(PathBuf, String)>) {
        let project = tempfile::tempdir().expect("create project dir");
        let files = [
            (
                "src/server.rs",
                "pub struct Registry {\n    source: String,\n}\n",
            ),
            (
                "docs/notes.md",
                "The residue left behind\nby the source\nchanges its hash.\n",
            ),
            (
                "scripts/check.py",
                "CHECKS = [\"residue-source-hash\", \"slice-fences\"]\n",
            ),
        ];
        let mut entries = Vec::new();
        for (relative, text) in files {
            let path = project.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            std::fs::write(&path, text).expect("write file");
            entries.push((path, text.to_string()));
        }
        (project, entries)
    }

    fn exact_paths(results: &[CandidateResult], root: &Path) -> Vec<(String, EvidenceKind)> {
        results
            .iter()
            .map(|candidate| {
                (
                    candidate
                        .path
                        .strip_prefix(root)
                        .unwrap_or(&candidate.path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    candidate.evidence.kind,
                )
            })
            .collect()
    }

    pub(crate) fn large_identifier_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        let docs = project.path().join("docs");
        let source = project.path().join("packages/plugin/src");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        for i in 0..1_200 {
            std::fs::write(
                docs.join(format!("{i:04}.md")),
                if i < 2 {
                    "isPrefixBoundThinkingModel"
                } else {
                    "unrelated documentation"
                },
            )
            .unwrap();
        }
        for i in 0..45 {
            std::fs::write(
                source.join(format!("use_{i:02}.ts")),
                "export const value = isPrefixBoundThinkingModel(model);\n",
            )
            .unwrap();
        }
        project
    }

    #[test]
    fn unavailable_index_searches_code_beyond_old_thousand_file_cap() {
        let project = large_identifier_project();
        let result = ExactLane::new().execute_fallback_mode(
            project.path(),
            "isPrefixBoundThinkingModel",
            true,
            &FallbackExactOptions::default(),
        );
        assert_eq!(result.files_visited, 1_245);
        assert_eq!(result.verified_set.results.len(), 47);
        assert!(result.verified_set.bound_disclosure.is_none());
    }

    #[test]
    fn fallback_checks_source_before_docs_when_result_budget_expires() {
        let project = large_identifier_project();
        let result = ExactLane::new().execute_fallback_mode(
            project.path(),
            "isPrefixBoundThinkingModel",
            true,
            &FallbackExactOptions {
                result_limit: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(result.verified_set.results.len(), 1);
        assert!(result.verified_set.results[0]
            .path
            .starts_with(project.path().join("packages")));
        let notice = result.verified_set.bound_disclosure.unwrap();
        assert!(notice.contains("1 of 1245 discovered files"), "{notice}");
        assert!(
            notice.contains("not fully searched; use grep for an exhaustive check"),
            "{notice}"
        );
    }

    #[test]
    fn fallback_timeout_names_unknown_coverage_and_exhaustive_alternative() {
        let project = large_identifier_project();
        let result = ExactLane::new().execute_fallback_mode(
            project.path(),
            "isPrefixBoundThinkingModel",
            true,
            &FallbackExactOptions {
                time_limit: Some(Duration::ZERO),
                ..Default::default()
            },
        );
        let notice = result.verified_set.bound_disclosure.unwrap();
        assert!(
            notice.contains("total unknown because enumeration stopped"),
            "{notice}"
        );
        assert!(notice.contains("trigram index unavailable"), "{notice}");
        assert!(
            notice.contains(&project.path().display().to_string()),
            "{notice}"
        );
        assert!(
            notice.contains("not fully searched; use grep for an exhaustive check"),
            "{notice}"
        );
        assert_eq!(result.entries_examined, 1);
    }

    #[test]
    fn ready_index_does_not_reuse_loading_fallback_memo() {
        let project = large_identifier_project();
        let lane = ExactLane::new();
        let generation = GenerationToken::new(1);
        let partial = lane
            .search(
                None,
                project.path(),
                generation.clone(),
                "isPrefixBoundThinkingModel",
                true,
                0,
                50,
                Some(&FallbackExactOptions {
                    result_limit: Some(1),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(partial.results.len(), 1);
        let index = SearchIndex::build(project.path());
        let ready = lane
            .search(
                Some(&index),
                project.path(),
                generation,
                "isPrefixBoundThinkingModel",
                true,
                0,
                50,
                None,
            )
            .unwrap();
        assert_eq!(ready.results.len(), 47);
        assert!(ready.bound_disclosure.is_none());
    }

    #[test]
    #[ignore = "requires an isolated real corpus in AFT_EXACT_REPRO_ROOT"]
    fn copied_worktree_exact_fallback_reproduction() {
        let root =
            PathBuf::from(std::env::var_os("AFT_EXACT_REPRO_ROOT").expect("isolated corpus"));
        let old = crate::grep_executor::bounded_fallback_walk_files_with_limits(
            &root,
            &root,
            &crate::search_index::PathFilters::default(),
            1_024,
            crate::grep_executor::FALLBACK_WALK_BUDGET,
        );
        let mut old_files = old.files;
        old_files.sort();
        old_files.truncate(1_000);
        let old_hits: Vec<_> = old_files
            .iter()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .is_ok_and(|text| text.contains("isPrefixBoundThinkingModel"))
            })
            .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        eprintln!(
            "OLD WALK entries={} truncated={} hits={old_hits:?}",
            old.entries_visited, old.walk_truncated
        );
        let result = ExactLane::new().execute_fallback_mode(
            &root,
            "isPrefixBoundThinkingModel",
            true,
            &Default::default(),
        );
        eprintln!(
            "NEW WALK files={} hits={:?} notice={:?}",
            result.files_visited,
            exact_paths(&result.verified_set.results, &root),
            result.verified_set.bound_disclosure
        );
        assert!(
            result
                .verified_set
                .results
                .iter()
                .any(|candidate| candidate.path.starts_with(root.join("packages")))
                || result.verified_set.bound_disclosure.is_some()
        );
    }

    #[test]
    fn receiver_member_query_finds_the_member_declaration_in_source_only() {
        let project = tempfile::tempdir().expect("create project dir");
        let files = [
            (
                "src/handlers.rs",
                "fn handle() {\n    ask.list_pending_for_user(user);\n}\n",
            ),
            (
                "src/ask/store.rs",
                "pub fn list_pending_for_user(user: &str) {}\n",
            ),
            (
                ".notes/plan.md",
                "fn list_pending_for_user(user: &str) {}\n",
            ),
            (
                "src/fake_store.ts",
                "export const fakeAsk = {\n  list_pending_for_user: () => [],\n};\n",
            ),
        ];
        let mut index = SearchIndex::new();
        for (relative, text) in files {
            let path = project.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            std::fs::write(&path, text).expect("write file");
            index.index_file(&path, text.as_bytes());
        }
        index.ready = true;
        let expected = vec![
            ("src/ask/store.rs".to_string(), EvidenceKind::Definition),
            ("src/handlers.rs".to_string(), EvidenceKind::E1),
        ];

        let ready = ExactLane::new().execute_ready_mode(
            &index.snapshot(),
            project.path(),
            "ask.list_pending_for_user",
            false,
        );
        assert_eq!(exact_paths(&ready.results, project.path()), expected);

        let fallback = ExactLane::new().execute_fallback_mode(
            project.path(),
            "ask.list_pending_for_user",
            false,
            &FallbackExactOptions::default(),
        );
        assert_eq!(
            exact_paths(&fallback.verified_set.results, project.path()),
            expected
        );
    }

    #[test]
    fn qualified_member_name_needs_a_dotted_query_and_a_specific_member() {
        let member = qualified_member_name("ask.list_pending_for_user").expect("member");
        assert_eq!(member.receiver, "ask");
        assert_eq!(member.member, "list_pending_for_user");
        assert_eq!(
            qualified_member_name("adapter.clearPluginCache").map(|member| member.member),
            Some("clearplugincache".to_string())
        );
        // A plain-word member names too many unrelated declarations. A query
        // without a receiver needs no stand-in: its own tokens already earn
        // definition credit when a declaration has the same name.
        assert_eq!(qualified_member_name("health.check"), None);
        assert_eq!(qualified_member_name("inner.tasks.lock()"), None);
        assert_eq!(qualified_member_name("running_tasks"), None);
        assert_eq!(qualified_member_name("ask list_pending_for_user"), None);
    }

    #[test]
    fn receiver_matches_a_path_component_that_contains_or_abbreviates_it() {
        let member = |query| qualified_member_name(query).expect("member");
        let ctx = member("ctx.set_harness");
        assert!(ctx.path_names_receiver(Path::new("crates/aft/src/context.rs")));
        assert!(!ctx.path_names_receiver(Path::new("crates/aft/src/bash_background/registry.rs")));
        let ask = member("ask.list_pending_for_user");
        assert!(ask.path_names_receiver(Path::new("crates/core/src/ask/store.rs")));
        assert!(ask.path_names_receiver(Path::new("crates/core-store/src/ask.rs")));
        assert!(!member("self.set_harness").path_names_receiver(Path::new("src/self.rs")));
    }

    #[test]
    fn only_source_languages_earn_definition_credit() {
        for path in ["a.rs", "a.ts", "a.py", "a.go", "a.sh"] {
            assert!(is_definition_source(Path::new(path)), "{path}");
        }
        for path in [
            "a.md", "a.json", "a.yml", "a.toml", "a.html", "a.scss", "a.txt", "Makefile",
        ] {
            assert!(!is_definition_source(Path::new(path)), "{path}");
        }
        let declared = verify_exact_matches_in_text(
            Path::new("notes.md"),
            "fn line_col_to_byte(source: &str) -> usize {\n",
            "line_col_to_byte",
            &["line_col_to_byte".to_string()],
        )
        .expect("the note still contains the phrase");
        assert!(declared
            .iter()
            .all(|candidate| candidate.evidence.kind == EvidenceKind::E1));
    }

    #[test]
    fn hyphenated_literal_is_exact_only_where_the_whole_string_occurs() {
        let (project, entries) = hyphenated_literal_project();
        let mut index = SearchIndex::new();
        for (path, text) in &entries {
            index.index_file(path, text.as_bytes());
        }
        index.ready = true;

        let ready = ExactLane::new().execute_ready_mode(
            &index.snapshot(),
            project.path(),
            "residue-source-hash",
            false,
        );
        assert_eq!(
            exact_paths(&ready.results, project.path()),
            vec![("scripts/check.py".to_string(), EvidenceKind::E1)]
        );

        let fallback = ExactLane::new().execute_fallback_mode(
            project.path(),
            "residue-source-hash",
            false,
            &FallbackExactOptions::default(),
        );
        assert_eq!(
            exact_paths(&fallback.verified_set.results, project.path()),
            vec![("scripts/check.py".to_string(), EvidenceKind::E1)]
        );
    }

    #[test]
    fn spaced_words_keep_declaration_and_window_evidence() {
        // The same words separated by spaces are ordinary query words, so the
        // declared field and the three-line window still count as exact.
        let (project, entries) = hyphenated_literal_project();
        let mut index = SearchIndex::new();
        for (path, text) in &entries {
            index.index_file(path, text.as_bytes());
        }
        index.ready = true;

        let ready = ExactLane::new().execute_ready_mode(
            &index.snapshot(),
            project.path(),
            "residue source hash",
            false,
        );
        let found = exact_paths(&ready.results, project.path());
        assert!(
            found.contains(&("src/server.rs".to_string(), EvidenceKind::Definition)),
            "{found:?}"
        );
        assert!(
            found.contains(&("docs/notes.md".to_string(), EvidenceKind::E2)),
            "{found:?}"
        );
    }

    const PROSE_QUERY: &str = "how does the reconciler retry stalled uploads";

    /// A corpus for `PROSE_QUERY`: one file holds the sentence verbatim but
    /// wrapped across a line break and in another case, one holds every query
    /// word within three lines, two declare a function named after a query
    /// word (only one of them mentions `retry_upload`), and twenty mention only
    /// one or two of the query words.
    fn prose_project() -> (tempfile::TempDir, SearchIndex) {
        let mut files = vec![
            (
                "docs/guide.md".to_string(),
                "Q: How does the Reconciler\n   retry stalled uploads?\n".to_string(),
            ),
            (
                "src/window.rs".to_string(),
                "// reconciler\n// retry stalled\n// uploads\n".to_string(),
            ),
            ("src/retry.rs".to_string(), "fn retry() {}\n".to_string()),
            (
                "src/upload.rs".to_string(),
                "fn retry() {\n    retry_upload();\n}\n".to_string(),
            ),
        ];
        let mentions = ["the reconciler runs", "uploads go here", "stalled retry"];
        for index in 0..20 {
            files.push((
                format!("src/noise_{index}.rs"),
                format!("// {}\n", mentions[index % mentions.len()]),
            ));
        }
        let project = tempfile::tempdir().expect("create project dir");
        let mut index = SearchIndex::new();
        for (relative, text) in &files {
            let path = project.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            std::fs::write(&path, text).expect("write file");
            index.index_file(&path, text.as_bytes());
        }
        index.ready = true;
        (project, index)
    }

    /// Runs a ready-mode pass and returns its results with the number of
    /// files it read.
    fn scoped_pass(
        index: &SearchIndex,
        root: &Path,
        query: &str,
        scope: &ExactEvidenceScope,
    ) -> (Vec<CandidateResult>, usize) {
        crate::search_hot_path_measurements::reset();
        let results = ExactLane::new()
            .execute_ready_mode_scoped(&index.snapshot(), root, query, false, scope)
            .results;
        (
            results,
            crate::search_hot_path_measurements::counts().file_reads,
        )
    }

    #[test]
    fn prose_pass_without_declarations_reads_only_files_with_every_word() {
        let (project, index) = prose_project();
        let (_, unscoped_reads) = scoped_pass(
            &index,
            project.path(),
            PROSE_QUERY,
            &ExactEvidenceScope::All,
        );
        assert_eq!(unscoped_reads, 24, "every file mentions some query word");

        let (_, phrase_and_window_reads) = scoped_pass(
            &index,
            project.path(),
            PROSE_QUERY,
            &ExactEvidenceScope::PhraseAndWindow,
        );
        assert_eq!(
            phrase_and_window_reads, 2,
            "only the phrase and window files hold every query word"
        );

        let (_, declaration_reads) = scoped_pass(
            &index,
            project.path(),
            PROSE_QUERY,
            &ExactEvidenceScope::DefinitionsMentioning(vec!["retry_upload".to_string()]),
        );
        assert_eq!(
            declaration_reads, 3,
            "a declaration is read only where the identifier is mentioned"
        );
    }

    #[test]
    fn scoped_prose_pass_keeps_every_kept_exact_hit() {
        let (project, index) = prose_project();
        let root = project.path();
        let (all, _) = scoped_pass(&index, root, PROSE_QUERY, &ExactEvidenceScope::All);
        let all_found = exact_paths(&all, root);
        assert!(
            all_found.contains(&("docs/guide.md".to_string(), EvidenceKind::E1)),
            "a sentence wrapped across a line is still verbatim: {all_found:?}"
        );
        assert!(
            all_found.contains(&("src/window.rs".to_string(), EvidenceKind::E2)),
            "{all_found:?}"
        );

        // What a caller keeps: the evidence kinds it uses, plus declarations
        // in `definition_path`, the corpus file whose declaration mentions
        // `retry_upload`.
        let keep = |results: &[CandidateResult],
                    kinds: &[EvidenceKind],
                    definition_path: Option<&str>| {
            results
                .iter()
                .filter(|candidate| {
                    kinds.contains(&candidate.evidence.kind)
                        || (candidate.evidence.kind == EvidenceKind::Definition
                            && definition_path.is_some_and(|path| candidate.path.ends_with(path)))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let phrase_and_window_kinds = [EvidenceKind::E1, EvidenceKind::E2];
        let (phrase_and_window, _) = scoped_pass(
            &index,
            root,
            PROSE_QUERY,
            &ExactEvidenceScope::PhraseAndWindow,
        );
        assert_eq!(
            keep(&phrase_and_window, &phrase_and_window_kinds, None),
            keep(&all, &phrase_and_window_kinds, None)
        );
        let (phrase, _) = scoped_pass(&index, root, PROSE_QUERY, &ExactEvidenceScope::Phrase);
        assert_eq!(
            keep(&phrase, &[EvidenceKind::E1], None),
            keep(&all, &[EvidenceKind::E1], None)
        );
        let (declarations, _) = scoped_pass(
            &index,
            root,
            PROSE_QUERY,
            &ExactEvidenceScope::DefinitionsMentioning(vec!["retry_upload".to_string()]),
        );
        let kept_declarations = keep(
            &declarations,
            &phrase_and_window_kinds,
            Some("src/upload.rs"),
        );
        assert_eq!(
            kept_declarations,
            keep(&all, &phrase_and_window_kinds, Some("src/upload.rs"))
        );
        assert!(
            exact_paths(&kept_declarations, root)
                .contains(&("src/upload.rs".to_string(), EvidenceKind::Definition)),
            "{kept_declarations:?}"
        );
    }

    #[test]
    fn scoped_memo_entry_never_serves_a_caller_that_keeps_more() {
        let (project, index) = prose_project();
        let lane = ExactLane::new();
        let generation = GenerationToken::new_with_str("generation");
        let serve = |scope: &ExactEvidenceScope| {
            let outcome = lane
                .search_scoped(
                    Some(&index),
                    project.path(),
                    generation.clone(),
                    PROSE_QUERY,
                    false,
                    0,
                    usize::MAX,
                    None,
                    scope,
                )
                .expect("exact search");
            exact_paths(&outcome.results, project.path())
        };
        let narrow = serve(&ExactEvidenceScope::PhraseAndWindow);
        assert!(
            narrow.contains(&("src/window.rs".to_string(), EvidenceKind::E2)),
            "{narrow:?}"
        );
        let full = serve(&ExactEvidenceScope::All);
        assert!(
            full.contains(&("src/retry.rs".to_string(), EvidenceKind::Definition)),
            "{full:?}"
        );
        assert_eq!(lane.memo.verifier_call_count(), 2);
    }
}
