use std::path::Path;

use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};

/// Handle an `impact` request.
///
/// Performs enriched callers analysis: returns all call sites affected by a
/// symbol change, annotated with the caller's signature, entry point status,
/// source line at the call site, and extracted parameter names.
///
/// Expects:
/// - `file` (string, required) — path to the source file containing the target symbol
/// - `symbol` (string, required) — name of the symbol to analyze
/// - `depth` (number, optional, default 5) — maximum transitive caller depth
///
/// Returns `ImpactResult` with fields: `symbol`, `file`, `signature`,
/// `parameters`, `total_affected`, `affected_files`, `callers`.
///
/// Returns error if:
/// - required params missing
/// - call graph not initialized (configure not called)
/// - symbol not found in the file
pub fn handle_impact(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "impact: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "impact: missing required param 'symbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(100) as usize;

    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "impact: project not configured — send 'configure' first",
            );
        }
    };

    let file_path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let project_root = ctx.config().project_root.clone();
    if let Some(project_root) = project_root {
        let canonical_root = std::fs::canonicalize(&project_root).unwrap_or(project_root.clone());
        let input_for_resolution = if file_path.is_relative() {
            project_root.join(&file_path)
        } else {
            file_path.clone()
        };
        let canonical_input =
            std::fs::canonicalize(&input_for_resolution).unwrap_or(input_for_resolution);
        if !canonical_input.starts_with(&canonical_root) {
            return Response::error(
                &req.id,
                "path_outside_project_root",
                format!(
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            );
        }
    }

    // Build file data first to check if the symbol exists
    match graph.build_file(&file_path) {
        Ok(data) => {
            let has_symbol = data.calls_by_symbol.contains_key(symbol)
                || data.exported_symbols.contains(&symbol.to_string())
                || data.symbol_metadata.contains_key(symbol);
            if !has_symbol {
                return Response::error(
                    &req.id,
                    "symbol_not_found",
                    format!("impact: symbol '{}' not found in {}", symbol, file),
                );
            }
        }
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    }

    let max_files = ctx.config().max_callgraph_files;

    match graph.impact(&file_path, symbol, depth, max_files) {
        Ok(result) => {
            let result_json = serde_json::to_value(&result).unwrap_or_default();
            Response::success(&req.id, result_json)
        }
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            Response::error(&req.id, "project_too_large", format!("{}", err))
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}
