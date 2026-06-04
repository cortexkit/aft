use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::context::{AppContext, SemanticIndexStatus};
use crate::grep_executor::{self, GrepParams};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::query_shape::{self, QueryKind, QueryShape};
use crate::search_index::{
    sort_grep_matches_by_mtime_desc, GrepMatch, GrepResult, IndexStatus, SearchIndex,
};
use crate::semantic_index::{is_onnx_runtime_unavailable, EmbeddingModel, SemanticResult};
use crate::symbols::SymbolKind;

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;
const HYBRID_LEXICAL_BOOST: f32 = 1.1;
const LEXICAL_ONLY_SCORE_CEILING: f32 = 0.25;
const LEXICAL_ENUMERATION_LIMIT: usize = 50;
const SEMANTIC_OVERFETCH_MULTIPLIER: usize = 3;
const SEMANTIC_OVERFETCH_FLOOR: usize = 10;
const DEGRADED_GREP_FILE_LIMIT: usize = 5_000;
const DEGRADED_GREP_RESULT_LIMIT: usize = 100;

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
    pub snippet: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SearchHint {
    Regex,
    Literal,
    Semantic,
    #[default]
    Auto,
}

#[derive(Debug, Deserialize)]
struct SemanticSearchParams {
    query: String,
    #[serde(default = "default_top_k", alias = "topK")]
    top_k: usize,
    #[serde(default)]
    hint: SearchHint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Regex,
    Literal,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone)]
struct LexicalCollection {
    files: Vec<(PathBuf, f32)>,
    ready: bool,
    engine_capped: bool,
}

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
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
    let semantic_status_snapshot = ctx.semantic_index_status().borrow().clone();
    let semantic_status = semantic_status_label(&semantic_status_snapshot);
    let mut warnings = Vec::new();

    let lexical_ready = search_index_ready(ctx);
    let mode = choose_mode(
        params.hint,
        &params.query,
        &shape,
        lexical_ready,
        &mut warnings,
    );

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
        ),
        SearchMode::Semantic | SearchMode::Hybrid => handle_semantic_or_hybrid_search(
            req,
            ctx,
            params,
            top_k,
            shape,
            mode,
            semantic_status_snapshot,
            semantic_status,
            warnings,
            &project_root,
        ),
    }
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

fn semantic_candidate_limit(top_k: usize) -> usize {
    top_k
        .saturating_mul(SEMANTIC_OVERFETCH_MULTIPLIER)
        .clamp(SEMANTIC_OVERFETCH_FLOOR, MAX_TOP_K)
}

fn choose_mode(
    hint: SearchHint,
    query: &str,
    shape: &QueryShape,
    lexical_ready: bool,
    warnings: &mut Vec<String>,
) -> SearchMode {
    match hint {
        SearchHint::Regex => {
            if shape.kind == QueryKind::NaturalLanguage {
                warnings.push(
                    "hint:'regex' was provided for a natural-language-looking query; interpreting it as regex.".to_string(),
                );
            }
            SearchMode::Regex
        }
        SearchHint::Literal => {
            if literal_tokens_all_short(query) {
                warnings.push(
                    "Literal query with tokens shorter than 3 chars requires per-file scan; latency may be slow on large repos.".to_string(),
                );
            }
            SearchMode::Literal
        }
        SearchHint::Semantic => {
            if shape.kind == QueryKind::Regex {
                warnings.push(
                    "hint:'semantic' was provided for a regex-looking query; skipping lexical/regex matching.".to_string(),
                );
            }
            SearchMode::Semantic
        }
        SearchHint::Auto => {
            if shape.kind == QueryKind::Regex {
                return SearchMode::Regex;
            }
            if shape.kind != QueryKind::NaturalLanguage && extracted_tokens_all_short(query, shape)
            {
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
                warnings.push(
                    "Lexical trigram index is unavailable; using semantic search only.".to_string(),
                );
                SearchMode::Semantic
            }
        }
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
) -> Response {
    let literal = mode == SearchMode::Literal;
    let compiled = match pattern_compile::compile(
        query,
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
                serde_json::json!({"pattern": query}),
            );
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Use hint:'literal' or rewrite without {feature}."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            );
        }
    };

    let scope = match grep_executor::resolve_grep_scope(ctx, None, top_k, &req.id) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let params = GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: top_k,
    };
    let result = grep_executor::execute(ctx, &compiled, &scope, &params);
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }

    let result_source = if literal { "literal" } else { "regex" };
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let interpreted_as = interpreted_as_label(mode);
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

fn handle_semantic_or_hybrid_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    top_k: usize,
    shape: QueryShape,
    mode: SearchMode,
    status: SemanticIndexStatus,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
) -> Response {
    let lexical = if mode == SearchMode::Hybrid {
        collect_lexical_files(ctx, &params.query, &shape)
    } else {
        LexicalCollection {
            files: Vec::new(),
            ready: search_index_ready(ctx),
            engine_capped: false,
        }
    };

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
                lexical,
                warnings,
                project_root,
                top_k,
            );
        }
        SemanticIndexStatus::Failed(error) => {
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "unavailable",
                "unavailable",
                format!("Semantic search unavailable: {error}"),
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
            ..
        } => {
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

            if natural_language_degraded_fallback_available(params.hint, mode, &shape) {
                return semantic_unavailable_grep_fallback_response(
                    req,
                    ctx,
                    &params,
                    &shape,
                    "building",
                    detail,
                    warnings,
                    project_root,
                    top_k,
                );
            }

            let lexical_count = lexical.files.len();
            let lexical_engine_capped = lexical.engine_capped;
            let results = fuse_hybrid_results(Vec::new(), lexical.files, &shape, top_k);
            let result_values = results.iter().map(result_to_json).collect::<Vec<_>>();
            let note = building_lexical_note(lexical.ready);
            let mut extras = serde_json::Map::new();
            extras.insert("stage".to_string(), serde_json::json!(stage));
            extras.insert("files".to_string(), serde_json::json!(files));
            extras.insert("entries_done".to_string(), serde_json::json!(entries_done));
            extras.insert(
                "entries_total".to_string(),
                serde_json::json!(entries_total),
            );
            extras.insert("note".to_string(), serde_json::json!(note));
            extras.insert("semantic_rebuilding".to_string(), serde_json::json!(true));
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
        SemanticIndexStatus::Ready { refreshing } => {
            if !refreshing.is_empty() {
                warnings.push(format!(
                    "{} file(s) refreshing; results for those files may be temporarily missing",
                    refreshing.len()
                ));
            }
        }
    }

    if !semantic_index_loaded(ctx) {
        return semantic_unavailable_or_fallback_response(
            req,
            ctx,
            &params,
            mode,
            &shape,
            "unavailable",
            "not_ready",
            "Semantic index is not ready yet.".to_string(),
            lexical,
            warnings,
            project_root,
            top_k,
        );
    }

    let query_vector = match embed_query(&params.query, ctx) {
        Ok(query_vector) => query_vector,
        Err(error) => {
            if params.hint == SearchHint::Semantic
                || !semantic_degraded_fallback_available(&params, mode, &shape, &lexical)
            {
                return semantic_error_response(&req.id, &error);
            }

            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "unavailable",
                "unavailable",
                format!("Semantic search unavailable: {error}"),
                lexical,
                warnings,
                project_root,
                top_k,
            );
        }
    };

    let semantic_limit = semantic_candidate_limit(top_k);
    let semantic_fetch_limit = semantic_limit.saturating_add(1);
    let mut semantic_results = {
        let semantic_index = ctx.semantic_index().borrow();
        semantic_index
            .as_ref()
            .map(|index| index.search(&query_vector, semantic_fetch_limit))
            .unwrap_or_default()
    };
    let semantic_more_available = semantic_results.len() > semantic_limit;
    if semantic_more_available {
        semantic_results.truncate(semantic_limit);
    }

    let mut results = fuse_hybrid_results(
        semantic_results,
        lexical.files,
        &shape,
        top_k.saturating_add(1),
    );
    let fused_more_available = results.len() > top_k;
    if fused_more_available {
        results.truncate(top_k);
    }
    let more_available = fused_more_available || semantic_more_available || lexical.engine_capped;

    // No score threshold: silent filtering produced "0 results" even when the
    // model had reasonable matches the agent could have judged. Surface every
    // hit so the caller can decide.

    // Read display snippets from source on the fly (top 3 only, rank-budgeted)
    // so both the text rendering and the JSON `results` carry fresh, correctly
    // sized previews. Drives the conditional zoom hint.
    let snippets_incomplete = enrich_snippets_from_source(&mut results);

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text: format_semantic_text(&results, project_root, more_available, snippets_incomplete),
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

fn search_response(req: &RawRequest, parts: SearchResponseParts<'_>) -> Response {
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

fn semantic_unavailable_or_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    mode: SearchMode,
    shape: &QueryShape,
    semantic_status: &'static str,
    unavailable_status: &'static str,
    detail: String,
    lexical: LexicalCollection,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
) -> Response {
    if params.hint == SearchHint::Semantic {
        return semantic_unavailable_response(&req.id, detail);
    }

    let lexical_ready = mode == SearchMode::Hybrid && lexical.ready;
    if lexical_ready {
        let lexical_count = lexical.files.len();
        let lexical_engine_capped = lexical.engine_capped;
        let results = fuse_hybrid_results(Vec::new(), lexical.files, shape, top_k);
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
                text: format_lexical_unavailable_text(&detail, &results, project_root),
                results: result_values,
                more_available: lexical_count > top_k || lexical_engine_capped,
                engine_capped: lexical_engine_capped,
                fully_degraded: false,
                warnings,
                extras: semantic_unavailable_extras(true),
            },
        );
    }

    if semantic_degraded_fallback_available(params, mode, shape, &lexical) {
        return semantic_unavailable_grep_fallback_response(
            req,
            ctx,
            params,
            shape,
            semantic_status,
            detail,
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

fn semantic_unavailable_response(request_id: &str, detail: String) -> Response {
    Response::error(request_id, "semantic_unavailable", detail)
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
    params: &SemanticSearchParams,
    mode: SearchMode,
    shape: &QueryShape,
    lexical: &LexicalCollection,
) -> bool {
    if natural_language_degraded_fallback_available(params.hint, mode, shape) {
        return true;
    }

    params.hint != SearchHint::Semantic
        && mode == SearchMode::Semantic
        && !lexical.ready
        && shape.weights.should_use_lexical
}

fn natural_language_degraded_fallback_available(
    hint: SearchHint,
    mode: SearchMode,
    shape: &QueryShape,
) -> bool {
    hint != SearchHint::Semantic
        && mode == SearchMode::Semantic
        && shape.kind == QueryKind::NaturalLanguage
}

fn semantic_unavailable_grep_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    semantic_status: &'static str,
    detail: String,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
) -> Response {
    let result = match execute_degraded_grep_fallback(&params.query, project_root, top_k, &req.id) {
        Ok(result) => result,
        Err(response) => return response,
    };
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }
    warnings
        .push("Semantic search unavailable; returning lexical-only fallback results.".to_string());

    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, "literal"))
        .collect::<Vec<_>>();
    let more_available = result.truncated || result.total_matches > result.matches.len();

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
            text: format_grep_lexical_unavailable_text(&detail, &result, project_root),
            results: result_values,
            more_available,
            engine_capped: result.engine_capped,
            fully_degraded: result.fully_degraded,
            warnings,
            extras: semantic_unavailable_extras(true),
        },
    )
}

fn execute_degraded_grep_fallback(
    query: &str,
    project_root: &Path,
    top_k: usize,
    request_id: &str,
) -> Result<GrepResult, Response> {
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
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Use hint:'literal' or rewrite without {feature}."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            ));
        }
    };

    let max_results = top_k.clamp(1, DEGRADED_GREP_RESULT_LIMIT);
    let (files, file_cap_reached) = collect_degraded_grep_files(project_root);
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_searched = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated = false;
    let mut engine_capped = file_cap_reached;

    for file in files {
        if truncated {
            engine_capped = true;
            break;
        }

        let Some(content) = crate::search_index::read_searchable_text(&file) else {
            continue;
        };
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

    Ok(GrepResult {
        matches,
        total_matches,
        files_searched,
        files_with_matches,
        index_status: IndexStatus::Fallback,
        truncated,
        fully_degraded: true,
        engine_capped,
    })
}

fn collect_degraded_grep_files(project_root: &Path) -> (Vec<PathBuf>, bool) {
    let walker = ignore::WalkBuilder::new(project_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry
                .file_type()
                .map_or(false, |file_type| file_type.is_dir())
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
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.filter_map(|entry| entry.ok()) {
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        if files.len() >= DEGRADED_GREP_FILE_LIMIT {
            return (files, true);
        }
        files.push(entry.into_path());
    }

    (files, false)
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

fn semantic_index_loaded(ctx: &AppContext) -> bool {
    ctx.semantic_index().borrow().is_some()
}

fn collect_lexical_files(ctx: &AppContext, query: &str, shape: &QueryShape) -> LexicalCollection {
    let search_index = ctx.search_index().borrow();
    let Some(index) = search_index.as_ref().filter(|index| index.ready) else {
        return LexicalCollection {
            files: Vec::new(),
            ready: false,
            engine_capped: false,
        };
    };

    // No `should_use_lexical` gate here: collect_lexical_files is only called
    // when choose_mode picked Hybrid, which already means we want the lexical
    // lane. The shape weight was a second, conflicting gate that suppressed
    // lexical for short NL concepts routed to Hybrid.
    //
    // NL shapes yield no tokens from extract_tokens (their words aren't code
    // identifiers), but a short NL concept routed to Hybrid (e.g. "parse
    // imports") is exactly the case where the literal words should hit the
    // trigram lane — so use the short-NL extractor there.
    let tokens = if shape.kind == QueryKind::NaturalLanguage {
        query_shape::extract_short_nl_lexical_tokens(query)
    } else {
        query_shape::extract_tokens(query, shape)
    };
    let token_refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);
    // No extension filter: the trigram index already covers the project's text
    // files. Gating the lexical candidate set on the *semantic* extension
    // allow-list made named config/doc files (Cargo.toml, README.md,
    // package.json) structurally unreachable in hybrid mode — exactly the
    // literal-filename hits the lexical lane exists to catch.
    let ranked = index.lexical_rank_with_stats(&query_trigrams, None, LEXICAL_ENUMERATION_LIMIT);
    LexicalCollection {
        files: ranked.files,
        ready: true,
        engine_capped: ranked.engine_capped,
    }
}

fn search_index_ready(ctx: &AppContext) -> bool {
    ctx.search_index()
        .borrow()
        .as_ref()
        .is_some_and(|index| index.ready)
}

fn embed_query(query: &str, ctx: &AppContext) -> Result<Vec<f32>, String> {
    let mut model_ref = ctx.semantic_embedding_model().borrow_mut();
    let semantic_config = ctx.config().semantic.clone();

    if model_ref.is_none() {
        *model_ref = Some(EmbeddingModel::from_config(&semantic_config)?);
    }

    let model = model_ref
        .as_mut()
        .ok_or_else(|| "embedding model was not initialized".to_string())?;
    let query_vector = model
        .embed_query_cached(query)
        .map_err(|error| format!("failed to embed query: {error}"))?;

    if let Some(index) = ctx.semantic_index().borrow().as_ref() {
        if index.len() > 0 && index.dimension() != query_vector.len() {
            return Err(format!(
                "semantic embedding dimension mismatch: query backend returned {}, index expects {}. Rebuild the semantic index for the active backend/model.",
                query_vector.len(),
                index.dimension()
            ));
        }
    }

    Ok(query_vector)
}

pub fn fuse_hybrid_results(
    semantic: Vec<SemanticResult>,
    lexical_files: Vec<(PathBuf, f32)>,
    shape: &QueryShape,
    top_k: usize,
) -> Vec<HybridResult> {
    if top_k == 0 {
        return Vec::new();
    }

    if lexical_files.is_empty() {
        return semantic
            .into_iter()
            .map(|result| hybrid_from_semantic(result, None))
            .take(top_k)
            .collect();
    }

    if semantic.is_empty() {
        return lexical_files
            .into_iter()
            .take(top_k)
            .map(|(file, score)| lexical_only_result(file, score, shape))
            .collect();
    }

    // Use every collected lexical candidate, not a hidden sub-cap. The lexical
    // lane already bounds enumeration at LEXICAL_ENUMERATION_LIMIT upstream and
    // returns candidates pre-ranked by score; an additional `.take(20)` here
    // silently dropped candidates 21..=50 from both the semantic-boost map and
    // the standalone-lexical results without that loss being reflected in
    // `more_available`/`engine_capped`. The final output is already bounded by
    // cap_per_file + truncate(top_k), so honoring all collected candidates is
    // both more correct and honest about what was considered.
    let lexical_top_files: HashMap<PathBuf, f32> = lexical_files.iter().cloned().collect();
    let mut results: Vec<HybridResult> = semantic
        .into_iter()
        .map(|result| {
            let lexical_score = lexical_top_files.get(&result.file).copied();
            hybrid_from_semantic(result, lexical_score)
        })
        .collect();

    let semantic_files: HashSet<PathBuf> =
        results.iter().map(|result| result.file.clone()).collect();
    for (file, score) in &lexical_files {
        if !semantic_files.contains(file) {
            results.push(lexical_only_result(file.clone(), *score, shape));
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut results = cap_per_file(results, 2);
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    results.truncate(top_k);
    results
}

fn hybrid_from_semantic(result: SemanticResult, lexical_score: Option<f32>) -> HybridResult {
    let semantic_score = result.score;
    let hybrid_boosted = lexical_score.is_some();
    let score = if hybrid_boosted {
        semantic_score * HYBRID_LEXICAL_BOOST
    } else {
        semantic_score
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
    }
}

fn lexical_only_result(file: PathBuf, lexical_score: f32, shape: &QueryShape) -> HybridResult {
    HybridResult {
        file,
        name: String::new(),
        kind: SymbolKind::FileSummary,
        start_line: 0,
        end_line: 0,
        exported: false,
        // Lexical scores are not cosine-normalized and can exceed the semantic
        // lane's score scale. Keep lexical-only files visible without letting
        // broad trigram overlaps evict strong semantic matches.
        score: (lexical_score * shape_dependent_lexical_only_weight(shape))
            .min(LEXICAL_ONLY_SCORE_CEILING),
        source: "lexical",
        semantic_score: None,
        lexical_score: Some(lexical_score),
        hybrid_boosted: false,
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
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut capped = Vec::new();
    for result in results {
        let count = counts.entry(result.file.clone()).or_insert(0);
        if *count < cap {
            *count += 1;
            capped.push(result);
        }
    }
    capped
}

fn semantic_error_response(request_id: &str, error: &str) -> Response {
    if is_onnx_runtime_unavailable(error) {
        return Response::error(
            request_id,
            "semantic_search_unavailable",
            format!("Semantic search unavailable: {error}"),
        );
    }

    Response::error(
        request_id,
        "semantic_search_failed",
        format!("semantic_search: {error}"),
    )
}

fn format_lexical_unavailable_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
) -> String {
    if results.is_empty() {
        return format!(
            "{detail}\nSemantic search unavailable; lexical-only fallback returned 0 result(s). [semantic: unavailable]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: unavailable]",
        format_result_sections(results, project_root),
        results.len()
    )
}

fn format_grep_lexical_unavailable_text(
    detail: &str,
    result: &GrepResult,
    project_root: &Path,
) -> String {
    if result.matches.is_empty() {
        return format!(
            "{detail}\nSemantic search unavailable; lexical-only fallback returned 0 result(s). [semantic: unavailable]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: unavailable]",
        crate::commands::grep::format_grep_text(result, project_root),
        result.matches.len()
    )
}

fn building_lexical_note(lexical_index_ready: bool) -> &'static str {
    if lexical_index_ready {
        "Semantic index is rebuilding; results are lexical-only fallback results from the trigram index."
    } else {
        "Semantic index is rebuilding; lexical fallback is unavailable because the trigram index is not ready."
    }
}

fn format_building_lexical_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
    lexical_index_ready: bool,
) -> String {
    let note = building_lexical_note(lexical_index_ready);
    if results.is_empty() {
        return format!(
            "{detail}\n{note}\nFound 0 lexical fallback result(s). [semantic: rebuilding]"
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
) -> String {
    if results.is_empty() {
        return "Found 0 results.".to_string();
    }

    let mut text = format_result_sections(results, project_root);
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
fn enrich_snippets_from_source(results: &mut [HybridResult]) -> bool {
    // Cache reads so two top-3 hits in the same file read it once.
    let mut file_lines: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    let mut incomplete = false;

    for (rank, result) in results.iter_mut().enumerate() {
        if result.source == "lexical" || matches!(result.kind, SymbolKind::FileSummary) {
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

        let lines = file_lines.entry(result.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(&result.file)
                .ok()
                .map(|content| content.lines().map(str::to_string).collect())
        });

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

fn format_result_sections(results: &[HybridResult], project_root: &Path) -> String {
    // Results arrive sorted by fused score desc. Group by file preserving
    // first-appearance order so the most relevant file's group renders first.
    // A BTreeMap would re-sort groups alphabetically by path and scramble the
    // ranking the agent relies on to read most-relevant-first. Snippets are
    // already budgeted by enrich_snippets_from_source; render them verbatim.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&HybridResult>> = HashMap::new();

    for result in results.iter() {
        let display_path = result
            .file
            .strip_prefix(project_root)
            .unwrap_or(&result.file)
            .display()
            .to_string();
        if !groups.contains_key(&display_path) {
            group_order.push(display_path.clone());
        }
        groups.entry(display_path).or_default().push(result);
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
            for result in &groups[file] {
                if result.source == "lexical" {
                    // Whole-file lexical match (no specific symbol).
                    section.push_str(" [lexical match]");
                    continue;
                }
                if matches!(result.kind, SymbolKind::FileSummary) {
                    section.push_str(&format!("\n  {} [file summary]", result.name));
                } else {
                    section.push_str(&format!(
                        "\n  {} [{}] lines {}-{}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        display_line_number(result.start_line),
                        display_line_number(result.end_line),
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

fn literal_tokens_all_short(query: &str) -> bool {
    let tokens = query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    !tokens.is_empty() && tokens.iter().all(|token| token.len() < 3)
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
    if let Some(threshold) = code.strip_prefix("search_too_many_files:") {
        return format!(
            "Project source-file count exceeds search_index threshold ({} files); trigram index disabled. Narrow project_root or open a smaller subdirectory.",
            threshold
        );
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
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use crate::semantic_index::SemanticIndex;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;

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
    fn short_nl_concept_routes_to_hybrid_when_lexical_ready() {
        // "parse imports" classifies as a two-word lowercase NL concept, but it
        // is a literal code phrase the trigram lane can hit. With lexical ready
        // it must route to Hybrid (run the lexical lane), not pure Semantic.
        let shape = query_shape::classify("parse imports");
        assert_eq!(shape.kind, QueryKind::NaturalLanguage);
        let mut warnings = Vec::new();
        let mode = choose_mode(
            SearchHint::Auto,
            "parse imports",
            &shape,
            true,
            &mut warnings,
        );
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
        let mode = choose_mode(SearchHint::Auto, q, &shape, true, &mut warnings);
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
        *ctx.search_index().borrow_mut() = Some(index);
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
            stage: "embedding".to_string(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
            stats: None,
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
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("lexical fallback"));
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
    fn regex_query_runs_without_semantic_index() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn exported() {}\n").expect("write source file");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

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
    fn literal_hint_short_token_warns_and_runs_grep_line_results() {
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
    fn unsupported_regex_returns_specific_error() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("(?=foo)", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "unsupported_pattern");
        assert!(response["message"]
            .as_str()
            .expect("message")
            .contains("lookaround"));
    }

    #[test]
    fn humanize_degraded_reason_messages() {
        let reasons = vec![
            "home_root".to_string(),
            "search_too_many_files:20000".to_string(),
            "custom".to_string(),
        ];
        let human = humanize_degraded_reasons(&reasons);
        assert!(human[0].contains("home directory"));
        assert!(human[1].contains("search_index threshold (20000 files)"));
        assert!(human[1].contains("Narrow project_root"));
        assert_eq!(human[2], "(Degraded: custom)");
        assert!(human.join("; ").contains("; "));
    }

    #[test]
    fn semantic_candidate_limit_scales_with_small_top_k() {
        assert_eq!(semantic_candidate_limit(1), SEMANTIC_OVERFETCH_FLOOR);
        assert_eq!(semantic_candidate_limit(5), 15);
        assert_eq!(semantic_candidate_limit(100), MAX_TOP_K);
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
                    max_batch_size: 64,
                    max_files: 20_000,
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
        *ctx.semantic_index().borrow_mut() =
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
        }];

        let text = format_semantic_text(&results, project_root, false, false);

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
        let body = (0..body_lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).expect("write symbol file");
        HybridResult {
            file: path,
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_line: 0,
            end_line: (body_lines.saturating_sub(1)) as u32,
            exported: false,
            snippet: String::new(),
            score: 0.5,
            source: "semantic",
            semantic_score: Some(0.5),
            lexical_score: None,
            hybrid_boosted: false,
        }
    }

    #[test]
    fn rows_omit_score_and_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 2)];
        let incomplete = enrich_snippets_from_source(&mut results);
        let text = format_semantic_text(&results, dir.path(), false, incomplete);
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
        let incomplete = enrich_snippets_from_source(&mut results);
        assert!(incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete);

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
    fn weak_top_match_emits_low_confidence_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hit = write_symbol_hit(dir.path(), "a.rs", "foo", 2);
        // Top semantic cosine below the weak floor.
        hit.semantic_score = Some(0.22);
        hit.score = 0.22;
        let results = vec![hit];
        let text = format_semantic_text(&results, dir.path(), false, false);
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
        let text = format_semantic_text(&results, dir.path(), false, false);
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
        let incomplete = enrich_snippets_from_source(&mut results);
        assert!(!incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete);
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
        }];
        // Must not panic; header renders, no snippet body.
        let _ = enrich_snippets_from_source(&mut results);
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
    fn more_available_appends_raise_topk_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 1)];
        let text = format_semantic_text(&results, dir.path(), true, false);
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
}
