use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
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
use crate::inspect::job::is_test_file;
use crate::query_shape::{contains_all_content_tokens, extract_content_tokens};
use crate::search_index::{SearchIndex, SearchIndexSnapshot};

pub const DEFAULT_FALLBACK_FILE_LIMIT: usize = 1_000;
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
        let matches = snapshot.whole_corpus_exact_pass(
            query,
            project_root,
            Some(&|path| include_tests || !path.to_str().map_or(false, is_test_file)),
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
        let deadline = options.time_limit.map(|d| Instant::now() + d);

        // 1. Collect all project files
        let mut files = Vec::new();
        collect_files_recursive(project_root, &mut files);

        // Filter tests if needed
        if !include_tests {
            files.retain(|p| !p.to_str().map_or(false, is_test_file));
        }

        // Visit order: project-root-relative path, byte-wise ascending
        if options.force_directory_order {
            // Mutation red test: do not sort, keep directory order
        } else {
            files.sort_by(|a, b| {
                let rel_a = a.strip_prefix(project_root).unwrap_or(a);
                let rel_b = b.strip_prefix(project_root).unwrap_or(b);
                rel_a
                    .as_os_str()
                    .as_encoded_bytes()
                    .cmp(rel_b.as_os_str().as_encoded_bytes())
            });
        }

        let phrase = exact_phrase(query);
        let norm_phrase = normalize_exact_phrase(phrase);
        let content_tokens = extract_content_tokens(query);

        let mut results = Vec::new();
        let mut file_digests = HashMap::new();
        let mut files_visited = 0;
        let mut bound_reason: Option<String> = None;
        let mut stability_void = false;

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

            files_visited += 1;

            // Injected delay hook if configured
            if let Some(delay_fn) = &options.delay_hook {
                delay_fn(&file_path);
            }

            // Check match in file
            if let Ok(bytes) = fs::read(&file_path) {
                let digest = compute_content_digest(&bytes);
                file_digests.insert(file_path.clone(), digest);

                let text = String::from_utf8_lossy(&bytes);
                if let Some(candidates) =
                    verify_exact_matches_in_text(&file_path, &text, &norm_phrase, &content_tokens)
                {
                    results.extend(candidates);
                }
            }
        }

        let bound_disclosure = if let Some(ref reason) = bound_reason {
            if reason == "time limit" {
                Some(format!(
                    "exact pass: bounded ({files_visited} files, time limit) - page stability void"
                ))
            } else {
                Some(format!(
                    "exact pass: bounded ({files_visited} files, {reason})"
                ))
            }
        } else {
            // When index is not ready and walk completed without hitting other limits
            Some(format!(
                "exact pass: bounded ({files_visited} files, index not ready)"
            ))
        };

        results.sort_by(score_free_r3_cmp);

        FallbackExactResult {
            verified_set: VerifiedExactSet {
                results,
                file_digests,
                bound_disclosure,
                stability_void,
            },
            files_visited,
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
        let key = MemoKey::new(project_root, snapshot_generation, query, include_tests);

        let default_opts = FallbackExactOptions::default();
        let fallback_opts = fallback_options.unwrap_or(&default_opts);

        self.memo.get_or_verify(&key, offset, top_k, || {
            let is_ready = index.as_ref().is_some_and(|idx| idx.is_ready());
            if is_ready {
                let snapshot = index.unwrap().snapshot();
                Ok(self.execute_ready_mode(&snapshot, project_root, query, include_tests))
            } else {
                let fallback =
                    self.execute_fallback_mode(project_root, query, include_tests, fallback_opts);
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
        let mut candidates = self
            .execute_ready_mode(&snapshot, input.root, input.query, input.include_tests)
            .results;
        if input.shape != crate::commands::semantic_search::SearchShape::Identifier {
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

/// Recursively collect all regular files in a directory.
fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden or target/node_modules/git dirs
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.')
                    || name == "target"
                    || name == "node_modules"
                    || name == ".git"
                {
                    continue;
                }
            }
            collect_files_recursive(&path, files);
        } else if path.is_file() {
            files.push(path);
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
    let mut matches = Vec::new();
    let norm_text = normalize_exact_phrase(text);

    // 1. Check symbols first (for symbol-level candidates like `cap_chars`)
    let symbols = scan_symbols_in_text(text);
    for (name, range) in &symbols {
        let sym_text = &text[range.start..range.end.min(text.len())];
        let norm_sym_text = normalize_exact_phrase(sym_text);
        if !norm_phrase.is_empty() && norm_sym_text.contains(norm_phrase) {
            let occ = norm_sym_text.matches(norm_phrase).count();
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                Some(*range),
                EvidenceDescriptor::for_e1(occ, true, false),
            ));
        } else if !content_tokens.is_empty()
            && content_tokens
                .iter()
                .any(|t| name.to_ascii_lowercase() == *t)
        {
            // Symbol definition match for identifier / token
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                Some(*range),
                EvidenceDescriptor::for_definition(true, false),
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

    // 3. Check 3-line window match (E2) if no E1 match on file-level
    if matches.is_empty() && content_tokens.len() >= 2 {
        let lines: Vec<&str> = text.lines().collect();
        for width in 1..=3 {
            if lines.len() < width {
                continue;
            }
            if lines
                .windows(width)
                .any(|w| contains_all_content_tokens(&w.join("\n"), content_tokens))
            {
                matches.push(CandidateResult::new_exact(
                    file_path.to_path_buf(),
                    None,
                    EvidenceDescriptor::for_e2(width, true, false),
                ));
                break;
            }
        }
    }

    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

/// Lightweight symbol scanner finding functions / structs / classes in source files.
pub fn scan_symbols_in_text(text: &str) -> Vec<(String, SymbolOffsetRange)> {
    let mut symbols = Vec::new();
    let mut current_offset = 0;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let leading_spaces = line.len() - trimmed.len();
        let line_offset = current_offset + leading_spaces;

        let name_opt = if let Some(rest) = trimmed.strip_prefix("pub fn ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("fn ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub(crate) fn ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("def ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("function ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("pub struct ") {
            extract_identifier(rest)
        } else if let Some(rest) = trimmed.strip_prefix("struct ") {
            extract_identifier(rest)
        } else {
            None
        };

        if let Some(name) = name_opt {
            let start = line_offset;
            // The lightweight span is byte-addressed because downstream ranges
            // slice UTF-8 source. Clamp the approximate end to a character boundary.
            let mut end = (start + 500).min(text.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            symbols.push((name, SymbolOffsetRange::new(start, end)));
        }

        current_offset += line.len() + 1; // +1 for newline
    }

    symbols
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
