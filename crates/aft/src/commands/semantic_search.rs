use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::Deserialize;

use crate::commands::callgraph_store_adapter::callers_result;
use crate::commands::symbol_render::{
    build_container_outline, might_have_container_members, render_symbol_within_budget,
    BudgetedSymbolRenderStatus,
};
use crate::config::IndexKind;
use crate::context::{AppContext, SemanticIndexStatus};
use crate::grep_executor::{self, GrepParams};
use crate::inspect::job::{is_test_file, is_test_support_file};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::query_shape::{self, QueryKind, QueryShape};
use crate::readonly_artifacts::{GitRootResolutionError, ReadOnlyArtifact, ReadOnlyDegradation};
use crate::search_index::{
    sort_grep_matches_by_mtime_desc, try_read_with_budget, GrepMatch, GrepPathExclusion,
    GrepResult, IndexStatus, SearchIndex, SearchIndexSnapshot, INTERACTIVE_ARTIFACT_READ_BUDGET,
};
use crate::semantic_index::{
    query_embedding_timeout_budget, strip_query_embedding_timeout_marker, EmbeddingModel,
    QueryBudget, SemanticIndex, SemanticIndexFingerprint, SemanticResult,
};
use crate::symbols::{Range, Symbol, SymbolKind};

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;
const HYBRID_LEXICAL_BOOST: f32 = 1.1;
const LEXICAL_ONLY_SCORE_CEILING: f32 = 0.25;
const LEXICAL_ENUMERATION_LIMIT: usize = 50;
const GENERATED_DIRECTORY_DENSITY_NUMERATOR: usize = 3;
const GENERATED_DIRECTORY_DENSITY_DENOMINATOR: usize = 5;
const SEMANTIC_OVERFETCH_MULTIPLIER: usize = 3;
const SEMANTIC_OVERFETCH_FLOOR: usize = 10;
const DEGRADED_GREP_FILE_LIMIT: usize = 1_000;
const DEGRADED_GREP_RESULT_LIMIT: usize = 100;
const DEGRADED_GREP_WALK_BUDGET: Duration = Duration::from_secs(10);
/// Fresh borrowed trigram bases were measured ready 1.1–2.5 seconds after root
/// bind. Waiting through that observed window prevents a healthy first search
/// from returning an empty loading response while keeping a stuck load bounded.
const FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET: Duration = Duration::from_millis(2_500);
const SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(2);
const SUPPRESS_STATUS_BAR_FIELD: &str = "_aft_suppress_status_bar";

#[cfg(test)]
thread_local! {
    static FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

fn first_search_index_load_wait_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(std::cell::Cell::get) {
        return budget;
    }
    FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET
}

#[cfg(test)]
fn with_first_search_index_load_wait_budget_for_test<T>(
    budget: Duration,
    action: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<Duration>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(|slot| slot.set(self.0));
        }
    }

    let previous =
        FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(|slot| slot.replace(Some(budget)));
    let _reset = Reset(previous);
    action()
}
const BORROWED_SEARCH_LOAD_WARNING: &str = "Borrowed search index loading stopped at the interactive budget; returning a bounded lexical scan (no semantic ranking).";
const BORROWED_SEARCH_LOAD_FOOTER: &str = "[Degraded: borrowed search index loading stopped at the interactive budget; bounded lexical scan only.]";
const BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS: &str = "Semantic lane is loading the shared index; lexical results below are complete for exact/identifier matches.";
const STALE_CLI_SNAPSHOT_WARNING: &str = "Serving the last usable standing-root CLI snapshot after its freshness could not be verified; rerun `npx @cortexkit/aft index` to refresh it.";
/// Cap on the rank-0 full-symbol preview. Sized to absorb the follow-up zoom for
/// virtually every real function/type so the agent doesn't re-read a file it
/// already saw in search; a symbol exceeding it falls back to the line-budget
/// preview + "+N more lines". aft_zoom itself is uncapped, but search expansion is
/// automatic (not explicitly requested), so a runaway giant stays bounded.
const RANK0_FULL_SNIPPET_MAX_LINES: usize = 250;

/// Appended under the rank-0 snippet ONLY when the complete symbol was shown
/// (full expansion, not the capped preview). Tells the agent the body is the live
/// on-disk content so it can edit directly instead of spending a redundant
/// zoom/read — which is the entire point of the full-symbol expansion. It must
/// never appear on a partial preview (ranks 1-2, or the >cap fallback that ends
/// in "+N more lines"), where re-reading IS needed.
const RANK0_FULL_SYMBOL_NOTICE: &str =
    "full symbol shown as-is from disk; edit directly from this context — no need to re-read or zoom first";

#[derive(Debug, Clone)]
pub struct HybridResult {
    pub file: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
    pub score: f32,
    pub source: &'static str,
    pub semantic_score: Option<f32>,
    pub lexical_score: Option<f32>,
    pub hybrid_boosted: bool,
    pub(crate) cap_protected: bool,
    pub(crate) lexical_generated_artifact: bool,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
struct SemanticSearchParams {
    query: String,
    #[serde(default = "default_top_k", alias = "topK")]
    top_k: usize,
    #[serde(default, alias = "includeTests")]
    include_tests: bool,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Regex,
    Literal,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchIndexWaitError {
    Cancelled,
    Contended,
}

#[derive(Debug, Clone)]
struct LexicalCollection {
    files: Vec<(PathBuf, f32)>,
    ready: bool,
    engine_capped: bool,
    artifact_lock_timed_out: bool,
}

#[derive(Debug, Clone)]
struct LexicalCandidate {
    file: PathBuf,
    score: f32,
    generated_artifact: bool,
    ordinal: usize,
}

struct GeneratedArtifactCache<'a> {
    project_root: &'a Path,
    directory_cache: HashMap<PathBuf, bool>,
}

#[derive(Debug, Clone)]
struct DegradedGrepFallbackResult {
    grep: GrepResult,
    file_cap_reached: bool,
    file_limit: usize,
    candidate_files: usize,
    walk_budget_reached: bool,
}

#[derive(Debug, Clone, Default)]
struct ExternalBorrowMetadata {
    drift_count: usize,
    ignore_rules_differ: bool,
    degraded_reason: Option<&'static str>,
    standing_snapshot: bool,
    strict_verification_required: bool,
}

impl ExternalBorrowMetadata {
    fn record_drift(&mut self, drift_count: usize, ignore_rules_differ: bool) {
        self.drift_count = self.drift_count.max(drift_count);
        self.ignore_rules_differ |= ignore_rules_differ;
    }

    fn stale_cli_snapshot(&self) -> bool {
        self.standing_snapshot && (self.strict_verification_required || self.drift_count > 0)
    }
}

fn standing_snapshot_metadata(
    external_root: &Path,
    storage_dir: Option<&Path>,
) -> ExternalBorrowMetadata {
    let db_path = crate::bash_background::storage_dir(storage_dir).join("aft.db");
    if !db_path.is_file() {
        return ExternalBorrowMetadata::default();
    }

    let resolved_target = std::fs::canonicalize(external_root)
        .unwrap_or_else(|_| external_root.to_path_buf())
        .display()
        .to_string();
    let Ok(conn) = crate::db::open(&db_path) else {
        return ExternalBorrowMetadata::default();
    };
    let Ok(needs_strict_verify) =
        crate::db::standing_roots::needs_strict_verify_for_resolved_target(
            &conn,
            &resolved_target,
            IndexKind::Search,
        )
    else {
        return ExternalBorrowMetadata::default();
    };

    let Some(strict_verification_required) = needs_strict_verify else {
        return ExternalBorrowMetadata::default();
    };
    ExternalBorrowMetadata {
        standing_snapshot: true,
        strict_verification_required,
        ..ExternalBorrowMetadata::default()
    }
}

pub(crate) fn search_cancellation_requested() -> bool {
    crate::executor::current_job_cancelled()
}

fn cancelled_search_response(req: &RawRequest) -> Response {
    cancelled_search_response_from_id(&req.id)
}

fn cancelled_search_response_from_id(request_id: &str) -> Response {
    Response::error(
        request_id,
        "request_cancelled",
        "Search request cancelled because its route closed.",
    )
}

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let mut params = match serde_json::from_value::<SemanticSearchParams>(req.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("semantic_search: invalid params: {error}"),
            );
        }
    };

    if params.query.trim().is_empty() {
        return Response::error(&req.id, "invalid_request", "query must be non-empty");
    }

    // Strip a single pair of surrounding paired quotes from the literal needle.
    // Many agents and humans reach for the GitHub-code-search / `rg -F "..."`
    // convention of quoting a phrase, but AFT does pure substring matching by
    // default, so the quotes themselves become part of the needle and silently
    // produce zero results. Strip only matched leading+trailing pairs of `"`
    // or `'` (no escape handling — agents that genuinely want literal quotes
    // can pass `\"foo\"`-style content which won't be a balanced outer pair).
    params.query = strip_surrounding_quotes(params.query);
    if params.query.trim().is_empty() {
        return Response::error(&req.id, "invalid_request", "query must be non-empty");
    }

    let top_k = params.top_k.clamp(1, MAX_TOP_K);
    let project_root = grep_executor::project_root(ctx);
    let shape = query_shape::classify(&params.query);
    let requested_path = params
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string);
    if let Some(requested_path) = requested_path {
        let external_root = match ctx.resolve_external_git_root(&project_root, &requested_path) {
            Ok(root) => root,
            Err(GitRootResolutionError::PathNotFound(path)) => {
                return Response::error(
                    &req.id,
                    "path_not_found",
                    format!("path does not exist: {}", path.display()),
                );
            }
            Err(GitRootResolutionError::NotAGitRoot) => {
                return Response::error(
                    &req.id,
                    "not_a_git_root",
                    format!("path is not inside a git repository: {requested_path}"),
                );
            }
            Err(GitRootResolutionError::Other(error)) => {
                return Response::error(&req.id, "path_resolution_failed", error);
            }
        };

        if external_root != project_root {
            if ctx.config().restrict_to_project_root || ctx.request_force_restrict(&req.id) {
                return Response::error(
                    &req.id,
                    "path_outside_root",
                    format!(
                        "aft_search path is outside the configured project root while path restriction is enabled: {}",
                        external_root.display()
                    ),
                );
            }
            return handle_external_search(req, ctx, params, top_k, shape, external_root);
        }
    }
    let semantic_status_snapshot = match try_read_with_budget(
        ctx.semantic_index_status(),
        INTERACTIVE_ARTIFACT_READ_BUDGET,
    ) {
        Some(status) => status.clone(),
        None => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                &project_root,
                top_k,
                "semantic index status remained busy",
            );
        }
    };
    let semantic_status = semantic_status_label(&semantic_status_snapshot);
    let mut warnings = Vec::new();

    let search_index_wait_budget = match &semantic_status_snapshot {
        SemanticIndexStatus::Building { stage, .. } if stage == "loading_artifacts" => {
            first_search_index_load_wait_budget()
        }
        _ => INTERACTIVE_ARTIFACT_READ_BUDGET,
    };
    let lexical_ready = match search_index_ready_with_budget(ctx, search_index_wait_budget) {
        Ok(ready) => ready,
        Err(SearchIndexWaitError::Cancelled) => return cancelled_search_response(req),
        Err(SearchIndexWaitError::Contended) => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                &project_root,
                top_k,
                "search index remained busy",
            );
        }
    };
    let mode = choose_mode(&params.query, &shape, lexical_ready, &mut warnings);

    match mode {
        SearchMode::Regex | SearchMode::Literal => handle_grep_search(
            req,
            ctx,
            &params.query,
            top_k,
            &shape,
            mode,
            semantic_status,
            warnings,
            &project_root,
            params.include_tests,
        ),
        SearchMode::Semantic | SearchMode::Hybrid => handle_semantic_or_hybrid_search(
            req,
            ctx,
            params,
            top_k,
            shape,
            mode,
            lexical_ready,
            semantic_status_snapshot,
            semantic_status,
            warnings,
            &project_root,
        ),
    }
}

fn handle_external_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    top_k: usize,
    shape: QueryShape,
    external_root: PathBuf,
) -> Response {
    let storage_dir = ctx.config().storage_dir.clone();
    let mut borrow_metadata = if ctx.daemonless_query_mode() {
        standing_snapshot_metadata(&external_root, storage_dir.as_deref())
    } else {
        ExternalBorrowMetadata::default()
    };
    let search_index = match ctx.open_borrowed_search_index(&external_root, storage_dir.as_deref())
    {
        ReadOnlyArtifact::Fresh(index) => index,
        ReadOnlyArtifact::Degraded(degradation) => {
            return handle_external_borrowed_degraded_fallback(
                req,
                ctx,
                &params,
                top_k,
                &shape,
                &external_root,
                degradation,
            );
        }
        ReadOnlyArtifact::Cancelled => return cancelled_search_response(req),
        ReadOnlyArtifact::Absent => {
            // No index exists for this foreign root. Rather than dead-ending
            // with a not_indexed error (which reads to an agent as "this tool
            // can't help here" and pushes it to shell out to grep/bash), do a
            // bounded, best-effort lexical scan of the root — exactly what the
            // `grep` tool already does for an unindexed external root — and
            // disclose that it is lexical-only. Removing the error branch
            // removes the escape hatch.
            return handle_external_unindexed_fallback(
                req,
                ctx,
                &params,
                top_k,
                &shape,
                &external_root,
            );
        }
        ReadOnlyArtifact::Stale(stale) => {
            borrow_metadata.record_drift(stale.drift_count, stale.ignore_rules_differ);
            crate::slog_warn!(
                "{}",
                borrowed_drift_log_message("search", &external_root, stale.drift_count)
            );
            stale.index
        }
    };
    if borrow_metadata.standing_snapshot {
        borrow_metadata.record_drift(search_index.borrowed_stat_mismatch_count(), false);
    }

    let mut warnings = Vec::new();
    if borrow_metadata.stale_cli_snapshot() {
        warnings.push(STALE_CLI_SNAPSHOT_WARNING.to_string());
    }
    let mode = choose_mode(&params.query, &shape, true, &mut warnings);

    match mode {
        SearchMode::Regex | SearchMode::Literal => handle_external_grep_search(
            req,
            &params.query,
            top_k,
            &shape,
            mode,
            warnings,
            &external_root,
            params.include_tests,
            &search_index,
            &borrow_metadata,
        ),
        SearchMode::Semantic | SearchMode::Hybrid => handle_external_semantic_or_hybrid_search(
            req,
            ctx,
            params,
            top_k,
            shape,
            mode,
            warnings,
            external_root,
            search_index,
            borrow_metadata,
        ),
    }
}

/// Bounded lexical scan of a foreign git root that has no borrowable AFT
/// index. Mirrors the `grep` tool's unindexed-external behavior (fallback
/// filesystem walk via the shared `grep_executor`) so `aft_search` degrades to
/// real results with a disclosure instead of a hard `not_indexed` error.
/// Semantic/hybrid intent cannot run without embeddings, so every mode degrades
/// to a lexical substring/regex scan here.
fn handle_external_unindexed_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
) -> Response {
    handle_external_bounded_lexical_fallback(req, ctx, params, top_k, shape, external_root, None)
}

fn handle_external_borrowed_degraded_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
    degradation: ReadOnlyDegradation,
) -> Response {
    handle_external_bounded_lexical_fallback(
        req,
        ctx,
        params,
        top_k,
        shape,
        external_root,
        Some(degradation),
    )
}

fn handle_external_bounded_lexical_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
    degradation: Option<ReadOnlyDegradation>,
) -> Response {
    let borrow_metadata = ExternalBorrowMetadata::default();
    let literal = true;
    let compiled = match pattern_compile::compile(
        &params.query,
        CompileOpts {
            literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            return Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                external_response_extras(external_root, &borrow_metadata),
            );
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                external_response_extras(external_root, &borrow_metadata),
            );
        }
    };

    let path_value = serde_json::Value::String(external_root.to_string_lossy().into_owned());
    let scope = match grep_executor::resolve_grep_scope(ctx, Some(&path_value), top_k, &req.id) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let grep_params = grep_executor::GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: top_k,
        path_exclusion: grep_path_exclusion(params.include_tests),
    };
    let result = grep_executor::execute(ctx, &compiled, &scope, &grep_params);
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }

    let result_source = if literal { "literal" } else { "regex" };
    let interpreted_as = if literal { "literal" } else { "regex" };
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let display_root = absolute_display_root(external_root);
    let mut text = format_grep_search_text(&result, &display_root, interpreted_as);
    if degradation.is_some() {
        text.push_str("\n\n");
        text.push_str(BORROWED_SEARCH_LOAD_FOOTER);
    }

    let mut warnings = Vec::new();
    if degradation.is_some() {
        warnings.push(BORROWED_SEARCH_LOAD_WARNING.to_string());
    } else {
        warnings.push(format!(
            "No AFT index exists for {} — returning a bounded lexical scan (no semantic ranking). Open a session in that project for full indexed search.",
            external_root.display()
        ));
    }
    if result.walk_truncated {
        warnings.push(
            "Lexical scan stopped early (file-count or time budget reached); results may be incomplete.".to_string(),
        );
    }

    let mut extras = external_response_extras(external_root, &borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(degradation) = degradation {
        extras.insert(
            "borrowed_index_degraded_reason".to_string(),
            serde_json::json!(degradation.reason),
        );
    }
    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status: if degradation.is_some() {
                "external_borrowed_degraded"
            } else {
                "external_unindexed"
            },
            status: "ready",
            complete: degradation.is_none() && !result.walk_truncated,
            text,
            results: result_values,
            more_available: result.walk_truncated
                || result.truncated
                || result.total_matches > result.matches.len(),
            engine_capped: result.engine_capped,
            fully_degraded: true,
            warnings,
            extras,
        },
    )
}

fn handle_external_grep_search(
    req: &RawRequest,
    query: &str,
    top_k: usize,
    shape: &QueryShape,
    mode: SearchMode,
    mut warnings: Vec<String>,
    external_root: &Path,
    include_tests: bool,
    search_index: &SearchIndex,
    borrow_metadata: &ExternalBorrowMetadata,
) -> Response {
    let auto_regex = mode == SearchMode::Regex;
    let mut effective_mode = mode;
    let compile_literal_fallback = || -> Result<_, Response> {
        match pattern_compile::compile(
            query,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => Ok(compiled),
            CompileResult::InvalidPattern { message, .. } => Err(Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                external_response_extras(external_root, borrow_metadata),
            )),
            CompileResult::UnsupportedSyntax { feature, .. } => Err(Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                external_response_extras(external_root, borrow_metadata),
            )),
        }
    };

    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: mode == SearchMode::Literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(
                    short_regex_compile_reason(&message),
                ));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "invalid_pattern",
                    message,
                    external_response_extras(external_root, borrow_metadata),
                );
            }
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(format!(
                    "{feature} is not supported"
                )));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "unsupported_pattern",
                    format!(
                        "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                    ),
                    external_response_extras(external_root, borrow_metadata),
                );
            }
        }
    };

    let literal = effective_mode == SearchMode::Literal;
    let mut result = search_index.snapshot().search_grep_bounded(
        &compiled,
        &[],
        &[],
        external_root,
        top_k,
        grep_path_exclusion(include_tests),
        grep_executor::MAX_FALLBACK_WALK_FILES,
        grep_executor::FALLBACK_WALK_BUDGET,
    );
    result
        .matches
        .retain(|grep_match| grep_match.file.is_file());

    if result.matches.is_empty() {
        let lexical = collect_lexical_files_from_snapshot(
            Some(search_index.snapshot()),
            query,
            shape,
            include_tests,
            external_root,
        );
        let extras = external_response_extras(external_root, borrow_metadata)
            .as_object()
            .cloned()
            .unwrap_or_default();
        return stale_cli_snapshot_partial_response(
            zero_result_escalation_response(
                req,
                query,
                shape,
                effective_mode,
                "external",
                warnings,
                lexical,
                top_k,
                include_tests,
                external_root,
                &absolute_display_root(external_root),
                None,
                extras,
            ),
            borrow_metadata.stale_cli_snapshot(),
        );
    }

    let result_source = if literal { "literal" } else { "regex" };
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let interpreted_as = interpreted_as_label(effective_mode);
    let display_root = absolute_display_root(external_root);
    let text = format_grep_search_text(&result, &display_root, interpreted_as);
    let extras = external_response_extras(external_root, borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default();
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status: "external",
            status: "ready",
            complete: !borrow_metadata.stale_cli_snapshot(),
            text,
            results: result_values,
            more_available: result.truncated || result.total_matches > result.matches.len(),
            engine_capped: result.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn handle_external_semantic_or_hybrid_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    top_k: usize,
    shape: QueryShape,
    mode: SearchMode,
    mut warnings: Vec<String>,
    external_root: PathBuf,
    search_index: Arc<SearchIndex>,
    mut borrow_metadata: ExternalBorrowMetadata,
) -> Response {
    let lexical = if mode == SearchMode::Hybrid {
        collect_lexical_files_from_snapshot(
            Some(search_index.snapshot()),
            &params.query,
            &shape,
            params.include_tests,
            &external_root,
        )
    } else {
        LexicalCollection {
            files: Vec::new(),
            ready: true,
            engine_capped: false,
            artifact_lock_timed_out: false,
        }
    };

    let storage_dir = ctx.config().storage_dir.clone();
    let semantic_index = match ctx
        .open_borrowed_semantic_index(&external_root, storage_dir.as_deref())
    {
        ReadOnlyArtifact::Fresh(index) => match semantic_fingerprint_matches_session(ctx, &index) {
            true => Some(index),
            false => {
                warnings.push(
                    "External semantic index was built for a different embedding backend or model; returning lexical-only results from the trigram index.".to_string(),
                );
                None
            }
        },
        ReadOnlyArtifact::Stale(stale) => {
            borrow_metadata.record_drift(stale.drift_count, stale.ignore_rules_differ);
            crate::slog_warn!(
                "{}",
                borrowed_drift_log_message("semantic", &external_root, stale.drift_count)
            );
            if semantic_fingerprint_matches_session(ctx, &stale.index) {
                Some(stale.index)
            } else {
                warnings.push(
                    "External semantic index was built for a different embedding backend or model; returning lexical-only results from the trigram index.".to_string(),
                );
                None
            }
        }
        ReadOnlyArtifact::Degraded(degradation) => {
            warnings.push(
                "Borrowed semantic index loading stopped at the interactive budget; returning lexical-only results from the trigram index.".to_string(),
            );
            borrow_metadata.degraded_reason = Some(degradation.reason);
            None
        }
        ReadOnlyArtifact::Cancelled => return cancelled_search_response(req),
        ReadOnlyArtifact::Absent => {
            warnings.push(
                "External semantic index is not available; returning lexical-only results from the trigram index.".to_string(),
            );
            None
        }
    };

    let Some(semantic_index) = semantic_index else {
        // Hybrid mode already ranked the lexical candidates above; ranking is
        // the expensive half of an external query, so reuse it and collect
        // only when the original mode was pure Semantic.
        let lexical = if mode == SearchMode::Hybrid {
            lexical
        } else {
            collect_lexical_files_from_snapshot(
                Some(search_index.snapshot()),
                &params.query,
                &shape,
                params.include_tests,
                &external_root,
            )
        };
        return external_lexical_only_response(
            req,
            &params,
            mode,
            &shape,
            "Semantic search is not available for the external root.".to_string(),
            "unavailable",
            lexical,
            warnings,
            &external_root,
            top_k,
            &borrow_metadata,
        );
    };

    let query_vector =
        match embed_query_for_dimension(&params.query, ctx, Some(semantic_index.dimension())) {
            Ok(query_vector) => query_vector,
            Err(error) => {
                let lexical = if mode == SearchMode::Hybrid {
                    lexical
                } else {
                    collect_lexical_files_from_snapshot(
                        Some(search_index.snapshot()),
                        &params.query,
                        &shape,
                        params.include_tests,
                        &external_root,
                    )
                };
                let classified = classify_embed_query_error(&error);
                return external_lexical_only_response(
                    req,
                    &params,
                    mode,
                    &shape,
                    classified.detail,
                    classified.footer_reason,
                    lexical,
                    warnings,
                    &external_root,
                    top_k,
                    &borrow_metadata,
                );
            }
        };

    let semantic_limit = if params.include_tests {
        semantic_candidate_limit(top_k)
    } else {
        MAX_TOP_K
    };
    let semantic_fetch_limit = semantic_limit.saturating_add(1);
    let mut semantic_results =
        semantic_index.search_filtered(&query_vector, semantic_fetch_limit, |file| {
            path_allowed_by_include_tests(file, &external_root, params.include_tests)
        });
    semantic_results.retain(|result| result.file.is_file());
    let semantic_more_available = semantic_results.len() > semantic_limit;
    if semantic_more_available {
        semantic_results.truncate(semantic_limit);
    }
    rerank_semantic_candidates(&mut semantic_results, &shape, &params.query);

    let mut results = fuse_hybrid_results(
        semantic_results,
        lexical.files,
        &shape,
        top_k.saturating_add(1),
        params.include_tests,
        &external_root,
    );
    results.retain(|result| result.file.is_file());
    let fused_more_available = results.len() > top_k;
    if fused_more_available {
        results.truncate(top_k);
    }
    let more_available = fused_more_available || semantic_more_available || lexical.engine_capped;

    if mode == SearchMode::Semantic
        && shape.kind == QueryKind::NaturalLanguage
        && results.is_empty()
        && lexical.ready
    {
        let escalation_lexical = collect_lexical_files_from_snapshot(
            Some(search_index.snapshot()),
            &params.query,
            &shape,
            params.include_tests,
            &external_root,
        );
        let extras = external_response_extras(&external_root, &borrow_metadata)
            .as_object()
            .cloned()
            .unwrap_or_default();
        return stale_cli_snapshot_partial_response(
            zero_result_escalation_response(
                req,
                &params.query,
                &shape,
                mode,
                "ready",
                warnings,
                escalation_lexical,
                top_k,
                params.include_tests,
                &external_root,
                &absolute_display_root(&external_root),
                None,
                extras,
            ),
            borrow_metadata.stale_cli_snapshot(),
        );
    }

    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut results, &external_root, None);
    let display_root = absolute_display_root(&external_root);

    let text = format_semantic_text_with_display_root(
        &results,
        &display_root,
        more_available,
        snippets_incomplete,
        None,
    );

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status: "ready",
            status: "ready",
            complete: !borrow_metadata.stale_cli_snapshot(),
            text,
            results: results.iter().map(result_to_json).collect::<Vec<_>>(),
            more_available,
            engine_capped: lexical.engine_capped,
            fully_degraded: false,
            warnings,
            extras: external_response_extras(&external_root, &borrow_metadata)
                .as_object()
                .cloned()
                .unwrap_or_default(),
        },
    )
}

fn external_lexical_only_response(
    req: &RawRequest,
    params: &SemanticSearchParams,
    mode: SearchMode,
    shape: &QueryShape,
    detail: String,
    footer_reason: &str,
    lexical: LexicalCollection,
    mut warnings: Vec<String>,
    external_root: &Path,
    top_k: usize,
    borrow_metadata: &ExternalBorrowMetadata,
) -> Response {
    let lexical_count = lexical.files.len();
    let lexical_engine_capped = lexical.engine_capped;
    let mut results = fuse_hybrid_results(
        Vec::new(),
        lexical.files,
        shape,
        top_k,
        params.include_tests,
        external_root,
    );
    results.retain(|result| result.file.is_file());
    let result_values = results.iter().map(result_to_json).collect::<Vec<_>>();
    warnings.push(
        "Semantic search unavailable for external root; returning lexical-only fallback results."
            .to_string(),
    );
    let display_root = absolute_display_root(external_root);
    let text = format_lexical_unavailable_text(&detail, &results, &display_root, footer_reason);
    let mut extras = semantic_unavailable_extras(true);
    for (key, value) in external_response_extras(external_root, borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default()
    {
        extras.insert(key, value);
    }

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: fallback_executed_label(mode, true),
            query_kind: query_kind_label(shape.kind),
            semantic_status: "unavailable",
            status: "ready",
            complete: false,
            text,
            results: result_values,
            more_available: lexical_count > top_k || lexical_engine_capped,
            engine_capped: lexical_engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn semantic_fingerprint_matches_session(ctx: &AppContext, index: &SemanticIndex) -> bool {
    let config = ctx.config().semantic.clone();
    let expected = SemanticIndexFingerprint::for_config_dimension(&config, index.dimension());
    index
        .fingerprint()
        .map(|fingerprint| fingerprint.as_string() == expected.as_string())
        .unwrap_or(false)
}

fn stale_cli_snapshot_partial_response(
    mut response: Response,
    stale_cli_snapshot: bool,
) -> Response {
    if stale_cli_snapshot {
        response.data["complete"] = serde_json::Value::Bool(false);
    }
    response
}

fn external_response_extras(
    external_root: &Path,
    borrow_metadata: &ExternalBorrowMetadata,
) -> serde_json::Value {
    let mut extras = serde_json::json!({
        "external_root": external_root.display().to_string(),
        "borrowed": true,
        "drift_count": borrow_metadata.drift_count,
        "ignore_rules_differ": borrow_metadata.ignore_rules_differ,
        SUPPRESS_STATUS_BAR_FIELD: true,
    });
    if let (Some(reason), Some(object)) = (borrow_metadata.degraded_reason, extras.as_object_mut())
    {
        object.insert(
            "borrowed_index_degraded_reason".to_string(),
            serde_json::json!(reason),
        );
    }
    extras
}

fn borrowed_drift_log_message(index_kind: &str, root: &Path, drift_count: usize) -> String {
    format!(
        "borrowed {index_kind} index for {} has {drift_count} drifted file(s); serving current-disk snippets without agent-facing stale prose",
        root.display()
    )
}

fn absolute_display_root(root: &Path) -> PathBuf {
    root.join(".aft-external-display-root-nonprefix")
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

fn semantic_candidate_limit(top_k: usize) -> usize {
    top_k
        .saturating_mul(SEMANTIC_OVERFETCH_MULTIPLIER)
        .clamp(SEMANTIC_OVERFETCH_FLOOR, MAX_TOP_K)
}

fn project_relative_path<'a>(path: &'a Path, project_root: &'a Path) -> &'a Path {
    path.strip_prefix(project_root).unwrap_or(path)
}

fn path_is_test_support_file(path: &Path, project_root: &Path) -> bool {
    let relative = project_relative_path(path, project_root);
    is_test_support_file(relative.to_string_lossy().as_ref())
}

/// Whether `path` is something `aft_search` hides unless `include_tests` is set:
/// a test-support file (fixtures/mocks/snapshots) OR an actual test file
/// (`*.test.ts`, `__tests__/`, `*_test.rs`, …). Search is a code-discovery tool,
/// so test code is noise by default; `include_tests: true` shows both classes.
fn path_is_hidden_test_file(path: &Path, project_root: &Path) -> bool {
    let relative = project_relative_path(path, project_root);
    let rel = relative.to_string_lossy();
    is_test_support_file(rel.as_ref()) || is_test_file(rel.as_ref())
}

fn path_allowed_by_include_tests(path: &Path, project_root: &Path, include_tests: bool) -> bool {
    include_tests || !path_is_hidden_test_file(path, project_root)
}

fn grep_path_exclusion(include_tests: bool) -> Option<GrepPathExclusion> {
    (!include_tests).then_some(path_is_hidden_test_file)
}

fn path_has_unambiguous_generated_artifact_type(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    if file_name.ends_with(".min.js") || file_name.ends_with(".min.css") {
        return true;
    }

    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("html" | "htm" | "css" | "map")
    )
}

fn path_has_ambiguous_generated_artifact_type(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("json" | "svg" | "docset")
    )
}

fn directory_is_generated_artifact_dominated(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    let mut file_count = 0usize;
    let mut generated_count = 0usize;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        file_count += 1;
        if path_has_unambiguous_generated_artifact_type(&entry.path()) {
            generated_count += 1;
        }
    }

    file_count > 0
        && generated_count * GENERATED_DIRECTORY_DENSITY_DENOMINATOR
            >= file_count * GENERATED_DIRECTORY_DENSITY_NUMERATOR
}

impl<'a> GeneratedArtifactCache<'a> {
    fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            directory_cache: HashMap::new(),
        }
    }

    fn is_generated_artifact(&mut self, path: &Path) -> bool {
        if path_has_unambiguous_generated_artifact_type(path) {
            return true;
        }
        if !path_has_ambiguous_generated_artifact_type(path) {
            return false;
        }

        let path_is_inside_project = path.starts_with(self.project_root);
        let mut current = path.parent();
        while let Some(dir) = current {
            if path_is_inside_project && !dir.starts_with(self.project_root) {
                break;
            }
            if self.directory_is_generated_artifact_dominated(dir) {
                return true;
            }
            if path_is_inside_project && dir == self.project_root {
                break;
            }
            current = dir.parent();
        }
        false
    }

    fn directory_is_generated_artifact_dominated(&mut self, dir: &Path) -> bool {
        if let Some(cached) = self.directory_cache.get(dir) {
            return *cached;
        }

        let dominated = directory_is_generated_artifact_dominated(dir);
        self.directory_cache.insert(dir.to_path_buf(), dominated);
        dominated
    }
}

fn lexical_candidates_with_generated_artifact_rank(
    lexical_files: Vec<(PathBuf, f32)>,
    project_root: &Path,
) -> Vec<LexicalCandidate> {
    let mut generated_cache = GeneratedArtifactCache::new(project_root);
    let mut candidates = lexical_files
        .into_iter()
        .enumerate()
        .map(|(ordinal, (file, score))| {
            let generated_artifact = generated_cache.is_generated_artifact(&file);
            LexicalCandidate {
                file,
                score,
                generated_artifact,
                ordinal,
            }
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|a, b| {
        a.generated_artifact
            .cmp(&b.generated_artifact)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.ordinal.cmp(&b.ordinal))
    });
    candidates
}

fn choose_mode(
    query: &str,
    shape: &QueryShape,
    lexical_ready: bool,
    warnings: &mut Vec<String>,
) -> SearchMode {
    if shape.kind == QueryKind::Regex {
        return SearchMode::Regex;
    }
    if shape.kind != QueryKind::NaturalLanguage && extracted_tokens_all_short(query, shape) {
        warnings.push(
            "Auto mode is using literal full-file scan for all-short exact tokens because the trigram index cannot rank tokens shorter than 3 chars.".to_string(),
        );
        return SearchMode::Literal;
    }
    if shape.kind == QueryKind::NaturalLanguage {
        // Short NL concepts (e.g. "parse imports", "retry backoff") are
        // frequently literal code tokens the trigram lane nails exactly.
        // Run them as Hybrid so lexical still contributes; only longer
        // NL phrases go pure semantic. One extra trigram lookup.
        let word_count = query.split_whitespace().count();
        if lexical_ready && word_count <= 2 {
            return SearchMode::Hybrid;
        }
        return SearchMode::Semantic;
    }
    if lexical_ready {
        SearchMode::Hybrid
    } else {
        warnings
            .push("Lexical trigram index is unavailable; using semantic search only.".to_string());
        SearchMode::Semantic
    }
}

fn handle_grep_search(
    req: &RawRequest,
    ctx: &AppContext,
    query: &str,
    top_k: usize,
    shape: &QueryShape,
    mode: SearchMode,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
    include_tests: bool,
) -> Response {
    let auto_regex = mode == SearchMode::Regex;
    let mut effective_mode = mode;
    let compile_literal_fallback = || -> Result<_, Response> {
        match pattern_compile::compile(
            query,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => Ok(compiled),
            CompileResult::InvalidPattern { message, .. } => Err(Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                serde_json::json!({"pattern": query}),
            )),
            CompileResult::UnsupportedSyntax { feature, .. } => Err(Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            )),
        }
    };

    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: mode == SearchMode::Literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(
                    short_regex_compile_reason(&message),
                ));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "invalid_pattern",
                    message,
                    serde_json::json!({"pattern": query}),
                );
            }
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(format!(
                    "{feature} is not supported"
                )));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "unsupported_pattern",
                    format!(
                        "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                    ),
                    serde_json::json!({"pattern": query, "feature": feature}),
                );
            }
        }
    };

    let literal = effective_mode == SearchMode::Literal;
    let scope = match grep_executor::resolve_grep_scope(ctx, None, top_k, &req.id) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let params = GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: top_k,
        path_exclusion: grep_path_exclusion(include_tests),
    };
    let result = grep_executor::execute(ctx, &compiled, &scope, &params);
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }

    let result_source = if literal { "literal" } else { "regex" };
    if result.matches.is_empty() && search_index_ready(ctx) {
        let lexical = collect_lexical_files(ctx, query, shape, include_tests, project_root);
        return zero_result_escalation_response(
            req,
            query,
            shape,
            effective_mode,
            semantic_status,
            warnings,
            lexical,
            top_k,
            include_tests,
            project_root,
            project_root,
            Some(ctx),
            serde_json::Map::new(),
        );
    }

    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let interpreted_as = interpreted_as_label(effective_mode);
    let text = format_grep_search_text(&result, project_root, interpreted_as);
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text,
            results: result_values,
            more_available: result.truncated || result.total_matches > result.matches.len(),
            engine_capped: result.engine_capped,
            fully_degraded: result.fully_degraded,
            warnings,
            extras: serde_json::Map::new(),
        },
    )
}

fn short_regex_compile_reason(message: &str) -> Cow<'_, str> {
    let trimmed = message.trim();
    let reason = trimmed
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.chars().all(|ch| ch == '^'))
        .unwrap_or(trimmed);
    Cow::Borrowed(
        reason
            .strip_prefix("error: ")
            .or_else(|| reason.strip_prefix("invalid regex: "))
            .unwrap_or(reason),
    )
}

fn auto_regex_literal_fallback_warning(reason: impl AsRef<str>) -> String {
    format!(
        "Query looked like a regex but failed to compile ({}); searched literally instead. Use grep when explicit regex lane control is required.",
        reason.as_ref()
    )
}

fn handle_semantic_or_hybrid_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    top_k: usize,
    shape: QueryShape,
    mode: SearchMode,
    lexical_ready: bool,
    status: SemanticIndexStatus,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
) -> Response {
    let lexical = if mode == SearchMode::Hybrid {
        collect_lexical_files(
            ctx,
            &params.query,
            &shape,
            params.include_tests,
            project_root,
        )
    } else {
        LexicalCollection {
            files: Vec::new(),
            ready: lexical_ready,
            engine_capped: false,
            artifact_lock_timed_out: false,
        }
    };

    if lexical.artifact_lock_timed_out {
        return artifact_contention_fallback_response(
            req,
            ctx,
            &params,
            &shape,
            project_root,
            top_k,
            "search index remained busy",
        );
    }

    match status {
        SemanticIndexStatus::Disabled => {
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "disabled",
                "disabled",
                "Semantic search is not enabled.".to_string(),
                "disabled",
                false,
                lexical,
                warnings,
                project_root,
                top_k,
            );
        }
        SemanticIndexStatus::Failed(error) => {
            let retrying_read_only_snapshot = ctx.shared_artifacts_read_only()
                && super::configure::trigger_semantic_index_reload_if_evicted(ctx);
            let (semantic_status, status, detail, footer_reason) = if retrying_read_only_snapshot {
                (
                    "building",
                    "reloading",
                    "Semantic index is reloading from the shared read-only snapshot; retry shortly."
                        .to_string(),
                    "reloading",
                )
            } else {
                (
                    "unavailable",
                    "unavailable",
                    format!("Semantic search unavailable: {error}"),
                    "unavailable",
                )
            };
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                semantic_status,
                status,
                detail,
                footer_reason,
                false,
                lexical,
                warnings,
                project_root,
                top_k,
            );
        }
        SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
        } => {
            let borrowed_loading = stage == "loading_artifacts" && ctx.shared_artifacts_read_only();
            let mut detail = format!("Semantic index is still building (stage: {}).", stage);
            if let Some(files) = files {
                detail.push_str(&format!(" files: {}", files));
            }
            if let Some(entries_done) = entries_done {
                detail.push_str(&format!(" entries done: {}", entries_done));
            }
            if let Some(entries_total) = entries_total {
                detail.push_str(&format!(" / {}", entries_total));
            }

            if natural_language_degraded_fallback_available(mode, &shape) {
                return semantic_unavailable_grep_fallback_response(
                    req,
                    ctx,
                    &params,
                    &shape,
                    "building",
                    detail,
                    if borrowed_loading {
                        "loading"
                    } else {
                        "building"
                    },
                    borrowed_loading,
                    warnings,
                    project_root,
                    top_k,
                );
            }

            let lexical_count = lexical.files.len();
            let lexical_engine_capped = lexical.engine_capped;
            let results = fuse_hybrid_results(
                Vec::new(),
                lexical.files,
                &shape,
                top_k,
                params.include_tests,
                project_root,
            );
            let result_values = results.iter().map(result_to_json).collect::<Vec<_>>();
            let note = building_lexical_note(borrowed_loading && !results.is_empty());
            let mut extras = serde_json::Map::new();
            extras.insert("stage".to_string(), serde_json::json!(stage));
            extras.insert("files".to_string(), serde_json::json!(files));
            extras.insert("entries_done".to_string(), serde_json::json!(entries_done));
            extras.insert(
                "entries_total".to_string(),
                serde_json::json!(entries_total),
            );
            extras.insert("note".to_string(), serde_json::json!(note));
            extras.insert(
                "semantic_rebuilding".to_string(),
                serde_json::json!(!borrowed_loading),
            );
            if borrowed_loading {
                extras.insert(
                    "semantic_loading_shared".to_string(),
                    serde_json::json!(true),
                );
            }
            extras.insert(
                "lexical_only_fallback".to_string(),
                serde_json::json!(lexical.ready),
            );

            return search_response(
                req,
                SearchResponseParts {
                    query: &params.query,
                    // While semantic rebuilds, only the lexical lane produced
                    // these results (semantic input is empty here). Report
                    // "lexical" when it ran; the "building" status + the
                    // semantic_rebuilding/lexical_only_fallback extras tell the
                    // agent semantic results are still coming.
                    interpreted_as: fallback_executed_label(mode, lexical.ready),
                    query_kind: query_kind_label(shape.kind),
                    semantic_status: "building",
                    status: "building",
                    complete: false,
                    text: format_building_lexical_text(
                        &detail,
                        &results,
                        project_root,
                        lexical.ready,
                        stage == "loading_artifacts",
                        borrowed_loading,
                    ),
                    results: result_values,
                    more_available: lexical_count > top_k || lexical_engine_capped,
                    engine_capped: lexical_engine_capped,
                    fully_degraded: false,
                    warnings,
                    extras,
                },
            );
        }
        SemanticIndexStatus::Ready { refreshing, .. } => {
            if !refreshing.is_empty() {
                warnings.push(format!(
                    "{} file(s) refreshing; results for those files may be temporarily missing",
                    refreshing.len()
                ));
            }
        }
    }

    let semantic_loaded = match semantic_index_loaded_with_budget(ctx) {
        Ok(loaded) => loaded,
        Err(()) => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                project_root,
                top_k,
                "semantic index remained busy",
            );
        }
    };
    if !semantic_loaded {
        let reloading = super::configure::trigger_semantic_index_reload_if_evicted(ctx);
        let detail = if reloading {
            "Semantic index is reloading; retry shortly."
        } else {
            "Semantic index is not ready yet."
        };
        return semantic_unavailable_or_fallback_response(
            req,
            ctx,
            &params,
            mode,
            &shape,
            "unavailable",
            "not_ready",
            detail.to_string(),
            "not_ready",
            false,
            lexical,
            warnings,
            project_root,
            top_k,
        );
    }

    let query_vector = match embed_query(&params.query, ctx) {
        Ok(query_vector) => query_vector,
        Err(error) => {
            if search_cancellation_requested() {
                return cancelled_search_response(req);
            }
            let lexical = if mode == SearchMode::Semantic {
                collect_lexical_files(
                    ctx,
                    &params.query,
                    &shape,
                    params.include_tests,
                    project_root,
                )
            } else {
                lexical
            };

            let classified = classify_embed_query_error(&error);
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "unavailable",
                "unavailable",
                classified.detail,
                classified.footer_reason,
                true,
                lexical,
                warnings,
                project_root,
                top_k,
            );
        }
    };
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }

    let semantic_limit = if params.include_tests {
        semantic_candidate_limit(top_k)
    } else {
        MAX_TOP_K
    };
    let semantic_fetch_limit = semantic_limit.saturating_add(1);
    let mut semantic_results =
        match try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET) {
            Some(semantic_index) => semantic_index
                .as_ref()
                .map(|index| {
                    index.search_filtered(&query_vector, semantic_fetch_limit, |file| {
                        path_allowed_by_include_tests(file, project_root, params.include_tests)
                    })
                })
                .unwrap_or_default(),
            None => {
                let lexical = if mode == SearchMode::Semantic {
                    collect_lexical_files(
                        ctx,
                        &params.query,
                        &shape,
                        params.include_tests,
                        project_root,
                    )
                } else {
                    lexical
                };
                return semantic_unavailable_or_fallback_response(
                    req,
                    ctx,
                    &params,
                    mode,
                    &shape,
                    "unavailable",
                    "unavailable",
                    format!(
                        "Semantic search artifacts remained busy beyond {}ms.",
                        INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
                    ),
                    "artifact contention",
                    true,
                    lexical,
                    warnings,
                    project_root,
                    top_k,
                );
            }
        };
    if ctx.shared_artifacts_read_only() {
        semantic_results.retain(|result| result.file.is_file());
    }
    let semantic_more_available = semantic_results.len() > semantic_limit;
    if semantic_more_available {
        semantic_results.truncate(semantic_limit);
    }
    rerank_semantic_candidates(&mut semantic_results, &shape, &params.query);

    let mut results = fuse_hybrid_results(
        semantic_results,
        lexical.files,
        &shape,
        top_k.saturating_add(1),
        params.include_tests,
        project_root,
    );
    if ctx.shared_artifacts_read_only() {
        results.retain(|result| result.file.is_file());
    }
    let fused_more_available = results.len() > top_k;
    if fused_more_available {
        results.truncate(top_k);
    }
    let more_available = fused_more_available || semantic_more_available || lexical.engine_capped;

    if mode == SearchMode::Semantic
        && shape.kind == QueryKind::NaturalLanguage
        && results.is_empty()
        && lexical.ready
    {
        let escalation_lexical = collect_lexical_files(
            ctx,
            &params.query,
            &shape,
            params.include_tests,
            project_root,
        );
        return zero_result_escalation_response(
            req,
            &params.query,
            &shape,
            mode,
            semantic_status,
            warnings,
            escalation_lexical,
            top_k,
            params.include_tests,
            project_root,
            project_root,
            Some(ctx),
            serde_json::Map::new(),
        );
    }

    // No score threshold: silent filtering produced "0 results" even when the
    // model had reasonable matches the agent could have judged. Surface every
    // hit so the caller can decide.

    // Read display snippets from source on the fly (top 3 only, rank-budgeted)
    // so both the text rendering and the JSON `results` carry fresh, correctly
    // sized previews. Drives the conditional zoom hint.
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut results, project_root, Some(ctx));

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text: format_semantic_text(
                &results,
                project_root,
                more_available,
                snippets_incomplete,
                Some(ctx),
            ),
            results: results.iter().map(result_to_json).collect::<Vec<_>>(),
            more_available,
            engine_capped: lexical.engine_capped,
            fully_degraded: false,
            warnings,
            extras: serde_json::Map::new(),
        },
    )
}

struct SearchResponseParts<'a> {
    query: &'a str,
    interpreted_as: &'static str,
    query_kind: &'static str,
    semantic_status: &'static str,
    status: &'static str,
    complete: bool,
    text: String,
    results: Vec<serde_json::Value>,
    more_available: bool,
    engine_capped: bool,
    fully_degraded: bool,
    warnings: Vec<String>,
    extras: serde_json::Map<String, serde_json::Value>,
}

impl<'a> SearchResponseParts<'a> {
    fn result_count(&self) -> usize {
        self.results.len()
    }
}

fn zero_result_escalation_disclosure(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Regex => "[interpreted_as: regex; no exact match — ranked by terms instead]",
        SearchMode::Literal => {
            "[interpreted_as: literal; no exact match — ranked by terms instead]"
        }
        SearchMode::Semantic => {
            "[interpreted_as: semantic; no result above cutoff — ranked by terms instead]"
        }
        SearchMode::Hybrid => {
            "[interpreted_as: hybrid; no result — no further escalation available]"
        }
    }
}

/// Render the single lexical second chance used after an auto-routed lane
/// returns no results. Keeping this pass separate makes the one-extra-pass
/// limit explicit and ensures zero remains an honest result when terms miss too.
fn zero_result_escalation_response(
    req: &RawRequest,
    query: &str,
    shape: &QueryShape,
    mode: SearchMode,
    semantic_status: &'static str,
    warnings: Vec<String>,
    lexical: LexicalCollection,
    top_k: usize,
    include_tests: bool,
    project_root: &Path,
    display_root: &Path,
    ctx: Option<&AppContext>,
    mut extras: serde_json::Map<String, serde_json::Value>,
) -> Response {
    let lexical_count = lexical.files.len();
    let lexical_engine_capped = lexical.engine_capped;
    let mut results = fuse_hybrid_results(
        Vec::new(),
        lexical.files,
        shape,
        top_k,
        include_tests,
        project_root,
    );
    if let Some(ctx) = ctx {
        if ctx.shared_artifacts_read_only() {
            results.retain(|result| result.file.is_file());
        }
    }
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut results, project_root, ctx);
    let mut text = format_semantic_text_with_display_root(
        &results,
        display_root,
        lexical_count > top_k || lexical_engine_capped,
        snippets_incomplete,
        ctx,
    );
    text.push('\n');
    text.push_str(zero_result_escalation_disclosure(mode));

    extras.insert(
        "zero_result_escalation".to_string(),
        serde_json::json!(true),
    );
    extras.insert("escalation_target".to_string(), serde_json::json!("hybrid"));
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text,
            results: results.iter().map(result_to_json).collect::<Vec<_>>(),
            more_available: lexical_count > top_k || lexical_engine_capped,
            engine_capped: lexical_engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn search_response(req: &RawRequest, parts: SearchResponseParts<'_>) -> Response {
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let mut object = serde_json::Map::new();
    object.insert("status".to_string(), serde_json::json!(parts.status));
    object.insert("complete".to_string(), serde_json::json!(parts.complete));
    object.insert("text".to_string(), serde_json::json!(parts.text));
    object.insert("query".to_string(), serde_json::json!(parts.query));
    object.insert(
        "interpreted_as".to_string(),
        serde_json::json!(parts.interpreted_as),
    );
    object.insert(
        "query_kind".to_string(),
        serde_json::json!(parts.query_kind),
    );
    object.insert(
        "result_count".to_string(),
        serde_json::json!(parts.result_count()),
    );
    object.insert(
        "results".to_string(),
        serde_json::Value::Array(parts.results),
    );
    object.insert(
        "more_available".to_string(),
        serde_json::json!(parts.more_available),
    );
    object.insert(
        "engine_capped".to_string(),
        serde_json::json!(parts.engine_capped),
    );
    object.insert(
        "fully_degraded".to_string(),
        serde_json::json!(parts.fully_degraded),
    );
    object.insert(
        "semantic_status".to_string(),
        serde_json::json!(parts.semantic_status),
    );
    if !parts.warnings.is_empty() {
        object.insert("warnings".to_string(), serde_json::json!(parts.warnings));
    }
    for (key, value) in parts.extras {
        object.insert(key, value);
    }
    Response::success(&req.id, serde_json::Value::Object(object))
}

fn artifact_contention_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    project_root: &Path,
    top_k: usize,
    artifact: &str,
) -> Response {
    semantic_unavailable_grep_fallback_response(
        req,
        ctx,
        params,
        shape,
        "unavailable",
        format!(
            "Search artifacts were busy beyond the {}ms interactive budget ({artifact}).",
            INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
        ),
        "artifact contention",
        false,
        Vec::new(),
        project_root,
        top_k,
    )
}

fn semantic_unavailable_or_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    mode: SearchMode,
    shape: &QueryShape,
    semantic_status: &'static str,
    unavailable_status: &'static str,
    detail: String,
    footer_reason: &str,
    force_lexical_fallback: bool,
    lexical: LexicalCollection,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
) -> Response {
    if lexical.artifact_lock_timed_out {
        return artifact_contention_fallback_response(
            req,
            ctx,
            params,
            shape,
            project_root,
            top_k,
            "search index remained busy",
        );
    }
    let lexical_ready = (mode == SearchMode::Hybrid || force_lexical_fallback) && lexical.ready;
    if lexical_ready {
        let lexical_count = lexical.files.len();
        let lexical_engine_capped = lexical.engine_capped;
        let results = fuse_hybrid_results(
            Vec::new(),
            lexical.files,
            shape,
            top_k,
            params.include_tests,
            project_root,
        );
        let result_values = results.iter().map(result_to_json).collect::<Vec<_>>();
        warnings.push(
            "Semantic search unavailable; returning lexical-only fallback results.".to_string(),
        );

        return search_response(
            req,
            SearchResponseParts {
                query: &params.query,
                // The trigram lexical lane produced these results; semantic
                // never ran. Report what executed, not the routed mode.
                interpreted_as: fallback_executed_label(mode, true),
                query_kind: query_kind_label(shape.kind),
                semantic_status,
                status: "ready",
                complete: false,
                text: format_lexical_unavailable_text(
                    &detail,
                    &results,
                    project_root,
                    footer_reason,
                ),
                results: result_values,
                more_available: lexical_count > top_k || lexical_engine_capped,
                engine_capped: lexical_engine_capped,
                fully_degraded: false,
                warnings,
                extras: semantic_unavailable_extras(true),
            },
        );
    }

    if force_lexical_fallback || semantic_degraded_fallback_available(mode, shape, &lexical) {
        return semantic_unavailable_grep_fallback_response(
            req,
            ctx,
            params,
            shape,
            semantic_status,
            detail,
            footer_reason,
            false,
            warnings,
            project_root,
            top_k,
        );
    }

    let mut extras = semantic_unavailable_extras(false);
    if mode == SearchMode::Hybrid {
        extras.insert("lexical_unavailable".to_string(), serde_json::json!(true));
    }

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: unavailable_status,
            complete: false,
            text: detail,
            results: Vec::new(),
            more_available: false,
            engine_capped: lexical.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn semantic_unavailable_extras(
    lexical_only_fallback: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let mut extras = serde_json::Map::new();
    extras.insert("semantic_unavailable".to_string(), serde_json::json!(true));
    extras.insert(
        "lexical_only_fallback".to_string(),
        serde_json::json!(lexical_only_fallback),
    );
    extras
}

fn semantic_degraded_fallback_available(
    mode: SearchMode,
    shape: &QueryShape,
    lexical: &LexicalCollection,
) -> bool {
    if natural_language_degraded_fallback_available(mode, shape) {
        return true;
    }

    mode == SearchMode::Semantic && !lexical.ready && shape.weights.should_use_lexical
}

fn natural_language_degraded_fallback_available(mode: SearchMode, shape: &QueryShape) -> bool {
    mode == SearchMode::Semantic && shape.kind == QueryKind::NaturalLanguage
}

fn semantic_unavailable_grep_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    semantic_status: &'static str,
    detail: String,
    footer_reason: &str,
    borrowed_loading: bool,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
) -> Response {
    let fallback = match execute_degraded_grep_fallback(
        &params.query,
        project_root,
        top_k,
        params.include_tests,
        &req.id,
    ) {
        Ok(result) => result,
        Err(response) => return response,
    };
    let result = &fallback.grep;
    let detail = if borrowed_loading && !result.matches.is_empty() {
        BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS.to_string()
    } else {
        detail
    };
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }
    if fallback.file_cap_reached {
        warnings.push(format!(
            "Degraded grep reached its {}-file scan cap; additional files were not scanned.",
            fallback.file_limit
        ));
    }
    if fallback.walk_budget_reached {
        warnings.push(
            "Degraded grep reached its 10-second walk budget; additional files were not scanned."
                .to_string(),
        );
    }
    warnings
        .push("Semantic search unavailable; returning lexical-only fallback results.".to_string());

    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, "literal"))
        .collect::<Vec<_>>();
    let more_available = result.truncated
        || result.total_matches > result.matches.len()
        || fallback.file_cap_reached
        || fallback.walk_budget_reached
        || result.skipped_foreign_mounts > 0;
    let mut extras = semantic_unavailable_extras(true);
    if fallback.file_cap_reached || fallback.walk_budget_reached {
        extras.insert(
            "degraded_grep_walk_truncated".to_string(),
            serde_json::json!(true),
        );
    }
    if result.skipped_foreign_mounts > 0 {
        extras.insert(
            "degraded_grep_skipped_foreign_mounts".to_string(),
            serde_json::json!(result.skipped_foreign_mounts),
        );
    }
    if fallback.file_cap_reached {
        extras.insert(
            "degraded_grep_file_limit".to_string(),
            serde_json::json!(fallback.file_limit),
        );
        extras.insert(
            "degraded_grep_candidate_files".to_string(),
            serde_json::json!(fallback.candidate_files),
        );
    }

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            // This path ran a literal grep scan over the corpus (the results are
            // GrepLine entries), so report "literal" — not the routed
            // semantic/hybrid mode that never executed.
            interpreted_as: "literal",
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: false,
            text: format_grep_lexical_unavailable_text(
                &detail,
                result,
                project_root,
                footer_reason,
            ),
            results: result_values,
            more_available,
            engine_capped: result.engine_capped,
            fully_degraded: result.fully_degraded,
            warnings,
            extras,
        },
    )
}

fn execute_degraded_grep_fallback(
    query: &str,
    project_root: &Path,
    top_k: usize,
    include_tests: bool,
    request_id: &str,
) -> Result<DegradedGrepFallbackResult, Response> {
    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: true,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            return Err(Response::error_with_data(
                request_id,
                "invalid_pattern",
                message,
                serde_json::json!({"pattern": query}),
            ));
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Err(Response::error_with_data(
                request_id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            ));
        }
    };

    let max_results = top_k.clamp(1, DEGRADED_GREP_RESULT_LIMIT);
    let started = Instant::now();
    let (files, file_cap_reached, walk_budget_reached, skipped_foreign_mounts) =
        collect_degraded_grep_files(project_root, include_tests, started);
    if search_cancellation_requested() {
        return Err(cancelled_search_response_from_id(request_id));
    }
    let candidate_files = files.len();
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_searched = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated = false;
    let mut engine_capped = file_cap_reached || walk_budget_reached;

    let read_budget_reached = AtomicBool::new(walk_budget_reached);
    let cancellation = crate::executor::current_job_cancellation();
    let mut readable_files = files
        .par_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            if cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
            {
                return None;
            }
            if started.elapsed() >= DEGRADED_GREP_WALK_BUDGET {
                read_budget_reached.store(true, Ordering::Relaxed);
                return None;
            }
            crate::search_index::read_searchable_text(file)
                .map(|content| (index, file.clone(), content))
        })
        .collect::<Vec<_>>();
    if search_cancellation_requested() {
        return Err(cancelled_search_response_from_id(request_id));
    }
    // Rayon collection order is not part of the response contract; restore the
    // original walker order before applying the existing result-cap semantics.
    readable_files.sort_by_key(|(index, _, _)| *index);

    for (_, file, content) in readable_files {
        if search_cancellation_requested() {
            return Err(cancelled_search_response_from_id(request_id));
        }
        if truncated {
            engine_capped = true;
            break;
        }

        files_searched += 1;

        if search_degraded_grep_file(
            &file,
            &content,
            &compiled,
            max_results,
            &mut total_matches,
            &mut truncated,
            &mut matches,
        ) {
            files_with_matches += 1;
        }
    }

    if truncated {
        engine_capped = true;
    }
    sort_grep_matches_by_mtime_desc(&mut matches, project_root);

    let walk_budget_reached = read_budget_reached.load(Ordering::Relaxed);
    Ok(DegradedGrepFallbackResult {
        grep: GrepResult {
            matches,
            total_matches,
            files_searched,
            files_with_matches,
            index_status: IndexStatus::Fallback,
            truncated,
            fully_degraded: true,
            engine_capped,
            walk_truncated: walk_budget_reached,
            skipped_foreign_mounts,
        },
        file_cap_reached,
        file_limit: DEGRADED_GREP_FILE_LIMIT,
        candidate_files,
        walk_budget_reached,
    })
}

fn collect_degraded_grep_files(
    project_root: &Path,
    include_tests: bool,
    started: Instant,
) -> (Vec<PathBuf>, bool, bool, usize) {
    // Keep degraded semantic search on the root filesystem: ReadDir::drop can
    // abort the daemon if a disappearing child mount reports ENXIO.
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(project_root).ok();
    let walker = ignore::WalkBuilder::new(project_root)
        .same_file_system(true)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry({
            let skipped_foreign_mounts = Arc::clone(&skipped_foreign_mounts);
            move |entry| {
                if entry.depth() > 0
                    && entry
                        .file_type()
                        .is_some_and(|file_type| file_type.is_dir())
                    && matches!(
                        boundary
                            .as_ref()
                            .map(|boundary| boundary.should_descend(entry.path())),
                        Some(Ok(false))
                    )
                {
                    skipped_foreign_mounts.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                let name = entry.file_name().to_string_lossy();
                if entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_dir())
                {
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
                true
            }
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        if search_cancellation_requested() {
            return (
                files,
                false,
                true,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        if started.elapsed() >= DEGRADED_GREP_WALK_BUDGET {
            return (
                files,
                false,
                true,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if !include_tests && path_is_hidden_test_file(&path, project_root) {
            continue;
        }
        if files.len() >= DEGRADED_GREP_FILE_LIMIT {
            return (
                files,
                true,
                false,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        files.push(path);
    }

    (
        files,
        false,
        false,
        skipped_foreign_mounts.load(Ordering::Relaxed),
    )
}

fn search_degraded_grep_file(
    file: &Path,
    content: &str,
    compiled: &pattern_compile::CompiledPattern,
    max_results: usize,
    total_matches: &mut usize,
    truncated: &mut bool,
    matches: &mut Vec<GrepMatch>,
) -> bool {
    let line_starts = grep_executor::line_starts(content);
    let mut seen_lines = HashSet::new();
    let mut matched_this_file = false;

    match compiled {
        pattern_compile::CompiledPattern::Literal(literal) => {
            let Some(needle) = std::str::from_utf8(&literal.needle).ok() else {
                return false;
            };
            let haystack = if literal.case_insensitive_ascii {
                Cow::Owned(content.to_ascii_lowercase())
            } else {
                Cow::Borrowed(content)
            };

            for (offset, matched) in haystack.match_indices(needle) {
                if search_cancellation_requested() {
                    break;
                }
                let match_text = content[offset..offset + matched.len()].to_string();
                let (counted, should_continue) = record_degraded_grep_match(
                    file,
                    content,
                    &line_starts,
                    &mut seen_lines,
                    offset,
                    match_text,
                    max_results,
                    total_matches,
                    truncated,
                    matches,
                );
                matched_this_file |= counted;
                if !should_continue {
                    break;
                }
            }
        }
        pattern_compile::CompiledPattern::Regex { compiled, .. } => {
            for matched in compiled.find_iter(content.as_bytes()) {
                if search_cancellation_requested() {
                    break;
                }
                let (counted, should_continue) = record_degraded_grep_match(
                    file,
                    content,
                    &line_starts,
                    &mut seen_lines,
                    matched.start(),
                    String::from_utf8_lossy(matched.as_bytes()).into_owned(),
                    max_results,
                    total_matches,
                    truncated,
                    matches,
                );
                matched_this_file |= counted;
                if !should_continue {
                    break;
                }
            }
        }
    }

    matched_this_file
}

fn record_degraded_grep_match(
    file: &Path,
    content: &str,
    line_starts: &[usize],
    seen_lines: &mut HashSet<u32>,
    offset: usize,
    match_text: String,
    max_results: usize,
    total_matches: &mut usize,
    truncated: &mut bool,
    matches: &mut Vec<GrepMatch>,
) -> (bool, bool) {
    let (line, column, line_text) = grep_executor::line_details(content, line_starts, offset);
    if !seen_lines.insert(line) {
        return (false, true);
    }

    *total_matches += 1;
    if matches.len() >= max_results {
        *truncated = true;
        return (true, false);
    }

    matches.push(GrepMatch {
        file: file.to_path_buf(),
        line,
        column,
        line_text,
        match_text,
    });
    (true, true)
}

fn semantic_index_loaded_with_budget(ctx: &AppContext) -> Result<bool, ()> {
    let semantic_index =
        try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET).ok_or(())?;
    Ok(semantic_index.is_some())
}

fn collect_lexical_files(
    ctx: &AppContext,
    query: &str,
    shape: &QueryShape,
    include_tests: bool,
    project_root: &Path,
) -> LexicalCollection {
    let Some(search_index) =
        try_read_with_budget(ctx.search_index(), INTERACTIVE_ARTIFACT_READ_BUDGET)
    else {
        return LexicalCollection {
            files: Vec::new(),
            ready: false,
            engine_capped: false,
            artifact_lock_timed_out: true,
        };
    };
    let snapshot = search_index
        .as_ref()
        .filter(|index| index.ready)
        .map(SearchIndex::snapshot);
    collect_lexical_files_from_snapshot(snapshot, query, shape, include_tests, project_root)
}

fn collect_lexical_files_from_snapshot(
    snapshot: Option<SearchIndexSnapshot>,
    query: &str,
    shape: &QueryShape,
    include_tests: bool,
    project_root: &Path,
) -> LexicalCollection {
    // No `should_use_lexical` gate here: collect_lexical_files is only called
    // when choose_mode picked Hybrid, which already means we want the lexical
    // lane. The shape weight was a second, conflicting gate that suppressed
    // lexical for short NL concepts routed to Hybrid.
    //
    // Use the shared lexical token extraction, including quoted code tokens,
    // for both the normal hybrid lane and its one-pass zero-result escalation.
    let tokens = query_shape::extract_lexical_tokens(query, shape);
    let token_refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);

    // No extension filter: the trigram index already covers the project's text
    // files. Gating the lexical candidate set on the *semantic* extension
    // allow-list made named config/doc files (Cargo.toml, README.md,
    // package.json) structurally unreachable in hybrid mode — exactly the
    // literal-filename hits the lexical lane exists to catch.
    let Some(snapshot) = snapshot else {
        return LexicalCollection {
            files: Vec::new(),
            ready: false,
            engine_capped: false,
            artifact_lock_timed_out: false,
        };
    };
    let production_file_filter =
        |path: &Path| path_allowed_by_include_tests(path, project_root, include_tests);
    let filter = (!include_tests).then_some(&production_file_filter as &dyn Fn(&Path) -> bool);
    let ranked =
        snapshot.lexical_rank_with_stats(&query_trigrams, filter, LEXICAL_ENUMERATION_LIMIT);

    LexicalCollection {
        files: ranked.files,
        ready: true,
        engine_capped: ranked.engine_capped,
        artifact_lock_timed_out: false,
    }
}

fn search_index_ready_with_budget(
    ctx: &AppContext,
    wait_budget: Duration,
) -> Result<bool, SearchIndexWaitError> {
    let deadline = Instant::now() + wait_budget;
    loop {
        if search_cancellation_requested() {
            return Err(SearchIndexWaitError::Cancelled);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let read_budget = remaining.min(SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL);
        let Some(search_index) = try_read_with_budget(ctx.search_index(), read_budget) else {
            if Instant::now() >= deadline {
                return Err(SearchIndexWaitError::Contended);
            }
            continue;
        };
        if search_index.as_ref().is_some_and(|index| index.ready) {
            return Ok(true);
        }
        drop(search_index);

        // The loader publishes through a channel; the query holds neither the
        // index nor receiver lock while draining, so it cannot block publication.
        let Some(search_receiver) = try_read_with_budget(ctx.search_index_rx(), read_budget) else {
            if Instant::now() >= deadline {
                return Err(SearchIndexWaitError::Contended);
            }
            continue;
        };
        let load_in_progress = search_receiver.is_some();
        drop(search_receiver);
        if !load_in_progress {
            return Ok(false);
        }

        crate::runtime_drain::drain_search_index_events(ctx);
        if search_cancellation_requested() {
            return Err(SearchIndexWaitError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL));
    }
}

fn search_index_ready(ctx: &AppContext) -> bool {
    search_index_ready_with_budget(ctx, INTERACTIVE_ARTIFACT_READ_BUDGET).unwrap_or(false)
}

fn embed_query(query: &str, ctx: &AppContext) -> Result<Vec<f32>, String> {
    let index_dimension = {
        let semantic_index =
            try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET)
                .ok_or_else(|| {
                    format!(
                        "semantic index remained busy beyond {}ms",
                        INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
                    )
                })?;
        semantic_index
            .as_ref()
            .filter(|index| index.len() > 0)
            .map(|index| index.dimension())
    };
    embed_query_for_dimension(query, ctx, index_dimension)
}

fn embed_query_for_dimension(
    query: &str,
    ctx: &AppContext,
    index_dimension: Option<usize>,
) -> Result<Vec<f32>, String> {
    let semantic_config = ctx.config().semantic.clone();
    let query_budget = QueryBudget::from_config(&semantic_config);
    let mut model_ref = ctx.semantic_embedding_model().lock();

    if model_ref.is_none() {
        drop(model_ref);

        let constructed_model = EmbeddingModel::from_config_for_query(&semantic_config)?;

        model_ref = ctx.semantic_embedding_model().lock();
        if model_ref.is_none() {
            *model_ref = Some(constructed_model);
        } else {
            drop(model_ref);
            {
                let _discarded_model = constructed_model;
            }
            model_ref = ctx.semantic_embedding_model().lock();
        }
    }

    let model = model_ref
        .as_mut()
        .ok_or_else(|| "embedding model was not initialized".to_string())?;
    // Preserve the raw error so the timeout marker injected by
    // `send_embedding_request` survives — `classify_embed_query_error` reads it
    // to decide whether the configured query budget fired.
    let query_vector = model
        .embed_query_cached(query, query_budget)
        .map_err(|error| format!("failed to embed query: {error}"))?;
    drop(model_ref);

    if let Some(index_dimension) = index_dimension {
        if index_dimension != query_vector.len() {
            return Err(format!(
                "semantic embedding dimension mismatch: query backend returned {}, index expects {}. Rebuild the semantic index for the active backend/model.",
                query_vector.len(),
                index_dimension
            ));
        }
    }

    Ok(query_vector)
}

/// Classified query-embedding failure: the user-facing `detail` line that names
/// the mechanism and the remedy when the configured query budget fired, and a
/// short `footer_reason` token for the `[semantic: ...]` status footer the agent
/// sees. Non-timeout errors keep the current message shape and an `unavailable`
/// footer.
///
/// The timeout case is detected via the stable marker
/// [`crate::semantic_index::query_embedding_timeout_budget`] injects at the one
/// site that knows both the typed reqwest error and the Query budget — never by
/// substring-matching reqwest's rendered text, which varies by backend/locale.
struct ClassifiedEmbedQueryError {
    detail: String,
    footer_reason: &'static str,
}

fn classify_embed_query_error(error: &str) -> ClassifiedEmbedQueryError {
    if let Some(timeout_ms) = query_embedding_timeout_budget(error) {
        let clean = strip_query_embedding_timeout_marker(error);
        // Name the mechanism (the budget fired) and the remedy (raise the knob
        // for slow providers) so the agent can act, not just observe.
        let detail = format!(
            "Semantic search unavailable: query embedding timed out after {timeout_ms}ms (semantic.query_timeout_ms; raise it for slow providers). {clean}"
        );
        let footer_reason =
            Box::leak(format!("query embed timeout ({timeout_ms}ms)").into_boxed_str());
        ClassifiedEmbedQueryError {
            detail,
            footer_reason,
        }
    } else {
        ClassifiedEmbedQueryError {
            detail: format!("Semantic search unavailable: {error}"),
            footer_reason: "unavailable",
        }
    }
}

fn rerank_semantic_candidates(results: &mut Vec<SemanticResult>, shape: &QueryShape, query: &str) {
    let (tokens, allow_case_fold) = semantic_rerank_tokens(query, shape);
    let type_concept = query_shape::is_type_concept_identifier_query(query, shape);
    let apply_definition_priors = shape.kind == QueryKind::NaturalLanguage || type_concept;
    let kind_prior_strength = semantic_kind_prior_strength(shape, apply_definition_priors);

    for result in results.iter_mut() {
        result.rank_score = result.score;
        result.cap_protected = false;
        result.rank_score *= semantic_kind_multiplier(&result.kind, kind_prior_strength);

        if !tokens.is_empty()
            && is_definition_kind(&result.kind)
            && tokens
                .iter()
                .any(|token| token_matches_candidate_name(token, result, allow_case_fold))
        {
            result.rank_score *= EXACT_NAME_DEFINITION_BOOST;
            if result.score >= P2_CAP_PROTECTED_COSINE_FLOOR {
                result.cap_protected = true;
            }
        }
    }

    if apply_definition_priors {
        apply_natural_language_diversity_cap(results);
    }
}

#[derive(Clone, Copy)]
enum SemanticKindPriorStrength {
    NaturalLanguage,
    Mixed,
    Inert,
}

fn semantic_kind_prior_strength(
    shape: &QueryShape,
    apply_definition_priors: bool,
) -> SemanticKindPriorStrength {
    if apply_definition_priors {
        SemanticKindPriorStrength::NaturalLanguage
    } else if shape.kind == QueryKind::Mixed {
        SemanticKindPriorStrength::Mixed
    } else {
        SemanticKindPriorStrength::Inert
    }
}

fn semantic_rerank_tokens(query: &str, shape: &QueryShape) -> (Vec<String>, bool) {
    match shape.kind {
        QueryKind::Identifier => (query_shape::extract_tokens(query, shape), false),
        QueryKind::Mixed => (query_shape::extract_tokens(query, shape), true),
        QueryKind::NaturalLanguage => (query_shape::extract_explicit_code_tokens(query), true),
        QueryKind::Path | QueryKind::ErrorCode | QueryKind::Regex => (Vec::new(), false),
    }
}

fn semantic_kind_multiplier(kind: &SymbolKind, strength: SemanticKindPriorStrength) -> f32 {
    match strength {
        SemanticKindPriorStrength::NaturalLanguage => match kind {
            SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias => 1.08,
            SymbolKind::Variable => 0.92,
            SymbolKind::FileSummary => 0.80,
            SymbolKind::Heading => 1.0,
        },
        SemanticKindPriorStrength::Mixed => match kind {
            SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias => 1.03,
            SymbolKind::FileSummary => 0.90,
            SymbolKind::Variable | SymbolKind::Heading => 1.0,
        },
        SemanticKindPriorStrength::Inert => 1.0,
    }
}

fn is_definition_kind(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
    )
}

fn token_matches_candidate_name(
    token: &str,
    result: &SemanticResult,
    allow_case_fold: bool,
) -> bool {
    names_equal(token, &result.name, allow_case_fold)
        || result
            .qualified_name
            .as_deref()
            .is_some_and(|qualified_name| names_equal(token, qualified_name, allow_case_fold))
}

fn names_equal(token: &str, name: &str, allow_case_fold: bool) -> bool {
    token == name || (allow_case_fold && token.eq_ignore_ascii_case(name))
}

fn apply_natural_language_diversity_cap(results: &mut Vec<SemanticResult>) {
    let mut cluster_counts: HashMap<(String, SymbolKind), usize> = HashMap::new();
    results.retain(|result| {
        let key = (
            result
                .qualified_name
                .as_deref()
                .unwrap_or(&result.name)
                .to_string(),
            result.kind.clone(),
        );
        let count = cluster_counts.entry(key).or_insert(0);
        if *count < NATURAL_LANGUAGE_CLUSTER_CAP {
            *count += 1;
            true
        } else {
            false
        }
    });
}

fn sort_hybrid_results(results: &mut [HybridResult]) {
    results.sort_by(|a, b| {
        a.lexical_generated_artifact
            .cmp(&b.lexical_generated_artifact)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn fuse_hybrid_results(
    semantic: Vec<SemanticResult>,
    lexical_files: Vec<(PathBuf, f32)>,
    shape: &QueryShape,
    top_k: usize,
    include_tests: bool,
    project_root: &Path,
) -> Vec<HybridResult> {
    if top_k == 0 {
        return Vec::new();
    }

    let semantic = semantic
        .into_iter()
        .filter(|result| path_allowed_by_include_tests(&result.file, project_root, include_tests))
        .collect::<Vec<_>>();
    let lexical_files = lexical_files
        .into_iter()
        .filter(|(file, _)| path_allowed_by_include_tests(file, project_root, include_tests))
        .collect::<Vec<_>>();
    let lexical_candidates =
        lexical_candidates_with_generated_artifact_rank(lexical_files, project_root);

    if lexical_candidates.is_empty() {
        let mut results = semantic
            .into_iter()
            .map(|result| hybrid_from_semantic(result, None))
            .collect::<Vec<_>>();
        sort_hybrid_results(&mut results);
        results.truncate(top_k);
        return results;
    }

    if semantic.is_empty() {
        return lexical_candidates
            .into_iter()
            .take(top_k)
            .map(|candidate| lexical_only_result(candidate, shape))
            .collect();
    }

    // Use every collected lexical candidate, not a hidden sub-cap. The lexical
    // lane already bounds enumeration at LEXICAL_ENUMERATION_LIMIT upstream and
    // returns candidates pre-ranked by score; an additional `.take(20)` here
    // silently dropped candidates 21..=50 from both the semantic-boost map and
    // the standalone-lexical results without that loss being reflected in
    // `more_available`/`engine_capped`. The final output is already bounded by
    // cap_per_file + truncate(top_k), so honoring all collected candidates is
    // both more correct and honest about what was considered. Generated documentation
    // artifacts stay in that candidate set but are marked so their lexical lane
    // contribution cannot outrank non-generated files.
    let lexical_top_files: HashMap<PathBuf, LexicalCandidate> = lexical_candidates
        .iter()
        .map(|candidate| (candidate.file.clone(), candidate.clone()))
        .collect();
    let mut results: Vec<HybridResult> = semantic
        .into_iter()
        .map(|result| {
            let lexical_candidate = lexical_top_files.get(&result.file);
            hybrid_from_semantic(result, lexical_candidate)
        })
        .collect();

    let semantic_files: HashSet<PathBuf> =
        results.iter().map(|result| result.file.clone()).collect();
    for candidate in lexical_candidates {
        if !semantic_files.contains(&candidate.file) {
            results.push(lexical_only_result(candidate, shape));
        }
    }

    sort_hybrid_results(&mut results);
    let mut results = cap_per_file(results, 2);
    sort_hybrid_results(&mut results);
    results.truncate(top_k);
    results
}

fn hybrid_from_semantic(
    result: SemanticResult,
    lexical_candidate: Option<&LexicalCandidate>,
) -> HybridResult {
    let semantic_score = result.score;
    let ranking_score = result.rank_score;
    let cap_protected = result.cap_protected;
    let lexical_score = lexical_candidate.map(|candidate| candidate.score);
    let hybrid_boosted = lexical_candidate.is_some_and(|candidate| !candidate.generated_artifact);
    let score = if hybrid_boosted {
        ranking_score * HYBRID_LEXICAL_BOOST
    } else {
        ranking_score
    };

    HybridResult {
        file: result.file,
        name: result.name,
        kind: result.kind,
        start_line: result.start_line,
        end_line: result.end_line,
        exported: result.exported,
        snippet: result.snippet,
        score,
        source: "semantic",
        semantic_score: Some(semantic_score),
        lexical_score,
        hybrid_boosted,
        cap_protected,
        lexical_generated_artifact: false,
    }
}

fn lexical_only_result(candidate: LexicalCandidate, shape: &QueryShape) -> HybridResult {
    let score = if candidate.generated_artifact {
        0.0
    } else {
        // Lexical scores are not cosine-normalized and can exceed the semantic
        // lane's score scale. Keep lexical-only files visible without letting
        // broad trigram overlaps evict strong semantic matches.
        (candidate.score * shape_dependent_lexical_only_weight(shape))
            .min(LEXICAL_ONLY_SCORE_CEILING)
    };

    HybridResult {
        file: candidate.file,
        name: String::new(),
        kind: SymbolKind::FileSummary,
        start_line: 0,
        end_line: 0,
        exported: false,
        score,
        source: "lexical",
        semantic_score: None,
        lexical_score: Some(candidate.score),
        hybrid_boosted: false,
        cap_protected: false,
        lexical_generated_artifact: candidate.generated_artifact,
        snippet: "[lexical match — use aft_zoom or read for context]".to_string(),
    }
}

fn shape_dependent_lexical_only_weight(shape: &QueryShape) -> f32 {
    match shape.kind {
        QueryKind::Identifier => 0.8,
        QueryKind::Path | QueryKind::ErrorCode | QueryKind::Mixed => 0.5,
        QueryKind::NaturalLanguage | QueryKind::Regex => 0.0,
    }
}

fn cap_per_file(results: Vec<HybridResult>, cap: usize) -> Vec<HybridResult> {
    let mut ordinary_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut capped = Vec::new();
    for result in results {
        if result.cap_protected {
            capped.push(result);
            continue;
        }

        let count = ordinary_counts.entry(result.file.clone()).or_insert(0);
        if *count < cap {
            *count += 1;
            capped.push(result);
        }
    }
    capped
}

fn format_lexical_unavailable_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
    footer_reason: &str,
) -> String {
    if results.is_empty() {
        return format!(
            "{detail}\n0 lexical matches; the semantic lane is unavailable ({footer_reason}), so prose-style queries may match only via semantic. Retry in a few seconds. [semantic: {footer_reason}]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: {footer_reason}]",
        format_result_sections(results, project_root),
        results.len()
    )
}

fn format_grep_lexical_unavailable_text(
    detail: &str,
    result: &GrepResult,
    project_root: &Path,
    footer_reason: &str,
) -> String {
    if result.matches.is_empty() {
        return format!(
            "{detail}\n0 lexical matches; the semantic lane is unavailable ({footer_reason}), so prose-style queries may match only via semantic. Retry in a few seconds. [semantic: {footer_reason}]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: {footer_reason}]",
        crate::commands::grep::format_grep_text(result, project_root),
        result.matches.len()
    )
}

fn building_lexical_note(borrowed_loading_with_results: bool) -> &'static str {
    if borrowed_loading_with_results {
        BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS
    } else {
        "Semantic index is rebuilding; results are lexical-only fallback results from the trigram index."
    }
}

fn format_building_lexical_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
    lexical_index_ready: bool,
    loading_artifacts: bool,
    borrowed_loading: bool,
) -> String {
    if results.is_empty() && !lexical_index_ready {
        if loading_artifacts {
            return format!(
                "{detail}\nIndex artifacts are loading for this root; nothing was searched yet because the trigram index is not ready. Retry in a few seconds. [semantic: loading]"
            );
        }
        return format!(
            "{detail}\nSemantic index is still building and the trigram index is not ready; nothing was searched yet. Retry in a few seconds. [semantic: building]"
        );
    }

    let note = building_lexical_note(borrowed_loading && !results.is_empty());
    if results.is_empty() {
        return format!(
            "{detail}\n{note}\nFound 0 lexical fallback result(s). [semantic: rebuilding]"
        );
    }

    if borrowed_loading {
        return format!(
            "{note}\n\n{}\n\nFound {} lexical fallback result(s). [semantic: loading]",
            format_result_sections(results, project_root),
            results.len()
        );
    }

    format!(
        "{detail}\n{note}\n\n{}\n\nFound {} lexical fallback result(s). [semantic: rebuilding]",
        format_result_sections(results, project_root),
        results.len()
    )
}

/// Top semantic cosine below this floor means the embedder found nothing
/// genuinely relevant — the query likely whiffed. We don't show the raw score
/// (uncalibrated for ranking), but its absolute floor is a real signal: an
/// all-weak result set looks identical to a strong one without it.
const WEAK_MATCH_COSINE_FLOOR: f32 = 0.35;
const P2_CAP_PROTECTED_COSINE_FLOOR: f32 = WEAK_MATCH_COSINE_FLOOR;
const EXACT_NAME_DEFINITION_BOOST: f32 = 1.20;
const NATURAL_LANGUAGE_CLUSTER_CAP: usize = 2;
// Intentionally high: the default MiniLM scores are uncalibrated, so under-trigger rather than over-promise.
const HIGH_CONFIDENCE_COSINE_FLOOR: f32 = 0.60;

/// True when the best result's raw semantic cosine is below the weak floor.
/// Uses `semantic_score` (the raw cosine), not the fused `score`. Lexical-only
/// top results have no cosine and are not flagged here (lexical relevance is
/// judged differently).
fn results_are_low_confidence(results: &[HybridResult]) -> bool {
    results
        .first()
        .and_then(|r| r.semantic_score)
        .is_some_and(|cosine| cosine < WEAK_MATCH_COSINE_FLOOR)
}

fn format_semantic_text(
    results: &[HybridResult],
    project_root: &Path,
    more_available: bool,
    snippets_incomplete: bool,
    ctx: Option<&AppContext>,
) -> String {
    format_semantic_text_with_display_root(
        results,
        project_root,
        more_available,
        snippets_incomplete,
        ctx,
    )
}

fn format_semantic_text_with_display_root(
    results: &[HybridResult],
    display_root: &Path,
    more_available: bool,
    snippets_incomplete: bool,
    ctx: Option<&AppContext>,
) -> String {
    if results.is_empty() {
        return "Found 0 results.".to_string();
    }

    let mut text = format_result_sections_with_context(results, display_root, ctx);
    // Drop the unconditional "[index: ready]" tag — it was pure per-call tax on
    // the common path. Degraded/building/unavailable paths carry their own
    // distinct "[semantic: ...]" labels, so absence of a label means ready.
    text.push_str(&format!("\n\nFound {} result(s).", results.len()));
    if more_available {
        text.push_str(" More results available; raise topK to see more.");
    }
    // Recover the "did the search whiff" signal we lost by hiding the score:
    // one coarse flag when the top match is weak, so the agent reformulates or
    // falls back to grep instead of trusting a uniformly-weak ranking.
    if results_are_low_confidence(results) {
        text.push_str("\nTop match is weak — consider rephrasing or using grep for exact terms.");
    }
    // Only when snippet content was actually withheld (omitted for rank 4+, or
    // truncated within the top 3) — so the hint appears exactly when it's
    // actionable, not on every search.
    if snippets_incomplete {
        text.push_str("\nZoom any result for full source: aft_zoom <file> <symbol>.");
    }
    text
}

fn format_grep_search_text(
    result: &GrepResult,
    project_root: &Path,
    interpreted_as: &str,
) -> String {
    let base = crate::commands::grep::format_grep_text(result, project_root);
    format!("{base}\n[interpreted_as: {interpreted_as}]")
}

/// Snippet line budget by global rank (0-based). The fused score is an
/// uncalibrated, scale-mixed artifact (raw cosine for semantic-only hits,
/// cosine×boost for lexically-co-matched hits), so it is NOT shown to the
/// agent — position conveys rank. We spend snippet tokens by rank instead: the
/// top hit is disproportionately likely to be the final answer (a fuller
/// preview there can save a follow-up aft_zoom), tail hits only need to be
/// identifiable. Snippets are limited to the top 3; rank 4+ shows the symbol
/// header only and the agent zooms the ones it cares about.
fn snippet_line_budget(global_rank: usize) -> usize {
    match global_rank {
        // Rank 0 gets a fuller preview: 10 lines was often half a real function,
        // forcing a zoom anyway and defeating the "preview saves a follow-up"
        // goal. 20 (capped at the symbol's real length) clears most functions.
        0 => 20,
        1 | 2 => 5,
        _ => 0,
    }
}

/// Replace each result's display snippet with source lines read on the fly from
/// disk, bounded by the rank budget. Snippets are display-only (they never
/// affect embeddings), so reading them at query time keeps the on-disk index
/// free of display text, lets snippet sizing change without a re-index, and
/// shows the current file content instead of whatever was captured at index
/// time. Only the top 3 carry snippets; rank 4+ get a header only and the agent
/// zooms the ones it cares about. Lexical rows keep their placeholder and file
/// summaries keep the generated summary (not source lines). Returns true when
/// any snippet was truncated or omitted, so the caller emits the zoom hint only
/// when it is actionable.
#[cfg(test)]
fn enrich_snippets_from_source(results: &mut [HybridResult], project_root: &Path) -> bool {
    enrich_snippets_from_source_with_context(results, project_root, None)
}

#[derive(Debug, Clone, Copy, Default)]
struct SnippetReadPlan {
    fixed_last_line: Option<usize>,
    summary_nonempty_lines: usize,
}

impl SnippetReadPlan {
    fn include_through(&mut self, line: u32) {
        let line = line as usize;
        self.fixed_last_line = Some(
            self.fixed_last_line
                .map_or(line, |current| current.max(line)),
        );
    }

    fn include_summary_lines(&mut self, count: usize) {
        self.summary_nonempty_lines = self.summary_nonempty_lines.max(count);
    }

    fn is_satisfied(&self, line_index: usize, nonempty_lines: usize) -> bool {
        self.fixed_last_line
            .is_none_or(|last_line| line_index >= last_line)
            && nonempty_lines >= self.summary_nonempty_lines
    }
}

fn snippet_read_plans(
    results: &[HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> (HashMap<PathBuf, SnippetReadPlan>, HashMap<usize, Symbol>) {
    let mut plans = HashMap::<PathBuf, SnippetReadPlan>::new();
    let mut rank0_targets = HashMap::new();

    for (rank, result) in results.iter().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            continue;
        }

        let plan = plans.entry(result.file.clone()).or_default();
        if matches!(result.kind, SymbolKind::FileSummary) {
            plan.include_summary_lines(budget);
            continue;
        }

        plan.include_through(result.end_line);
        if should_expand_rank0_snippet(rank, result, project_root) {
            let target =
                symbol_for_rank0_render(result, ctx).unwrap_or_else(|| symbol_from_result(result));
            plan.include_through(target.range.end_line);
            rank0_targets.insert(rank, target);
        }
    }

    (plans, rank0_targets)
}

fn read_bounded_snippet_lines(path: &Path, plan: SnippetReadPlan) -> Option<Vec<String>> {
    let file = fs::File::open(path).ok()?;
    let mut lines = Vec::new();
    let mut nonempty_lines = 0usize;

    // Read each source once and stop after every requested symbol range and
    // FileSummary budget is satisfied. Any error before that boundary discards
    // the whole partial read, matching the previous read_to_string behavior.
    for (line_index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.ok()?;
        if !line.trim().is_empty() {
            nonempty_lines += 1;
        }
        lines.push(line);

        if plan.is_satisfied(line_index, nonempty_lines) {
            break;
        }
    }

    Some(lines)
}

fn enrich_snippets_from_source_with_context(
    results: &mut [HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> bool {
    let (plans, rank0_targets) = snippet_read_plans(results, project_root, ctx);
    let file_lines = plans
        .into_iter()
        .map(|(path, plan)| {
            let lines = read_bounded_snippet_lines(&path, plan);
            (path, lines)
        })
        .collect::<HashMap<_, _>>();
    let mut incomplete = false;

    for (rank, result) in results.iter_mut().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            // Header-only tier: a real body means there is more to see.
            if result.end_line >= result.start_line {
                incomplete = true;
            }
            result.snippet = String::new();
            continue;
        }

        let lines = file_lines
            .get(&result.file)
            .and_then(|lines| lines.as_ref());

        if matches!(result.kind, SymbolKind::FileSummary) {
            if let Some(lines) = lines {
                result.snippet = lines
                    .iter()
                    .filter(|line| !line.trim().is_empty())
                    .take(budget)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            continue;
        }

        let Some(lines) = lines else {
            // File unreadable or gone — no snippet beats a stale one.
            result.snippet = String::new();
            continue;
        };

        // start_line/end_line are 0-based inclusive; +1 makes an exclusive bound.
        let start = (result.start_line as usize).min(lines.len());
        let end = ((result.end_line as usize) + 1).min(lines.len());
        if start >= end {
            result.snippet = String::new();
            continue;
        }

        if should_expand_rank0_snippet(rank, result, project_root) {
            let rendered =
                render_rank0_symbol_snippet(result, lines, ctx, rank0_targets.get(&rank));
            match rendered.status {
                BudgetedSymbolRenderStatus::Complete => {
                    // Append the full-body notice only for Complete so callers know
                    // they received the entire symbol source. Skip it for Truncated
                    // or Menu results.
                    result.snippet = append_rank0_full_symbol_notice(rendered.content);
                    continue;
                }
                BudgetedSymbolRenderStatus::Truncated | BudgetedSymbolRenderStatus::Menu => {
                    result.snippet = rendered.content;
                    incomplete = true;
                    continue;
                }
            }
        }

        let range_len = end - start;
        let shown = range_len.min(budget);
        let mut snippet = lines[start..start + shown].join("\n");
        let remaining = range_len - shown;
        if remaining > 0 {
            // "lines" is load-bearing: a bare "+N more" reads as "N more
            // results" to a weak model, prompting a wrong topK bump. This is
            // N more lines of THIS symbol's body — zoom to see them.
            snippet.push_str(&format!("\n+{remaining} more lines"));
            incomplete = true;
        }
        result.snippet = snippet;
    }

    incomplete
}

#[cfg(test)]
fn enrich_snippets_from_source_reference(
    results: &mut [HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> bool {
    let mut file_lines: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    let mut incomplete = false;

    for (rank, result) in results.iter_mut().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            if result.end_line >= result.start_line {
                incomplete = true;
            }
            result.snippet = String::new();
            continue;
        }

        let lines = file_lines.entry(result.file.clone()).or_insert_with(|| {
            fs::read_to_string(&result.file)
                .ok()
                .map(|content| content.lines().map(str::to_string).collect())
        });

        if matches!(result.kind, SymbolKind::FileSummary) {
            if let Some(lines) = lines {
                result.snippet = lines
                    .iter()
                    .filter(|line| !line.trim().is_empty())
                    .take(budget)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            continue;
        }

        let Some(lines) = lines else {
            result.snippet = String::new();
            continue;
        };

        let start = (result.start_line as usize).min(lines.len());
        let end = ((result.end_line as usize) + 1).min(lines.len());
        if start >= end {
            result.snippet = String::new();
            continue;
        }

        if should_expand_rank0_snippet(rank, result, project_root) {
            let rendered = render_rank0_symbol_snippet(result, lines, ctx, None);
            match rendered.status {
                BudgetedSymbolRenderStatus::Complete => {
                    result.snippet = append_rank0_full_symbol_notice(rendered.content);
                    continue;
                }
                BudgetedSymbolRenderStatus::Truncated | BudgetedSymbolRenderStatus::Menu => {
                    result.snippet = rendered.content;
                    incomplete = true;
                    continue;
                }
            }
        }

        let range_len = end - start;
        let shown = range_len.min(budget);
        let mut snippet = lines[start..start + shown].join("\n");
        let remaining = range_len - shown;
        if remaining > 0 {
            snippet.push_str(&format!("\n+{remaining} more lines"));
            incomplete = true;
        }
        result.snippet = snippet;
    }

    incomplete
}

fn render_rank0_symbol_snippet(
    result: &HybridResult,
    lines: &[String],
    ctx: Option<&AppContext>,
    planned_target: Option<&Symbol>,
) -> crate::commands::symbol_render::BudgetedSymbolRender {
    let fallback_target;
    let target = match planned_target {
        Some(target) => target,
        None => {
            fallback_target =
                symbol_for_rank0_render(result, ctx).unwrap_or_else(|| symbol_from_result(result));
            &fallback_target
        }
    };
    let outline = ctx.and_then(|ctx| {
        if might_have_container_members(target) {
            build_container_outline(ctx, &result.file, target).ok()
        } else {
            None
        }
    });

    render_symbol_within_budget(
        target,
        lines,
        crate::parser::detect_language(&result.file),
        outline.as_ref(),
        RANK0_FULL_SNIPPET_MAX_LINES,
    )
}

fn symbol_for_rank0_render(ctx_result: &HybridResult, ctx: Option<&AppContext>) -> Option<Symbol> {
    let symbols = ctx?.provider().list_symbols(&ctx_result.file).ok()?;
    symbols
        .iter()
        .find(|symbol| symbol_matches_result(symbol, ctx_result, true))
        .cloned()
        .or_else(|| {
            symbols
                .into_iter()
                .find(|symbol| symbol_matches_result(symbol, ctx_result, false))
        })
}

fn symbol_matches_result(symbol: &Symbol, result: &HybridResult, exact_range: bool) -> bool {
    symbol.name == result.name
        && symbol.kind == result.kind
        && (!exact_range
            || (symbol.range.start_line == result.start_line
                && symbol.range.end_line == result.end_line))
}

fn symbol_from_result(result: &HybridResult) -> Symbol {
    Symbol {
        name: result.name.clone(),
        kind: result.kind.clone(),
        range: Range {
            start_line: result.start_line,
            start_col: 0,
            end_line: result.end_line,
            end_col: 0,
        },
        signature: None,
        scope_chain: Vec::new(),
        exported: result.exported,
        parent: None,
    }
}

fn append_rank0_full_symbol_notice(content: String) -> String {
    if content.is_empty() {
        RANK0_FULL_SYMBOL_NOTICE.to_string()
    } else {
        format!("{content}\n{RANK0_FULL_SYMBOL_NOTICE}")
    }
}

fn should_expand_rank0_snippet(rank: usize, result: &HybridResult, project_root: &Path) -> bool {
    rank == 0
        && result
            .semantic_score
            .is_some_and(|cosine| cosine >= HIGH_CONFIDENCE_COSINE_FLOOR)
        && !path_is_test_support_file(&result.file, project_root)
}

fn format_result_sections(results: &[HybridResult], project_root: &Path) -> String {
    format_result_sections_with_context(results, project_root, None)
}

fn format_result_sections_with_context(
    results: &[HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> String {
    // Results arrive sorted by fused score desc. Group by file preserving
    // first-appearance order so the most relevant file's group renders first.
    // A BTreeMap would re-sort groups alphabetically by path and scramble the
    // ranking the agent relies on to read most-relevant-first. Snippets are
    // already budgeted by enrich_snippets_from_source; render them verbatim.
    let annotations = ctx
        .map(|ctx| blast_radius_annotations(ctx, results))
        .unwrap_or_else(|| vec![None; results.len()]);
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<(usize, &HybridResult)>> = HashMap::new();

    for (index, result) in results.iter().enumerate() {
        let display_path = result
            .file
            .strip_prefix(project_root)
            .unwrap_or(&result.file)
            .display()
            .to_string();
        if !groups.contains_key(&display_path) {
            group_order.push(display_path.clone());
        }
        groups
            .entry(display_path)
            .or_default()
            .push((index, result));
    }

    group_order
        .iter()
        .map(|file| {
            let mut section = file.clone();

            // Three distinct indent levels disambiguate the three roles for a
            // weak model at a glance: file path at col 0 (with its `/` and
            // extension), symbol header at 2 spaces, snippet body at 6. Without
            // this, file paths and symbol headers were both at col 0 and could
            // only be told apart by parsing the "[kind] lines X-Y" suffix.
            for (index, result) in &groups[file] {
                if result.source == "lexical" {
                    // Whole-file lexical match (no specific symbol).
                    section.push_str(" [lexical match]");
                    continue;
                }
                if matches!(result.kind, SymbolKind::FileSummary) {
                    section.push_str(&format!("\n  {} [file summary]", result.name));
                } else {
                    section.push_str(&format!(
                        "\n  {} [{}] lines {}-{}{}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        display_line_number(result.start_line),
                        display_line_number(result.end_line),
                        annotations
                            .get(*index)
                            .and_then(|annotation| annotation.as_deref())
                            .unwrap_or("")
                    ));
                }
                if !result.snippet.trim().is_empty() {
                    for line in result.snippet.lines() {
                        section.push_str("\n      ");
                        section.push_str(line);
                    }
                }
            }

            section
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn blast_radius_annotations(ctx: &AppContext, results: &[HybridResult]) -> Vec<Option<String>> {
    let Some(store) = warm_callgraph_store(ctx) else {
        return vec![None; results.len()];
    };

    results
        .iter()
        .map(|result| blast_radius_annotation_for_result(&store, result))
        .collect()
}

fn warm_callgraph_store(
    ctx: &AppContext,
) -> Option<std::sync::Arc<crate::callgraph_store::ReadonlyCallGraphStore>> {
    let receiver = ctx.callgraph_store_rx().try_lock()?;
    if receiver.is_some() {
        return None;
    }
    drop(receiver);
    try_read_with_budget(ctx.callgraph_store(), INTERACTIVE_ARTIFACT_READ_BUDGET)
        .and_then(|store| store.as_ref().map(std::sync::Arc::clone))
}

fn blast_radius_annotation_for_result(
    store: &crate::callgraph_store::ReadonlyCallGraphStore,
    result: &HybridResult,
) -> Option<String> {
    if result.source == "lexical" || matches!(result.kind, SymbolKind::FileSummary) {
        return None;
    }
    if result.name.trim().is_empty() {
        return None;
    }

    let callers = callers_result(store, &result.file, &result.name, 1, true).ok()?;
    let mut caller_basenames = Vec::new();
    let mut seen_files = HashSet::new();
    for group in &callers.callers {
        if seen_files.insert(group.file.clone()) {
            caller_basenames.push(compact_caller_basename(&group.file));
        }
    }

    let mut suffix = format!("  ↩{}", callers.total_callers);
    if !caller_basenames.is_empty() {
        let more = caller_basenames.len() > 2;
        let names = caller_basenames
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        suffix.push(' ');
        suffix.push_str(&names);
        if more {
            suffix.push_str(",…");
        }
    }
    Some(suffix)
}

fn compact_caller_basename(file: &str) -> String {
    let basename = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file);
    truncate_chars(basename, 18)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn result_to_json(result: &HybridResult) -> serde_json::Value {
    let is_file_level = matches!(result.kind, SymbolKind::FileSummary);
    let (start_line, end_line) = if is_file_level {
        (serde_json::Value::Null, serde_json::Value::Null)
    } else {
        (
            serde_json::json!(display_line_number(result.start_line)),
            serde_json::json!(display_line_number(result.end_line)),
        )
    };

    serde_json::json!({
        "file": result.file.display().to_string(),
        "name": result.name,
        "kind": result.kind,
        "start_line": start_line,
        "end_line": end_line,
        "location": if result.source == "lexical" { "[lexical match]" } else if is_file_level { "[file summary]" } else { "line range" },
        "score": result.score,
        "source": result.source,
        "semantic_score": result.semantic_score,
        "lexical_score": result.lexical_score,
        "hybrid_boosted": result.hybrid_boosted,
        "snippet": result.snippet,
    })
}

fn grep_match_to_json(grep_match: &GrepMatch, source: &'static str) -> serde_json::Value {
    serde_json::json!({
        "kind": "GrepLine",
        "source": source,
        "file": grep_match.file.display().to_string(),
        "line": grep_match.line,
        "column": grep_match.column,
        "line_text": grep_match.line_text,
        "match_text": grep_match.match_text,
    })
}

fn display_line_number(line: u32) -> u32 {
    line.saturating_add(1)
}

fn symbol_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file-summary",
    }
}

fn semantic_status_label(status: &SemanticIndexStatus) -> &'static str {
    match status {
        SemanticIndexStatus::Ready { .. } => "ready",
        SemanticIndexStatus::Building { .. } => "building",
        SemanticIndexStatus::Disabled => "disabled",
        SemanticIndexStatus::Failed(_) => "unavailable",
    }
}

fn interpreted_as_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Regex => "regex",
        SearchMode::Literal => "literal",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
}

/// Honest `interpreted_as` for a response built on a semantic-unavailable
/// fallback path. The query may have been *routed* as semantic/hybrid, but if
/// semantic never executed, the field must report what actually produced the
/// results — otherwise an agent reads "hybrid" and trusts a semantic ranking
/// that never ran. `lexical_ran` is true when the lexical (trigram) lane
/// produced the returned results; otherwise we report the routed mode (the
/// attempt), with the `semantic_unavailable`/`status` fields conveying that it
/// could not run.
fn fallback_executed_label(mode: SearchMode, lexical_ran: bool) -> &'static str {
    if lexical_ran {
        "lexical"
    } else {
        interpreted_as_label(mode)
    }
}

fn query_kind_label(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Identifier => "Identifier",
        QueryKind::Mixed => "Mixed",
        QueryKind::ErrorCode => "ErrorCode",
        QueryKind::Path => "Path",
        QueryKind::Regex => "Regex",
        QueryKind::NaturalLanguage => "NaturalLanguage",
    }
}

/// Strip a single matched pair of surrounding `"` or `'` from a literal
/// query, matching the convention agents and humans bring from GitHub code
/// search, `rg -F "..."`, and most search engines. Only strips ONE pair, and
/// only when leading + trailing match — `'foo"` is left alone, and pre-stripped
/// queries like `foo` are returned unchanged.
fn strip_surrounding_quotes(query: String) -> String {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return query;
    }
    let first = trimmed.chars().next().unwrap();
    let last = trimmed.chars().next_back().unwrap();
    if (first == '"' || first == '\'') && first == last {
        let mut chars = trimmed.chars();
        chars.next();
        chars.next_back();
        return chars.as_str().to_string();
    }
    query
}

fn extracted_tokens_all_short(query: &str, shape: &QueryShape) -> bool {
    let tokens = query_shape::extract_tokens(query, shape);
    !tokens.is_empty() && tokens.iter().all(|token| token.len() < 3)
}

pub fn humanize_degraded_reasons(reasons: &[String]) -> Vec<String> {
    reasons.iter().map(|code| humanize_one(code)).collect()
}

fn humanize_one(code: &str) -> String {
    if code == "home_root" {
        return "Project root is set to your home directory; large file-system indexes are disabled to avoid scanning the whole home tree.".into();
    }
    if code == "watcher_unavailable" {
        return "file watcher unavailable; continuing without live external-change invalidation"
            .to_string();
    }
    format!("(Degraded: {})", code)
}

fn degraded_warning(ctx: &AppContext) -> String {
    let mut text = "Lexical search ran in degraded full-file-scan mode.".to_string();
    let reasons = ctx.degraded_reasons();
    if !reasons.is_empty() {
        text.push_str(" Reasons: ");
        text.push_str(&humanize_degraded_reasons(&reasons).join("; "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callgraph::walk_project_files;
    use crate::callgraph_store::CallGraphStore;
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::{
        callgraph_cold_build_spawn_count_for_test, reset_callgraph_cold_build_spawn_count_for_test,
        AppContext,
    };
    use crate::parser::TreeSitterProvider;
    use crate::semantic_index::SemanticIndex;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn semantic_request(query: &str, top_k: usize) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
        }))
        .expect("build semantic search request")
    }

    fn semantic_request_with_hint(query: &str, top_k: usize, hint: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
            "hint": hint,
        }))
        .expect("build semantic search request")
    }

    fn response_value(response: Response) -> serde_json::Value {
        serde_json::to_value(response).expect("serialize response")
    }

    fn test_context(project_root: &Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project_root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn install_warm_callgraph_store(ctx: &AppContext, project_root: &Path) {
        let root = std::fs::canonicalize(project_root).expect("canonical project root");
        let files = walk_project_files(&root).collect::<Vec<_>>();
        let store_dir = root.join(".callgraph-store-test");
        let store =
            CallGraphStore::open(store_dir.clone(), root.clone()).expect("open callgraph store");
        store.cold_build(&files).expect("build callgraph store");
        drop(store);
        let store = CallGraphStore::open_readonly(store_dir, root)
            .expect("open read-only callgraph store")
            .expect("ready callgraph store");
        *ctx.callgraph_store()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(store));
    }

    fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).expect("read embedding request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if buf.len() >= end + content_length {
                        break;
                    }
                }
            }

            let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write embedding response");
        });

        (format!("http://{}", addr), handle)
    }

    #[test]
    fn embed_query_construction_error_leaves_slot_empty_for_retry() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic: SemanticBackendConfig {
                    backend: SemanticBackend::OpenAiCompatible,
                    model: "test-embedding".to_string(),
                    base_url: None,
                    api_key_env: None,
                    timeout_ms: 5_000,
                    query_timeout_ms: 3_000,
                    max_batch_size: 64,
                    max_files: 20_000,
                    ..Default::default()
                },
                ..Config::default()
            },
        );

        let err = embed_query("anything", &ctx).expect_err("construction should fail");
        assert!(
            err.contains("base_url is required"),
            "expected missing base_url construction error, got: {err}"
        );
        assert!(
            ctx.semantic_embedding_model().lock().is_none(),
            "failed model construction must not poison the lazy slot"
        );

        let (base_url, handle) = start_mock_embedding_server();
        ctx.update_config(|config| {
            config.semantic.base_url = Some(base_url);
        });

        let vector = embed_query("anything", &ctx).expect("retry should construct and embed");
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
        assert!(
            ctx.semantic_embedding_model().lock().is_some(),
            "successful retry should install the constructed model"
        );
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn classify_embed_query_error_names_timeout_budget_and_knob() {
        // A query-embedding timeout carries the budget that fired. The
        // classified detail must name the mechanism, the budget value, and the
        // knob that raises it — and the footer reason must carry the budget so
        // the agent sees the cause in the status line, not just the body.
        let timeout_error = format!(
            "failed to embed query: {}openai compatible request failed: operation timed out",
            crate::semantic_index::query_embedding_timeout_marker(3_000)
        );
        let classified = classify_embed_query_error(&timeout_error);
        assert!(
            classified.detail.contains("timed out after 3000ms"),
            "timeout detail must name the budget: {}",
            classified.detail
        );
        assert!(
            classified.detail.contains("semantic.query_timeout_ms"),
            "timeout detail must name the knob: {}",
            classified.detail
        );
        assert!(
            classified.detail.contains("raise it for slow providers"),
            "timeout detail must name the remedy: {}",
            classified.detail
        );
        assert_eq!(classified.footer_reason, "query embed timeout (3000ms)");
    }

    #[test]
    fn classify_embed_query_error_non_timeout_keeps_current_shape() {
        // A non-timeout failure (HTTP 4xx, connection refused, dimension
        // mismatch) must NOT claim a timeout. It keeps the current message
        // shape and the plain "unavailable" footer reason.
        for non_timeout in [
            "failed to embed query: openai compatible request failed (HTTP 401): Unauthorized",
            "failed to embed query: openai compatible request failed: connection refused",
            "semantic embedding dimension mismatch: query backend returned 768, index expects 384",
        ] {
            let classified = classify_embed_query_error(non_timeout);
            assert!(
                !classified.detail.contains("timed out"),
                "non-timeout must not claim timeout: {}",
                classified.detail
            );
            assert!(
                !classified.detail.contains("query_timeout_ms"),
                "non-timeout must not name the knob: {}",
                classified.detail
            );
            assert_eq!(classified.footer_reason, "unavailable");
            assert!(
                classified
                    .detail
                    .starts_with("Semantic search unavailable: "),
                "non-timeout keeps current message shape: {}",
                classified.detail
            );
        }
    }

    #[test]
    fn short_nl_concept_routes_to_hybrid_when_lexical_ready() {
        // "parse imports" classifies as a two-word lowercase NL concept, but it
        // is a literal code phrase the trigram lane can hit. With lexical ready
        // it must route to Hybrid (run the lexical lane), not pure Semantic.
        let shape = query_shape::classify("parse imports");
        assert_eq!(shape.kind, QueryKind::NaturalLanguage);
        let mut warnings = Vec::new();
        let mode = choose_mode("parse imports", &shape, true, &mut warnings);
        assert_eq!(mode, SearchMode::Hybrid);
    }

    #[test]
    fn long_nl_phrase_stays_semantic() {
        // A longer NL phrase (>2 words) is a genuine concept query → pure
        // Semantic; the lexical lane would only add noise.
        let q = "how does the bridge resolve the binary";
        let shape = query_shape::classify(q);
        assert_eq!(shape.kind, QueryKind::NaturalLanguage);
        let mut warnings = Vec::new();
        let mode = choose_mode(q, &shape, true, &mut warnings);
        assert_eq!(mode, SearchMode::Semantic);
    }

    #[test]
    fn short_nl_extracts_lexical_tokens() {
        // The short-NL Hybrid path needs tokens; extract_tokens returns none for
        // NL, so collect_lexical_files uses the short-NL extractor.
        let tokens = query_shape::extract_short_nl_lexical_tokens("parse imports");
        assert_eq!(tokens, vec!["parse".to_string(), "imports".to_string()]);
        // Sub-3-char words are dropped (trigram floor).
        let tokens2 = query_shape::extract_short_nl_lexical_tokens("go to");
        assert!(tokens2.is_empty());
    }

    #[test]
    fn building_status_returns_lexical_fallback_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn needle_symbol() -> bool { true }\n";
        std::fs::write(&source_file, source).expect("write source file");

        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, source.as_bytes());
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "embedding".to_string(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
        };

        let response = response_value(handle_semantic_search(
            &semantic_request("needle_symbol", 5),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "building");
        assert_eq!(response["semantic_status"], "building");
        // While semantic builds, only the lexical lane produced results — so
        // interpreted_as honestly reports "lexical", not the routed "hybrid"
        // mode that hasn't executed yet. The "building" status + note convey
        // that semantic results are still coming.
        assert_eq!(response["interpreted_as"], "lexical");
        assert!(response["note"]
            .as_str()
            .expect("note")
            .contains("lexical-only fallback"));
        let text = response["text"].as_str().expect("text");
        assert!(text.contains("lexical fallback"));
        assert!(text.contains("Semantic index is rebuilding"));
        assert!(!text.contains(BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS));
        let results = response["results"].as_array().expect("results array");
        assert!(
            results.iter().any(|result| {
                result["source"] == "lexical"
                    && result["file"]
                        .as_str()
                        .expect("file")
                        .ends_with("src/lib.rs")
            }),
            "expected lexical fallback result, got {results:?}"
        );
    }

    #[test]
    fn borrowed_loading_with_lexical_results_describes_shared_lane_truthfully() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn borrowed_loading_needle() {}\n";
        std::fs::write(&source_file, source).expect("write source file");

        let ctx = test_context(project.path());
        ctx.set_cache_role(true, None);
        let mut index = SearchIndex::new();
        index.index_file(&source_file, source.as_bytes());
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };

        let response = response_value(handle_semantic_search(
            &semantic_request("borrowed_loading_needle", 5),
            &ctx,
        ));
        let text = response["text"].as_str().expect("response text");
        assert!(text.contains(
            "Semantic lane is loading the shared index; lexical results below are complete for exact/identifier matches."
        ));
        assert!(!text.contains("still building"));
        assert!(!text.contains("rebuilding"));
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
    }

    #[test]
    fn loading_artifacts_without_lexical_index_reports_one_actionable_status() {
        let text = format_building_lexical_text(
            "Semantic index is still building (stage: loading_artifacts).",
            &[],
            Path::new("/fixture"),
            false,
            true,
            false,
        );

        assert!(text.contains("Index artifacts are loading for this root"));
        assert!(text.contains("nothing was searched yet"));
        assert!(text.contains("Retry in a few seconds"));
        assert!(!text.contains("Found 0"));
        assert!(!text.contains("rebuilding"));
        assert!(!text.contains(BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS));
    }

    #[test]
    fn empty_lexical_fallback_names_missing_semantic_coverage() {
        let text = format_lexical_unavailable_text(
            "Semantic index is loading.",
            &[],
            Path::new("/fixture"),
            "loading",
        );

        assert!(text.contains("0 lexical matches"));
        assert!(text.contains("semantic lane is unavailable"));
        assert!(text.contains("prose-style queries may match only via semantic"));
        assert!(!text.contains("lexical-only fallback returned 0"));
    }

    #[test]
    fn empty_degraded_grep_fallback_names_missing_semantic_coverage() {
        let result = GrepResult {
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: IndexStatus::Fallback,
            truncated: false,
            fully_degraded: true,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
        };
        let text = format_grep_lexical_unavailable_text(
            "Semantic index is loading.",
            &result,
            Path::new("/fixture"),
            "loading",
        );

        assert!(text.contains("0 lexical matches"));
        assert!(text.contains("semantic lane is unavailable"));
        assert!(!text.contains("lexical-only fallback returned 0"));
    }

    #[test]
    fn first_search_waits_for_slow_borrowed_base_and_returns_results() {
        assert_eq!(
            FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET,
            Duration::from_millis(2_500)
        );
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn waited_for_borrowed_artifact() {}\n";
        std::fs::write(&source_file, source).expect("write source file");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };

        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(rx, ctx.configure_generation());
        let publish_file = source_file.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut index = SearchIndex::new();
            index.index_file(&publish_file, source.as_bytes());
            index.ready = true;
            tx.send(index).expect("publish search index");
        });

        // Decision-evidence form: non-empty results ARE the proof the query
        // waited for the publish (without waiting, status is Building and the
        // reply is the honest "nothing was searched yet"). Elapsed-time bounds
        // were removed because a loaded runner can deschedule either the
        // publisher thread or this thread past any tight wall-clock budget;
        // the generous budget below is a hang catch, not a timing assertion.
        let response =
            with_first_search_index_load_wait_budget_for_test(Duration::from_secs(30), || {
                response_value(handle_semantic_search(
                    &semantic_request("waited_for_borrowed_artifact", 5),
                    &ctx,
                ))
            });

        assert_eq!(response["interpreted_as"], "lexical");
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
        assert!(!response["text"]
            .as_str()
            .expect("response text")
            .contains("nothing was searched yet"));
    }

    #[test]
    fn first_search_wait_budget_expires_with_honest_loading_reply() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (_tx, rx) = crossbeam_channel::unbounded::<SearchIndex>();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        let wait_budget = Duration::from_millis(40);
        let started = Instant::now();
        let response = with_first_search_index_load_wait_budget_for_test(wait_budget, || {
            response_value(handle_semantic_search(
                &semantic_request("still_loading", 5),
                &ctx,
            ))
        });

        assert!(started.elapsed() >= wait_budget);
        assert!(started.elapsed() < Duration::from_secs(1));
        let text = response["text"].as_str().expect("response text");
        assert!(text.contains("nothing was searched yet"));
        assert!(text.contains("Retry in a few seconds"));
        assert!(!text.contains("Found 0"));
    }

    #[test]
    fn first_search_and_in_progress_load_complete_without_deadlock() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn no_deadlock_borrowed_artifact() {}\n";
        std::fs::write(&source_file, source).expect("write source file");
        let ctx = Arc::new(test_context(project.path()));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        let publish_file = source_file.clone();
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            let mut index = SearchIndex::new();
            index.index_file(&publish_file, source.as_bytes());
            index.ready = true;
            tx.send(index).expect("publish search index");
        });
        let search_ctx = Arc::clone(&ctx);
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        let search = std::thread::spawn(move || {
            let response = with_first_search_index_load_wait_budget_for_test(
                Duration::from_millis(200),
                || {
                    response_value(handle_semantic_search(
                        &semantic_request("no_deadlock_borrowed_artifact", 5),
                        &search_ctx,
                    ))
                },
            );
            completed_tx
                .send(response)
                .expect("publish search response");
        });

        let response = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("search and artifact publication must not deadlock");
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
        publisher.join().expect("publisher joins");
        search.join().expect("search joins");
    }

    #[test]
    fn first_search_wait_observes_executor_cancellation() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = Arc::new(test_context(project.path()));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (_tx, rx) = crossbeam_channel::unbounded::<SearchIndex>();
        ctx.install_search_index_rx(rx, ctx.configure_generation());
        let cancellation = crate::executor::JobCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker_ctx = Arc::clone(&ctx);
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            let _installed = crate::executor::install_job_cancellation(worker_cancellation);
            started_tx.send(()).expect("signal wait start");
            let response =
                with_first_search_index_load_wait_budget_for_test(Duration::from_secs(2), || {
                    response_value(handle_semantic_search(
                        &semantic_request("cancelled_wait", 5),
                        &worker_ctx,
                    ))
                });
            completed_tx
                .send(response)
                .expect("publish cancelled response");
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("search wait starts");
        std::thread::sleep(Duration::from_millis(20));
        cancellation.request_cancel();
        let response = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled wait completes promptly");
        assert_eq!(response["code"], "request_cancelled");
        worker.join().expect("cancelled search joins");
    }

    #[test]
    fn read_only_failed_snapshot_retries_on_semantic_query() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let ctx = test_context(project.path());
        ctx.update_config(|config| {
            config.semantic_search = true;
            config.storage_dir = Some(storage.path().to_path_buf());
        });
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        ctx.set_cache_writer_capabilities(false, true);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            SemanticIndexStatus::Failed("shared snapshot absent".to_string());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("retry snapshot", 5, "semantic"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "ready");
        assert_eq!(response["semantic_status"], "building");
        assert!(response["text"]
            .as_str()
            .expect("semantic fallback text")
            .contains("semantic lane is unavailable"));
        assert!(ctx.semantic_index_rx().lock().is_some());
        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
    }

    #[test]
    fn regex_query_runs_without_semantic_index() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn exported() {}\n").expect("write source file");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint(".*exported", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "regex");
        assert_eq!(response["query_kind"], "Regex");
        assert_eq!(response["semantic_status"], "disabled");
        assert_eq!(response["results"][0]["kind"], "GrepLine");
    }

    #[test]
    fn auto_regexlike_uncompilable_query_falls_back_to_literal() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        // The fallback recompiles as an escaped literal, so the exact needle
        // must be present for this test to produce a match.
        std::fs::write(
            &source_file,
            "// assert_ne!(.*route_channel\nassert_ne!(route_channel, 0);\n",
        )
        .expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("assert_ne!(.*route_channel", 5, "auto"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        let results = response["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "expected literal fallback result, got {results:?}"
        );
        assert_eq!(response["results"][0]["source"], "literal");
        assert_eq!(
            response["results"][0]["match_text"],
            "assert_ne!(.*route_channel"
        );
        let warnings = response["warnings"].as_array().expect("warnings array");
        let fallback_warning = warnings
            .iter()
            .filter_map(|warning| warning.as_str())
            .find(|warning| warning.contains("searched literally instead"))
            .expect("fallback warning");
        assert!(fallback_warning.contains("unclosed group"));
        assert!(fallback_warning.contains("Use grep when explicit regex lane control is required."));
    }

    #[test]
    fn legacy_regex_hint_is_ignored_for_uncompilable_query() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("assert_ne!(.*route_channel", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["text"]
            .as_str()
            .expect("literal fallback text")
            .contains("Found 0"));
    }

    #[test]
    fn valid_auto_regex_query_stays_regex_without_fallback_warning() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "let route_alpha_channel = 1;\n").expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("route_.*channel", 5, "auto"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "regex");
        assert_eq!(response["results"][0]["source"], "regex");
        let warnings = response["warnings"].as_array().expect("warnings array");
        assert!(!warnings.iter().any(|warning| {
            warning
                .as_str()
                .expect("warning")
                .contains("searched literally instead")
        }));
    }

    #[test]
    fn auto_short_token_warns_and_runs_grep_line_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "id = 1\n").expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("id", 5, "literal"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["warnings"][0]
            .as_str()
            .expect("warning")
            .contains("shorter than 3"));
    }

    #[test]
    fn unsupported_regex_auto_falls_back_to_literal() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("(?=foo)", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("searched literally instead"))));
    }

    #[test]
    fn regex_zero_results_escalate_once_to_hybrid_terms() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(
            &source_file,
            "const missing_route = true;\npub fn exported() {}\n",
        )
        .expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request("^missing_route$", 5),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["result_count"], 1);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| result["source"] == "lexical"));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("[interpreted_as: regex; no exact match — ranked by terms instead]"));

        let first_project = tempfile::tempdir().expect("create first-lane project");
        let first_source = first_project.path().join("src/lib.rs");
        std::fs::create_dir_all(first_source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&first_source, "pub fn exported() {}\n").expect("write source file");
        let first_ctx = test_context(first_project.path());
        *first_ctx
            .semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
        let first_lane_response = response_value(handle_semantic_search(
            &semantic_request_with_hint(".*exported", 5, "regex"),
            &first_ctx,
        ));
        assert!(
            first_lane_response["result_count"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "response: {first_lane_response:?}"
        );
        assert_eq!(
            first_lane_response
                .get("zero_result_escalation")
                .and_then(Value::as_bool),
            None,
            "response: {first_lane_response:?}"
        );
        assert!(!first_lane_response["text"]
            .as_str()
            .expect("text")
            .contains("no exact match"));
    }

    #[test]
    fn external_regex_zero_results_escalate_when_borrowed_lexical_index_is_ready() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const missing_route = true;\n").expect("write source file");
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        let query = "^missing_route$";
        let shape = query_shape::classify(query);
        let response = response_value(handle_external_grep_search(
            &semantic_request(query, 5),
            query,
            5,
            &shape,
            SearchMode::Regex,
            Vec::new(),
            project.path(),
            false,
            &index,
            &ExternalBorrowMetadata::default(),
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| result["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("src/reminder.rs"))));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("[interpreted_as: regex; no exact match — ranked by terms instead]"));
    }

    #[test]
    fn quoted_natural_language_zero_results_find_terms_in_one_escalation() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const template = \"outside <touser>\";\n")
            .expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(project.path().to_path_buf(), 3));
        let (base_url, handle) = start_mock_embedding_server();
        ctx.update_config(|config| {
            config.semantic.backend = SemanticBackend::OpenAiCompatible;
            config.semantic.base_url = Some(base_url);
            config.semantic.model = "test-embedding".to_string();
        });

        let response = response_value(handle_semantic_search(
            &semantic_request("\"outside <touser>\" reminder text", 5),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| result["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("src/reminder.rs"))));
        assert!(response["text"].as_str().expect("text").contains(
            "[interpreted_as: semantic; no result above cutoff — ranked by terms instead]"
        ));
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn garbage_regex_zero_after_escalation_is_honest_zero() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const present_route = true;\n").expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request("^qzxjvpl_888$", 5),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["result_count"], 0);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("[interpreted_as: regex; no exact match — ranked by terms instead]"));
    }

    #[test]
    fn humanize_degraded_reason_messages() {
        let reasons = vec![
            "home_root".to_string(),
            "watcher_unavailable".to_string(),
            "custom".to_string(),
        ];
        let human = humanize_degraded_reasons(&reasons);
        assert!(human[0].contains("home directory"));
        assert_eq!(
            human[1],
            "file watcher unavailable; continuing without live external-change invalidation"
        );
        assert_eq!(human[2], "(Degraded: custom)");
        assert!(human.join("; ").contains("; "));
    }

    #[test]
    fn semantic_candidate_limit_scales_with_small_top_k() {
        assert_eq!(semantic_candidate_limit(1), SEMANTIC_OVERFETCH_FLOOR);
        assert_eq!(semantic_candidate_limit(5), 15);
        assert_eq!(semantic_candidate_limit(100), MAX_TOP_K);
    }

    fn rerank_shape(kind: QueryKind) -> QueryShape {
        QueryShape {
            kind,
            weights: query_shape::ShapeWeights {
                semantic: 0.0,
                lexical: 0.0,
                should_use_lexical: false,
            },
        }
    }

    fn semantic_candidate(
        file: &str,
        name: &str,
        qualified_name: Option<&str>,
        kind: SymbolKind,
        score: f32,
    ) -> SemanticResult {
        SemanticResult {
            file: PathBuf::from(file),
            name: name.to_string(),
            qualified_name: qualified_name.map(str::to_string),
            kind,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score,
            rank_score: score,
            cap_protected: false,
            source: "semantic",
        }
    }

    fn candidate_rank<'a>(results: &'a [SemanticResult], name: &str) -> &'a SemanticResult {
        results
            .iter()
            .find(|result| result.name == name)
            .expect("candidate present")
    }

    #[test]
    fn type_concept_identifier_detector_fires_only_for_titlecase_concepts() {
        for query in [
            "Engine implementations",
            "Engine handlers",
            "Allocation strategies",
            "EngineFactory implementations",
        ] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::Identifier, "{query}");
            assert!(
                query_shape::is_type_concept_identifier_query(query, &shape),
                "{query} should get definition priors"
            );
        }

        for query in [
            "engineFactory",
            "useState hook",
            "parseConfig option",
            "Engine",
        ] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::Identifier, "{query}");
            assert!(
                !query_shape::is_type_concept_identifier_query(query, &shape),
                "{query} should keep Identifier priors inert"
            );
        }

        for query in ["get user", "parse config"] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::NaturalLanguage, "{query}");
            assert!(!query_shape::is_type_concept_identifier_query(
                query, &shape
            ));
        }
    }

    #[test]
    fn type_concept_identifier_kind_prior_raises_definitions() {
        let shape = rerank_shape(QueryKind::Identifier);
        let candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "engineCache",
                None,
                SymbolKind::Variable,
                0.80,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "engineState",
                None,
                SymbolKind::Variable,
                0.79,
            ),
            semantic_candidate(
                "/project/src/renderer.ts",
                "Renderer",
                Some("Renderer"),
                SymbolKind::Class,
                0.75,
            ),
        ];

        let mut type_concept_candidates = candidates.clone();
        rerank_semantic_candidates(
            &mut type_concept_candidates,
            &shape,
            "Engine implementations",
        );
        let type_concept_results = fuse_hybrid_results(
            type_concept_candidates,
            Vec::new(),
            &shape,
            10,
            true,
            Path::new("/project"),
        );
        assert_eq!(type_concept_results[0].name, "Renderer");

        let mut plain_identifier_candidates = candidates;
        rerank_semantic_candidates(&mut plain_identifier_candidates, &shape, "engineFactory");
        assert!(plain_identifier_candidates
            .iter()
            .all(|result| (result.rank_score - result.score).abs() < f32::EPSILON));
        let plain_identifier_results = fuse_hybrid_results(
            plain_identifier_candidates,
            Vec::new(),
            &shape,
            10,
            true,
            Path::new("/project"),
        );
        assert_eq!(plain_identifier_results[0].name, "engineCache");
        assert_eq!(plain_identifier_results[1].name, "engineState");
        assert_eq!(plain_identifier_results[2].name, "Renderer");
    }

    #[test]
    fn type_concept_identifier_diversity_cap_limits_repeated_clusters_only() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut repeated_candidates = vec![
            semantic_candidate(
                "/project/src/a.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.90,
            ),
            semantic_candidate(
                "/project/src/b.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.89,
            ),
            semantic_candidate(
                "/project/src/c.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.88,
            ),
        ];
        rerank_semantic_candidates(&mut repeated_candidates, &shape, "Engine implementations");
        assert_eq!(repeated_candidates.len(), 2);
        assert!(repeated_candidates
            .iter()
            .all(|result| result.name == "Engine"));

        let mut distinct_candidates = vec![
            semantic_candidate(
                "/project/src/renderer.ts",
                "Renderer",
                Some("Renderer"),
                SymbolKind::Class,
                0.80,
            ),
            semantic_candidate(
                "/project/src/parser.ts",
                "Parser",
                Some("Parser"),
                SymbolKind::Class,
                0.79,
            ),
            semantic_candidate(
                "/project/src/planner.ts",
                "Planner",
                Some("Planner"),
                SymbolKind::Class,
                0.78,
            ),
        ];
        rerank_semantic_candidates(&mut distinct_candidates, &shape, "Engine implementations");
        assert_eq!(distinct_candidates.len(), 3);
        assert!(distinct_candidates
            .iter()
            .all(|result| result.rank_score > result.score));
    }

    #[test]
    fn type_concept_identifier_exact_name_boost_composes_with_kind_prior() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.70,
            ),
            semantic_candidate(
                "/project/src/renderer.ts",
                "Renderer",
                Some("Renderer"),
                SymbolKind::Class,
                0.75,
            ),
        ];

        rerank_semantic_candidates(&mut candidates, &shape, "Engine implementations");
        let named = candidate_rank(&candidates, "Engine");
        let sibling = candidate_rank(&candidates, "Renderer");

        assert!(named.rank_score > sibling.rank_score);
        assert!((named.rank_score - (0.70 * 1.08 * 1.20)).abs() < 0.0001);
        assert!((sibling.rank_score - (0.75 * 1.08)).abs() < 0.0001);
    }

    #[test]
    fn natural_language_kind_prior_and_semantic_only_fuse_raise_definitions() {
        let shape = rerank_shape(QueryKind::NaturalLanguage);
        let mut candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.80,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "engineCache",
                None,
                SymbolKind::Variable,
                0.79,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "engineState",
                None,
                SymbolKind::Variable,
                0.78,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.76,
            ),
        ];

        rerank_semantic_candidates(&mut candidates, &shape, "Engine implementations");
        let results = fuse_hybrid_results(
            candidates,
            Vec::new(),
            &shape,
            10,
            true,
            Path::new("/project"),
        );

        assert_eq!(results[0].name, "Engine");
        assert_eq!(results[0].semantic_score, Some(0.76));
        assert!(
            results[0].score > 0.80,
            "definition prior should drive semantic-only ordering"
        );
    }

    #[test]
    fn natural_language_diversity_cap_limits_repeated_name_kind_clusters() {
        let nl_shape = rerank_shape(QueryKind::NaturalLanguage);
        let mixed_shape = rerank_shape(QueryKind::Mixed);
        let candidates = vec![
            semantic_candidate(
                "/project/src/a.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.90,
            ),
            semantic_candidate(
                "/project/src/b.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.89,
            ),
            semantic_candidate(
                "/project/src/c.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.88,
            ),
        ];

        let mut nl_candidates = candidates.clone();
        rerank_semantic_candidates(
            &mut nl_candidates,
            &nl_shape,
            "engine factory implementations",
        );
        assert_eq!(nl_candidates.len(), 2);

        let mut mixed_candidates = candidates;
        rerank_semantic_candidates(
            &mut mixed_candidates,
            &mixed_shape,
            "engineFactory implementations",
        );
        assert_eq!(mixed_candidates.len(), 3);
        assert!(mixed_candidates
            .iter()
            .all(|result| (result.rank_score - result.score).abs() < f32::EPSILON));
    }

    #[test]
    fn exact_identifier_definition_boost_and_cap_protection_survive_file_cap() {
        let shape = rerank_shape(QueryKind::Identifier);
        let file = "/project/src/allocation.ts";
        let mut candidates = vec![
            semantic_candidate(file, "unrelatedField", None, SymbolKind::Variable, 0.99),
            semantic_candidate(file, "unrelatedMethod", None, SymbolKind::Method, 0.98),
            semantic_candidate(
                file,
                "AllocationService",
                Some("AllocationService"),
                SymbolKind::Class,
                0.80,
            ),
            semantic_candidate(file, "allocationService", None, SymbolKind::Variable, 0.79),
            semantic_candidate(file, "allocate", None, SymbolKind::Method, 0.78),
        ];

        rerank_semantic_candidates(&mut candidates, &shape, "AllocationService");
        let class = candidate_rank(&candidates, "AllocationService");
        assert!(class.rank_score > candidate_rank(&candidates, "allocationService").rank_score);
        assert!(class.rank_score > candidate_rank(&candidates, "allocate").rank_score);
        assert!(class.cap_protected);

        let fused = fuse_hybrid_results(
            candidates,
            vec![(PathBuf::from(file), 1.0)],
            &shape,
            10,
            true,
            Path::new("/project"),
        );

        assert!(fused
            .iter()
            .any(|result| result.name == "AllocationService"));
        assert_eq!(
            fused
                .iter()
                .filter(|result| result.file.as_path() == Path::new(file))
                .count(),
            3,
            "protected exact-name hit should be reserved in addition to two ordinary siblings"
        );
    }

    #[test]
    fn qualified_name_tokens_rank_nested_definitions_over_outer_types() {
        let nl_shape = rerank_shape(QueryKind::NaturalLanguage);
        let id_shape = rerank_shape(QueryKind::Identifier);
        let candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.80,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "Index",
                Some("Engine.Index"),
                SymbolKind::Class,
                0.70,
            ),
        ];

        let mut spaced = candidates.clone();
        rerank_semantic_candidates(&mut spaced, &nl_shape, "Engine Index");
        let spaced_results = fuse_hybrid_results(
            spaced,
            Vec::new(),
            &nl_shape,
            10,
            true,
            Path::new("/project"),
        );
        assert_eq!(spaced_results[0].name, "Index");

        let mut dotted = candidates;
        rerank_semantic_candidates(&mut dotted, &id_shape, "Engine.Index");
        let dotted_results = fuse_hybrid_results(
            dotted,
            Vec::new(),
            &id_shape,
            10,
            true,
            Path::new("/project"),
        );
        assert_eq!(dotted_results[0].name, "Index");
    }

    #[test]
    fn identifier_engine_factory_and_non_name_shapes_leave_priors_inert() {
        let identifier_shape = rerank_shape(QueryKind::Identifier);
        let mut identifier_candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.80,
            ),
            semantic_candidate(
                "/project/src/engine.ts",
                "EngineFactory",
                Some("EngineFactory"),
                SymbolKind::Class,
                0.79,
            ),
        ];

        rerank_semantic_candidates(
            &mut identifier_candidates,
            &identifier_shape,
            "engineFactory",
        );
        assert_eq!(
            identifier_candidates[0].rank_score,
            identifier_candidates[0].score
        );
        assert_eq!(
            identifier_candidates[1].rank_score,
            identifier_candidates[1].score
        );
        assert!(!identifier_candidates
            .iter()
            .any(|result| result.cap_protected));
        let identifier_results = fuse_hybrid_results(
            identifier_candidates,
            Vec::new(),
            &identifier_shape,
            10,
            true,
            Path::new("/project"),
        );
        assert_eq!(identifier_results[0].name, "engineFactory");

        for kind in [QueryKind::Path, QueryKind::Regex, QueryKind::ErrorCode] {
            let shape = rerank_shape(kind);
            let mut candidates = vec![semantic_candidate(
                "/project/src/engine.ts",
                "EngineFactory",
                Some("EngineFactory"),
                SymbolKind::Class,
                0.79,
            )];
            rerank_semantic_candidates(&mut candidates, &shape, "EngineFactory");
            assert_eq!(candidates[0].rank_score, candidates[0].score);
            assert!(!candidates[0].cap_protected);
        }
    }

    #[test]
    fn exact_name_boost_does_not_cap_protect_near_zero_common_names() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut candidates = vec![semantic_candidate(
            "/project/src/list.ts",
            "List",
            Some("List"),
            SymbolKind::Class,
            0.01,
        )];

        rerank_semantic_candidates(&mut candidates, &shape, "List");

        assert!((candidates[0].rank_score - 0.012).abs() < 0.0001);
        assert!(!candidates[0].cap_protected);
    }

    #[test]
    fn churned_borrowed_tree_query_completes_with_budget_degradation() {
        let session = tempfile::tempdir().expect("session root");
        let external = tempfile::tempdir().expect("external root");
        let external_root =
            std::fs::canonicalize(external.path()).expect("canonical external root");
        let storage = tempfile::tempdir().expect("storage");
        for file_index in 0..64 {
            let file = external_root.join(format!(
                "packages/pkg_{}/src/module_{file_index}.rs",
                file_index % 8
            ));
            std::fs::create_dir_all(file.parent().expect("fixture parent"))
                .expect("create nested fixture directory");
            std::fs::write(
                file,
                format!("pub fn borrowed_tree_needle_{file_index}() {{}}\n"),
            )
            .expect("write borrowed fixture");
        }
        let cache_dir =
            crate::search_index::resolve_cache_dir(&external_root, Some(storage.path()));
        let mut index = SearchIndex::build(&external_root);
        index.write_to_disk(&cache_dir, None);

        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(session.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        let req = semantic_request("borrowed_tree_needle", 10);
        let shape = query_shape::classify("borrowed_tree_needle");
        let first = crate::readonly_artifacts::with_borrowed_search_load_limits_for_test(
            10_000,
            Duration::from_secs(5),
            || {
                response_value(handle_external_search(
                    &req,
                    &ctx,
                    SemanticSearchParams {
                        query: "borrowed_tree_needle".to_string(),
                        top_k: 10,
                        include_tests: false,
                        path: None,
                    },
                    10,
                    shape.clone(),
                    external_root.clone(),
                ))
            },
        );
        assert_eq!(first["success"], true, "initial borrow failed: {first:?}");

        for file_index in 0..40 {
            let file = external_root.join(format!(
                "packages/pkg_{}/src/module_{file_index}.rs",
                file_index % 8
            ));
            std::fs::write(
                file,
                format!("pub fn churned_borrowed_tree_{file_index}() {{}}\n"),
            )
            .expect("churn borrowed fixture");
        }
        let mut rebuilt = SearchIndex::build(&external_root);
        rebuilt.write_to_disk(&cache_dir, None);

        let started = Instant::now();
        let degraded = crate::readonly_artifacts::with_borrowed_search_load_limits_for_test(
            10,
            Duration::from_secs(5),
            || {
                response_value(handle_external_search(
                    &req,
                    &ctx,
                    SemanticSearchParams {
                        query: "borrowed_tree_needle".to_string(),
                        top_k: 10,
                        include_tests: false,
                        path: None,
                    },
                    10,
                    shape,
                    external_root,
                ))
            },
        );

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "churned generation must stop at the borrowed-load budget"
        );
        assert_eq!(degraded["success"], true);
        assert_eq!(degraded["complete"], false);
        assert_eq!(degraded["fully_degraded"], true);
        assert_eq!(
            degraded["borrowed_index_degraded_reason"],
            "borrowed_search_index_load_budget"
        );
        assert!(degraded["text"]
            .as_str()
            .expect("degraded response text")
            .ends_with(BORROWED_SEARCH_LOAD_FOOTER));
    }

    #[test]
    fn borrowed_load_budget_degradation_has_locked_disclosure() {
        let session = tempfile::tempdir().expect("session root");
        let external = tempfile::tempdir().expect("external root");
        std::fs::write(
            external.path().join("fixture.rs"),
            "pub fn budget_disclosure_needle() {}\n",
        )
        .expect("write external fixture");
        let ctx = test_context(session.path());
        let req = semantic_request("budget_disclosure_needle", 10);
        let params = SemanticSearchParams {
            query: "budget_disclosure_needle".to_string(),
            top_k: 10,
            include_tests: false,
            path: Some(external.path().display().to_string()),
        };
        let shape = query_shape::classify(&params.query);

        let response = response_value(handle_external_borrowed_degraded_fallback(
            &req,
            &ctx,
            &params,
            10,
            &shape,
            external.path(),
            crate::readonly_artifacts::BORROWED_SEARCH_LOAD_DEGRADATION,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["complete"], false);
        assert_eq!(response["fully_degraded"], true);
        assert_eq!(
            response["borrowed_index_degraded_reason"],
            "borrowed_search_index_load_budget"
        );
        assert!(response["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str() == Some(BORROWED_SEARCH_LOAD_WARNING)));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .ends_with(BORROWED_SEARCH_LOAD_FOOTER));
    }

    #[test]
    fn borrowed_drift_remains_available_in_logs() {
        let message = borrowed_drift_log_message("semantic", Path::new("/borrowed"), 3);
        assert!(message.contains("borrowed semantic index"));
        assert!(message.contains("3 drifted file(s)"));
    }

    #[test]
    fn file_summary_snippet_is_regenerated_from_current_disk_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.rs");
        std::fs::write(&path, "pub fn current_disk_symbol() {}\n").expect("write current source");
        let mut candidate = semantic_candidate(
            path.to_str().expect("utf8 path"),
            "summary.rs",
            None,
            SymbolKind::FileSummary,
            0.9,
        );
        candidate.snippet = "pub fn stale_persisted_symbol() {}".to_string();
        let shape = rerank_shape(QueryKind::NaturalLanguage);
        let mut results =
            fuse_hybrid_results(vec![candidate], Vec::new(), &shape, 5, true, dir.path());

        enrich_snippets_from_source(&mut results, dir.path());

        assert!(results[0].snippet.contains("current_disk_symbol"));
        assert!(!results[0].snippet.contains("stale_persisted_symbol"));
    }

    #[test]
    fn file_summary_keeps_persisted_snippet_when_source_is_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.rs");
        let mut candidate = semantic_candidate(
            missing.to_str().expect("utf8 path"),
            "missing.rs",
            None,
            SymbolKind::FileSummary,
            0.9,
        );
        candidate.snippet = "persisted fallback".to_string();
        let shape = rerank_shape(QueryKind::NaturalLanguage);
        let mut results =
            fuse_hybrid_results(vec![candidate], Vec::new(), &shape, 5, true, dir.path());

        enrich_snippets_from_source(&mut results, dir.path());

        assert_eq!(results[0].snippet, "persisted fallback");
    }

    #[test]
    fn boosted_rank_score_keeps_raw_cosine_honesty_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("allocation.rs");
        let body = (0..30)
            .map(|index| format!("line{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).expect("write symbol body");
        let shape = rerank_shape(QueryKind::Identifier);
        let mut candidates = vec![SemanticResult {
            file: path,
            name: "AllocationService".to_string(),
            qualified_name: Some("AllocationService".to_string()),
            kind: SymbolKind::Class,
            start_line: 0,
            end_line: 29,
            exported: false,
            snippet: String::new(),
            score: HIGH_CONFIDENCE_COSINE_FLOOR - 0.01,
            rank_score: HIGH_CONFIDENCE_COSINE_FLOOR - 0.01,
            cap_protected: false,
            source: "semantic",
        }];

        rerank_semantic_candidates(&mut candidates, &shape, "AllocationService");
        assert!(candidates[0].rank_score > HIGH_CONFIDENCE_COSINE_FLOOR);
        let mut fused = fuse_hybrid_results(candidates, Vec::new(), &shape, 10, true, dir.path());
        assert_eq!(
            fused[0].semantic_score,
            Some(HIGH_CONFIDENCE_COSINE_FLOOR - 0.01)
        );

        let incomplete = enrich_snippets_from_source(&mut fused, dir.path());
        assert!(incomplete);
        assert!(fused[0].snippet.contains("line19"));
        assert!(!fused[0].snippet.contains("line29"));
        assert!(!fused[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE));

        let mut weak = vec![semantic_candidate(
            "/project/src/weak.ts",
            "WeakService",
            Some("WeakService"),
            SymbolKind::Class,
            WEAK_MATCH_COSINE_FLOOR - 0.01,
        )];
        rerank_semantic_candidates(&mut weak, &shape, "WeakService");
        let weak_results =
            fuse_hybrid_results(weak, Vec::new(), &shape, 10, true, Path::new("/project"));
        assert!(weak_results[0].score > WEAK_MATCH_COSINE_FLOOR);
        let text = format_semantic_text(&weak_results, Path::new("/project"), false, false, None);
        assert!(text.contains("Top match is weak"), "got: {text}");
    }

    #[test]
    fn empty_semantic_index_skips_query_dimension_check() {
        let project = tempfile::tempdir().expect("create project dir");
        let (base_url, handle) = start_mock_embedding_server();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic: SemanticBackendConfig {
                    backend: SemanticBackend::OpenAiCompatible,
                    model: "test-embedding".to_string(),
                    base_url: Some(base_url),
                    api_key_env: None,
                    timeout_ms: 5_000,
                    query_timeout_ms: 3_000,
                    max_batch_size: 64,
                    max_files: 20_000,
                    ..Default::default()
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(project.path().to_path_buf(), 384));

        let response = response_value(handle_semantic_search(
            &semantic_request("anything", 5),
            &ctx,
        ));

        assert_eq!(
            response["success"], true,
            "response should not fail: {response:?}"
        );
        assert_eq!(response["status"], "ready");
        assert_eq!(response["semantic_status"], "ready");
        assert!(response["results"].as_array().expect("results").is_empty());
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn file_summary_text_uses_summary_location_instead_of_line_range() {
        let project_root = Path::new("/project");
        let results = vec![HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }];

        let text = format_semantic_text(&results, project_root, false, false, None);

        // File-summary rows show "[file summary]" with no line range, and no
        // longer leak the internal score/source.
        assert!(text.contains("index [file summary]"));
        assert!(!text.contains("lines 1-1"));
        assert!(!text.contains("score"));
        assert!(!text.contains("source semantic"));
    }

    /// A symbol hit whose `file` points at a real on-disk file with `body_lines`
    /// lines starting at line 0, so enrich_snippets_from_source can read it. The
    /// stored `snippet` is left empty on purpose — enrichment fills it from disk.
    fn write_symbol_hit(
        dir: &Path,
        file_name: &str,
        name: &str,
        body_lines: usize,
    ) -> HybridResult {
        let path = dir.join(file_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create symbol parent");
        }
        let body = (0..body_lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).expect("write symbol file");
        snippet_hit(
            path,
            name,
            SymbolKind::Function,
            0,
            body_lines.saturating_sub(1) as u32,
            0.5,
        )
    }

    fn snippet_hit(
        file: PathBuf,
        name: &str,
        kind: SymbolKind,
        start_line: u32,
        end_line: u32,
        score: f32,
    ) -> HybridResult {
        HybridResult {
            file,
            name: name.to_string(),
            kind,
            start_line,
            end_line,
            exported: false,
            snippet: String::new(),
            score,
            source: "semantic",
            semantic_score: Some(score),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }
    }

    #[test]
    fn bounded_snippet_enrichment_matches_full_read_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mid.rs");
        std::fs::write(
            &path,
            "preamble one\npreamble two\n\n/// Explains target.\n#[inline]\nfn target() {\n    work();\n}\nmarker after required range\n",
        )
        .expect("write fixture");
        let hit = snippet_hit(
            path,
            "target",
            SymbolKind::Function,
            5,
            7,
            HIGH_CONFIDENCE_COSINE_FLOOR,
        );
        let mut bounded = vec![hit.clone()];
        let mut reference = vec![hit];

        let bounded_incomplete = enrich_snippets_from_source(&mut bounded, dir.path());
        let reference_incomplete =
            enrich_snippets_from_source_reference(&mut reference, dir.path(), None);

        assert_eq!(bounded[0].snippet, reference[0].snippet);
        assert_eq!(bounded_incomplete, reference_incomplete);
        assert!(bounded[0].snippet.contains("Explains target"));
        assert!(bounded[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE));
    }

    #[test]
    fn bounded_snippet_reader_stops_before_later_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bounded.rs");
        std::fs::write(&path, "line0\nline1\nline2\nmarker-must-not-be-read\n")
            .expect("write fixture");
        let plan = SnippetReadPlan {
            fixed_last_line: Some(2),
            summary_nonempty_lines: 0,
        };

        let lines = read_bounded_snippet_lines(&path, plan).expect("bounded read");

        assert_eq!(lines, vec!["line0", "line1", "line2"]);
    }

    #[test]
    fn bounded_snippet_reader_ignores_invalid_utf8_only_beyond_required_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("invalid-tail.rs");
        std::fs::write(&path, b"line0\nline1\n\xff\n").expect("write fixture");

        let mut valid_prefix = vec![snippet_hit(
            path.clone(),
            "prefix",
            SymbolKind::Function,
            0,
            1,
            0.5,
        )];
        let valid_incomplete = enrich_snippets_from_source(&mut valid_prefix, dir.path());
        assert_eq!(valid_prefix[0].snippet, "line0\nline1");
        assert!(!valid_incomplete);

        let mut invalid_range = vec![snippet_hit(
            path,
            "invalid",
            SymbolKind::Function,
            0,
            2,
            0.5,
        )];
        let invalid_incomplete = enrich_snippets_from_source(&mut invalid_range, dir.path());
        assert!(invalid_range[0].snippet.is_empty());
        assert!(!invalid_incomplete);
    }

    #[test]
    fn file_summary_and_symbol_share_one_bounded_file_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.rs");
        std::fs::write(
            &path,
            "\nsummary one\n\nfn target() {\n}\nsummary two\nsummary three\nmarker after requirements\n",
        )
        .expect("write fixture");
        let symbol = snippet_hit(path.clone(), "target", SymbolKind::Function, 3, 4, 0.5);
        let mut summary = snippet_hit(path, "shared.rs", SymbolKind::FileSummary, 0, 0, 0.5);
        summary.snippet = "persisted summary".to_string();
        let original = vec![symbol, summary];
        let (plans, _) = snippet_read_plans(&original, dir.path(), None);
        assert_eq!(plans.len(), 1, "same-file hits must share one read plan");

        let mut bounded = original.clone();
        let mut reference = original;
        let bounded_incomplete = enrich_snippets_from_source(&mut bounded, dir.path());
        let reference_incomplete =
            enrich_snippets_from_source_reference(&mut reference, dir.path(), None);

        assert_eq!(bounded[0].snippet, reference[0].snippet);
        assert_eq!(bounded[1].snippet, reference[1].snippet);
        assert_eq!(bounded_incomplete, reference_incomplete);
    }

    #[test]
    fn rows_omit_score_and_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 2)];
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);
        assert!(text.contains("foo [function] lines 1-2"));
        assert!(!text.contains("score"));
        assert!(!text.contains("source"));
    }

    #[test]
    fn snippets_are_rank_tiered_top_three_only_from_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Five hits, each a 30-line body, in distinct files so grouping does not
        // merge them. Rank order = vector order (already sorted). Budgets:
        // rank 0 = 20 lines (+10 more lines), ranks 1-2 = 5 lines (+25 more
        // lines), rank 3+ = header only.
        let mut results: Vec<HybridResult> = (0..5)
            .map(|i| write_symbol_hit(dir.path(), &format!("f{i}.rs"), &format!("fn{i}"), 30))
            .collect();
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        assert!(incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);

        assert!(text.contains("fn0 [function]"));
        // "lines" wording is load-bearing (vs "+N more" reading as results).
        assert!(text.contains("+10 more lines"));
        assert!(text.contains("+25 more lines"));
        // Rank 0 genuinely shows MORE than ranks 1-2 (gradient not inverted).
        let body_lines =
            |r: &HybridResult| r.snippet.lines().filter(|l| l.starts_with("line")).count();
        assert_eq!(body_lines(&results[0]), 20);
        assert_eq!(body_lines(&results[1]), 5);
        // Ranks 3,4 → header only, no body lines.
        assert!(
            results[3].snippet.is_empty(),
            "rank 4+ must have no snippet"
        );
        assert!(
            results[4].snippet.is_empty(),
            "rank 4+ must have no snippet"
        );
        // Zoom hint present because snippets were withheld.
        assert!(text.contains("aft_zoom <file> <symbol>"));
    }

    #[test]
    fn high_confidence_rank0_expands_full_symbol_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "full.rs", "full", 30)];
        results[0].semantic_score = Some(HIGH_CONFIDENCE_COSINE_FLOOR);
        results[0].score = HIGH_CONFIDENCE_COSINE_FLOOR;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(
            !incomplete,
            "full rank-0 symbol should not need a zoom hint"
        );
        assert!(results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE));
        assert!(!results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn subfloor_rank0_keeps_preview_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "preview.rs", "preview", 30)];
        results[0].semantic_score = Some(HIGH_CONFIDENCE_COSINE_FLOOR - 0.01);
        results[0].score = HIGH_CONFIDENCE_COSINE_FLOOR - 0.01;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(results[0].snippet.contains("line19"));
        assert!(!results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn test_support_rank0_never_full_expands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(
            dir.path(),
            "fixtures/full.rs",
            "fixture",
            30,
        )];
        results[0].semantic_score = Some(0.99);
        results[0].score = 0.99;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(!results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn rank0_large_container_renders_member_menu_without_full_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large.ts");
        let mut content = String::from(
            "class BigContainer {\n  methodOne(): number {\n    const visibleMethodBodyLine = 1;\n",
        );
        for i in 0..155 {
            content.push_str(&format!("    const filler{i} = {i};\n"));
        }
        content.push_str(
            "    return visibleMethodBodyLine;\n  }\n\n  methodTwo(): void {\n    console.log(\"second\");\n  }\n}\n",
        );
        std::fs::write(&path, content).expect("write large class");
        let ctx = test_context(dir.path());
        let symbols = ctx.provider().list_symbols(&path).expect("list symbols");
        let target = symbols
            .iter()
            .find(|symbol| symbol.name == "BigContainer")
            .expect("BigContainer symbol");
        let mut results = vec![HybridResult {
            file: path,
            name: "BigContainer".to_string(),
            kind: SymbolKind::Class,
            start_line: target.range.start_line,
            end_line: target.range.end_line,
            exported: false,
            snippet: String::new(),
            score: 0.99,
            source: "semantic",
            semantic_score: Some(0.99),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }];

        let incomplete =
            enrich_snippets_from_source_with_context(&mut results, dir.path(), Some(&ctx));
        let snippet = &results[0].snippet;

        assert!(incomplete, "member menu is not a complete body");
        assert!(
            snippet.contains("member-signature menu; zoom a member for its body"),
            "large container should render a member menu: {snippet}"
        );
        assert!(
            snippet.contains("BigContainer.methodOne(): number"),
            "menu should include qualified method signatures: {snippet}"
        );
        assert!(
            !snippet.contains("visibleMethodBodyLine"),
            "menu must not include the class body: {snippet}"
        );
        assert!(
            !snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "member menu must not claim the full symbol was shown: {snippet}"
        );
    }

    #[test]
    fn oversized_rank0_full_expansion_renders_budgeted_head_slice() {
        // Use a 300-line symbol so the top result is truncated to a head slice,
        // setting incomplete=true instead of using the small default preview.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "huge.rs", "huge", 300)];
        results[0].semantic_score = Some(0.99);
        results[0].score = 0.99;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(results[0].snippet.contains("line249"));
        assert!(!results[0].snippet.contains("line299"));
        assert!(results[0]
            .snippet
            .contains("… +50 more lines — zoom huge for the full body"));
        assert!(
            !results[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "capped fallback is incomplete — must NOT claim no-re-read"
        );
    }

    #[test]
    fn rank0_expansion_includes_leading_doc_and_excludes_trailing_neighbor() {
        // A symbol preceded by a doc comment and a decorator, with a NEXT symbol
        // immediately after. Rank-0 expansion must show the doc + the symbol, and
        // must NOT bleed the following symbol in.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doc.ts");
        let content = "import x from 'y';\n\
                       \n\
                       /** Does the thing. */\n\
                       @decorator\n\
                       export function target() {\n\
                       \x20\x20return 1;\n\
                       }\n\
                       \n\
                       export function nextSymbol() {\n\
                       \x20\x20return 2;\n\
                       }\n";
        std::fs::write(&path, content).expect("write");
        // target() body spans the `export function target` line (index 4) through
        // its closing brace (index 6), 0-based inclusive.
        let mut results = vec![HybridResult {
            file: path,
            name: "target".to_string(),
            kind: SymbolKind::Function,
            start_line: 4,
            end_line: 6,
            exported: true,
            snippet: String::new(),
            score: 0.99,
            source: "semantic",
            semantic_score: Some(0.99),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }];

        enrich_snippets_from_source(&mut results, dir.path());
        let snippet = &results[0].snippet;

        assert!(
            snippet.contains("Does the thing."),
            "leading doc comment must be included: {snippet}"
        );
        assert!(
            snippet.contains("@decorator"),
            "leading decorator must be included: {snippet}"
        );
        assert!(
            snippet.contains("export function target()"),
            "symbol signature must be present: {snippet}"
        );
        assert!(
            !snippet.contains("nextSymbol"),
            "trailing neighbor must NOT bleed in: {snippet}"
        );
        assert!(
            snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "full expansion must carry the no-re-read notice: {snippet}"
        );
    }

    #[test]
    fn rank0_expansion_does_not_include_c_preprocessor_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.c");
        let content = "#include <x.h>
                       int target(void) {
                         return 0;
                       }
";
        std::fs::write(&path, content).expect("write");
        let mut results = vec![HybridResult {
            file: path,
            name: "target".to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 3,
            exported: false,
            snippet: String::new(),
            score: HIGH_CONFIDENCE_COSINE_FLOOR,
            source: "semantic",
            semantic_score: Some(HIGH_CONFIDENCE_COSINE_FLOOR),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }];

        enrich_snippets_from_source(&mut results, dir.path());
        let snippet = &results[0].snippet;

        assert!(
            snippet.contains("int target(void)"),
            "symbol signature must be present: {snippet}"
        );
        assert!(
            !snippet.contains("#include <x.h>"),
            "C preprocessor directives must not be treated as symbol docs: {snippet}"
        );
        assert!(
            snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "full expansion must carry the no-re-read notice: {snippet}"
        );
    }

    #[test]
    fn weak_top_match_emits_low_confidence_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hit = write_symbol_hit(dir.path(), "a.rs", "foo", 2);
        // Top semantic cosine below the weak floor.
        hit.semantic_score = Some(0.22);
        hit.score = 0.22;
        let results = vec![hit];
        let text = format_semantic_text(&results, dir.path(), false, false, None);
        assert!(
            text.contains("Top match is weak"),
            "expected weak-match note, got: {text}"
        );
    }

    #[test]
    fn strong_top_match_has_no_low_confidence_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hit = write_symbol_hit(dir.path(), "a.rs", "foo", 2);
        hit.semantic_score = Some(0.72);
        hit.score = 0.72;
        let results = vec![hit];
        let text = format_semantic_text(&results, dir.path(), false, false, None);
        assert!(!text.contains("Top match is weak"), "got: {text}");
        // And no unconditional "[index: ready]" tax on the happy path.
        assert!(!text.contains("[index: ready]"), "got: {text}");
    }

    #[test]
    fn no_zoom_hint_when_all_snippets_fit() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two small symbols (3 lines each), both within their rank budget.
        let mut results = vec![
            write_symbol_hit(dir.path(), "a.rs", "foo", 3),
            write_symbol_hit(dir.path(), "b.rs", "bar", 3),
        ];
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        assert!(!incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);
        assert!(!text.contains("+"), "no truncation marker expected: {text}");
        assert!(!text.contains("aft_zoom"), "no zoom hint expected: {text}");
    }

    #[test]
    fn enrich_handles_missing_file_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![HybridResult {
            file: dir.path().join("does-not-exist.rs"),
            name: "ghost".to_string(),
            kind: SymbolKind::Function,
            start_line: 0,
            end_line: 9,
            exported: false,
            snippet: String::new(),
            score: 0.5,
            source: "semantic",
            semantic_score: Some(0.5),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        }];
        // Must not panic; header renders, no snippet body.
        let _ = enrich_snippets_from_source(&mut results, dir.path());
        assert!(results[0].snippet.is_empty());
        let text = format_result_sections(&results, dir.path());
        assert!(text.contains("ghost [function]"));
    }

    #[test]
    fn groups_render_in_rank_order_not_alphabetical() {
        let dir = tempfile::tempdir().expect("tempdir");
        // zzz.rs holds the top hit, aaa.rs the second. Alphabetical grouping
        // (the old BTreeMap bug) would put aaa.rs first; rank order keeps zzz.
        let results = vec![
            write_symbol_hit(dir.path(), "zzz.rs", "top", 1),
            write_symbol_hit(dir.path(), "aaa.rs", "second", 1),
        ];
        let text = format_result_sections(&results, dir.path());
        let zzz_at = text.find("zzz.rs").expect("zzz present");
        let aaa_at = text.find("aaa.rs").expect("aaa present");
        assert!(zzz_at < aaa_at, "top-ranked file must render first: {text}");
    }

    #[test]
    fn warm_callgraph_adds_compact_blast_radius_suffixes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        let fixture_dir = dir.path().join("fixtures");
        std::fs::create_dir_all(&src_dir).expect("create src");
        std::fs::create_dir_all(&fixture_dir).expect("create fixtures");
        let target_file = src_dir.join("target.ts");
        std::fs::write(
            &target_file,
            "export function covered() {\n  return 1;\n}\nexport function untested() {\n  return 2;\n}\n",
        )
        .expect("write target");
        std::fs::write(
            src_dir.join("app.ts"),
            "import { covered, untested } from './target';\nexport function callerOne() {\n  return covered() + untested();\n}\nexport function callerTwo() {\n  return covered();\n}\n",
        )
        .expect("write app");
        std::fs::write(
            fixture_dir.join("covered_fixture.ts"),
            "import { covered } from '../src/target';\nexport function fixtureCaller() {\n  return covered();\n}\n",
        )
        .expect("write fixture");
        let ctx = test_context(dir.path());
        install_warm_callgraph_store(&ctx, dir.path());

        let results = vec![
            HybridResult {
                file: target_file.clone(),
                name: "covered".to_string(),
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 2,
                exported: true,
                snippet: String::new(),
                score: 0.8,
                source: "semantic",
                semantic_score: Some(0.8),
                lexical_score: None,
                hybrid_boosted: false,
                cap_protected: false,
                lexical_generated_artifact: false,
            },
            HybridResult {
                file: target_file,
                name: "untested".to_string(),
                kind: SymbolKind::Function,
                start_line: 3,
                end_line: 5,
                exported: true,
                snippet: String::new(),
                score: 0.7,
                source: "semantic",
                semantic_score: Some(0.7),
                lexical_score: None,
                hybrid_boosted: false,
                cap_protected: false,
                lexical_generated_artifact: false,
            },
        ];

        let text = format_semantic_text(&results, dir.path(), false, false, Some(&ctx));
        let covered_line = text
            .lines()
            .find(|line| line.contains("covered [function]"))
            .expect("covered row");
        assert!(
            covered_line.contains("↩"),
            "covered row should show callers: {text}"
        );
        assert!(
            covered_line.contains("app.ts") || covered_line.contains("covered_fixture.ts"),
            "covered row should include caller basenames: {covered_line}"
        );

        let untested_line = text
            .lines()
            .find(|line| line.contains("untested [function]"))
            .expect("untested row");
        assert!(
            untested_line.contains("↩"),
            "untested row should show callers: {text}"
        );
        assert!(
            untested_line.contains("app.ts"),
            "untested caller basename missing: {untested_line}"
        );

        // The `⚠untested` marker was removed: it provided no actionable signal in
        // a discovery tool and relied on is_test_support_file (fixtures/mocks
        // only), so it false-flagged genuinely tested code. Blast radius keeps
        // only the accurate `↩callers` + basenames.
        assert!(
            !text.contains("⚠untested"),
            "untested marker must no longer appear in search output: {text}"
        );
    }

    #[test]
    fn absent_warm_callgraph_emits_no_blast_radius_and_starts_no_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_context(dir.path());
        reset_callgraph_cold_build_spawn_count_for_test();
        let results = vec![write_symbol_hit(dir.path(), "target.rs", "target", 1)];

        let text = format_semantic_text(&results, dir.path(), false, false, Some(&ctx));

        assert!(
            !text.contains("↩"),
            "cold store should not annotate rows: {text}"
        );
        assert_eq!(callgraph_cold_build_spawn_count_for_test(), 0);
        assert!(ctx.callgraph_store_rx().lock().is_none());
    }

    #[test]
    fn more_available_appends_raise_topk_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 1)];
        let text = format_semantic_text(&results, dir.path(), true, false, None);
        assert!(text.contains("More results available; raise topK to see more."));
    }

    #[test]
    fn file_summary_json_uses_summary_location_instead_of_line_numbers() {
        let result = HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
            hybrid_boosted: false,
            cap_protected: false,
            lexical_generated_artifact: false,
        };

        let json = result_to_json(&result);

        assert_eq!(json["kind"], "file_summary");
        assert_eq!(json["location"], "[file summary]");
        assert!(json["start_line"].is_null());
        assert!(json["end_line"].is_null());
        assert_eq!(json["source"], "semantic");
        assert_eq!(json["semantic_score"], 0.75);
        assert!(json["lexical_score"].is_null());
    }

    #[test]
    fn ready_uncontended_search_keeps_indexed_literal_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn parity_marker() {}\n").expect("write source");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, b"pub fn parity_marker() {}\n");
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("parity_marker", 5, "literal"),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "lexical");
        assert_eq!(response["results"][0]["source"], "lexical");
        assert!(response["results"][0]["file"]
            .as_str()
            .expect("result file")
            .ends_with("src/lib.rs"));
    }

    #[test]
    fn contended_search_index_degrades_with_disclosure() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn contention_marker() {}\n").expect("write source");
        let ctx = test_context(project.path());
        let writer_guard = ctx
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let started = std::time::Instant::now();
        let response = response_value(handle_semantic_search(
            &semantic_request("contention_marker", 5),
            &ctx,
        ));
        drop(writer_guard);

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "contended search exceeded bounded response time"
        );
        assert_eq!(response["success"], true);
        assert_eq!(response["lexical_only_fallback"], true);
        assert!(response["text"]
            .as_str()
            .expect("fallback text")
            .contains("artifact contention"));
    }

    #[test]
    fn contended_callgraph_receiver_does_not_block_search_formatting() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());
        let receiver_guard = ctx.callgraph_store_rx().lock();
        let started = std::time::Instant::now();
        assert!(warm_callgraph_store(&ctx).is_none());
        assert!(
            started.elapsed() < INTERACTIVE_ARTIFACT_READ_BUDGET,
            "callgraph receiver contention exceeded artifact budget"
        );
        drop(receiver_guard);
    }
}
