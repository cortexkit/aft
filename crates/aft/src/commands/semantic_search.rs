use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::context::{AppContext, SemanticIndexStatus};
use crate::protocol::{RawRequest, Response};
use crate::query_shape::{self, QueryKind, QueryShape};
use crate::search_index::SearchIndex;
use crate::semantic_index::{
    is_onnx_runtime_unavailable, is_semantic_indexed_extension, EmbeddingModel, SemanticResult,
};
use crate::symbols::SymbolKind;

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;
const HYBRID_LEXICAL_BOOST: f32 = 1.1;
const LEXICAL_ONLY_SCORE_CEILING: f32 = 0.25;

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
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
struct SemanticSearchParams {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<SemanticSearchParams>(req.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("semantic_search: invalid params: {error}"),
            );
        }
    };

    match &*ctx.semantic_index_status().borrow() {
        SemanticIndexStatus::Disabled => {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "status": "disabled",
                    "text": "Semantic search is not enabled.",
                }),
            );
        }
        SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
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
            return Response::success(
                &req.id,
                serde_json::json!({
                    "status": "building",
                    "text": detail,
                    "stage": stage,
                    "files": files,
                    "entries_done": entries_done,
                    "entries_total": entries_total,
                }),
            );
        }
        SemanticIndexStatus::Failed(error) => {
            return semantic_error_response(&req.id, error);
        }
        SemanticIndexStatus::Ready => {}
    }

    let query_vector = match embed_query(&params.query, ctx) {
        Ok(query_vector) => query_vector,
        Err(error) => return semantic_error_response(&req.id, &error),
    };

    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);

    let semantic_results = {
        let semantic_index = ctx.semantic_index().borrow();
        let Some(index) = semantic_index.as_ref() else {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "status": "not_ready",
                    "text": "Semantic index is not ready yet.",
                }),
            );
        };
        index.search(&query_vector, params.top_k.clamp(50, MAX_TOP_K))
    };

    let shape = query_shape::classify(&params.query);
    let lexical_files = if shape.weights.should_use_lexical {
        let tokens = query_shape::extract_tokens(&params.query, &shape);
        let token_refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
        let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);
        ctx.search_index()
            .borrow()
            .as_ref()
            .filter(|index| index.ready)
            .map(|index| {
                index.lexical_rank(&query_trigrams, Some(&is_semantic_indexed_extension), 50)
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let results = fuse_hybrid_results(
        semantic_results,
        lexical_files,
        &shape,
        params.top_k.min(MAX_TOP_K),
    );

    // No score threshold: silent filtering produced "0 results" even when the
    // model had reasonable matches the agent could have judged. Surface every
    // hit with its score so the caller can decide.

    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Ready;

    Response::success(
        &req.id,
        serde_json::json!({
            "status": "ready",
            "text": format_semantic_text(&results, &project_root),
            "results": results.iter().map(result_to_json).collect::<Vec<_>>(),
        }),
    )
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
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
        if index.dimension() != query_vector.len() {
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
            .map(|result| hybrid_from_semantic(result, "semantic", None))
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

    let lexical_top_files: HashMap<PathBuf, f32> = lexical_files.iter().take(20).cloned().collect();
    let mut results: Vec<HybridResult> = semantic
        .into_iter()
        .map(|result| {
            if let Some(&lexical_score) = lexical_top_files.get(&result.file) {
                hybrid_from_semantic(result, "hybrid", Some(lexical_score))
            } else {
                hybrid_from_semantic(result, "semantic", None)
            }
        })
        .collect();

    let semantic_files: HashSet<PathBuf> =
        results.iter().map(|result| result.file.clone()).collect();
    for (file, score) in lexical_files.iter().take(20) {
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

fn hybrid_from_semantic(
    result: SemanticResult,
    source: &'static str,
    lexical_score: Option<f32>,
) -> HybridResult {
    let semantic_score = result.score;
    let score = if source == "hybrid" {
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
        source,
        semantic_score: Some(semantic_score),
        lexical_score,
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
        snippet: "[lexical match — use aft_zoom or read for context]".to_string(),
    }
}

fn shape_dependent_lexical_only_weight(shape: &QueryShape) -> f32 {
    match shape.kind {
        QueryKind::Identifier => 0.8,
        QueryKind::Path | QueryKind::ErrorCode | QueryKind::Mixed => 0.5,
        QueryKind::NaturalLanguage => 0.0,
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

fn format_semantic_text(results: &[HybridResult], project_root: &Path) -> String {
    if results.is_empty() {
        return "Found 0 semantic result(s). [index: ready]".to_string();
    }

    let mut groups: BTreeMap<String, Vec<&HybridResult>> = BTreeMap::new();

    for result in results {
        let display_path = result
            .file
            .strip_prefix(project_root)
            .unwrap_or(&result.file)
            .display()
            .to_string();
        groups.entry(display_path).or_default().push(result);
    }

    let sections = groups
        .into_iter()
        .map(|(file, file_results)| {
            let mut section = file;

            for result in file_results {
                if result.source == "lexical" {
                    section.push_str(&format!(" [lexical match — score: {:.3}]", result.score));
                } else if matches!(result.kind, SymbolKind::FileSummary) {
                    section.push_str(&format!(
                        "\n{} [{}] [file summary] score {:.3} source {}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        result.score,
                        result.source
                    ));
                } else {
                    section.push_str(&format!(
                        "\n{} [{}] lines {}-{} score {:.3} source {}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        display_line_number(result.start_line),
                        display_line_number(result.end_line),
                        result.score,
                        result.source
                    ));
                }

                if !result.snippet.trim().is_empty() {
                    for line in result.snippet.lines() {
                        section.push_str("\n    ");
                        section.push_str(line);
                    }
                }
            }

            section
        })
        .collect::<Vec<_>>();

    format!(
        "{}\n\nFound {} semantic result(s). [index: ready]",
        sections.join("\n\n"),
        results.len()
    )
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
        "snippet": result.snippet,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        }];

        let text = format_semantic_text(&results, project_root);

        assert!(text.contains("index [file-summary] [file summary] score 0.750 source semantic"));
        assert!(!text.contains("lines 1-1"));
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
