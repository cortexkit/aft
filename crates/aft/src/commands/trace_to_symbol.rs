use std::path::{Path, PathBuf};

use crate::commands::callgraph_store_adapter::suspended_response;
use crate::commands::callgraph_store_adapter::{
    building_response, ensure_symbol_resolves, note_callgraph_building, note_callgraph_served,
    serialized_response, serialized_value, store_error_response, trace_to_symbol_candidates,
    trace_to_symbol_result, unavailable_for,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::inspect::job::is_test_file;
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_to_symbol` request.
pub fn handle_trace_to_symbol(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'symbol'",
            );
        }
    };

    let to_symbol = match req.params.get("toSymbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'toSymbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(16) as usize;

    let project_root = ctx.config().project_root.clone();

    let file_path = match validate_callgraph_path(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let to_file_arg = req.params.get("toFile").and_then(|v| v.as_str());
    if let Some(to_file) = to_file_arg {
        let requested_path = resolve_request_path(project_root.as_deref(), to_file);
        if !requested_path.exists() {
            return Response::error(
                &req.id,
                "to_file_not_found",
                format!("trace_to_symbol: toFile not found: {}", to_file),
            );
        }
    }

    let to_file_path = match to_file_arg {
        Some(to_file) => match validate_callgraph_path(req, ctx, to_file) {
            Ok(path) => Some(path),
            Err(resp) => return resp,
        },
        None => None,
    };

    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Building => {
            note_callgraph_building(ctx, "trace_to_symbol");
            return building_response(&req.id, "trace_to_symbol");
        }
        CallgraphStoreAccess::Suspended(suspension) => {
            return suspended_response(&req.id, "trace_to_symbol", &suspension)
        }
        CallgraphStoreAccess::Unavailable => {
            return unavailable_for(&req.id, "trace_to_symbol", ctx)
        }
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "trace_to_symbol", error)
        }
    };

    if let Err(error) = ensure_symbol_resolves(&store, &file_path, symbol) {
        return store_error_response(&req.id, "trace_to_symbol", error);
    }

    let include_tests = include_tests_param(req);
    let target_candidates = match trace_to_symbol_candidates(&store, to_symbol) {
        Ok(candidates) => candidates,
        Err(error) => return store_error_response(&req.id, "trace_to_symbol", error),
    };
    let target_candidates = if include_tests {
        target_candidates
    } else {
        target_candidates
            .into_iter()
            .filter(|candidate| !is_test_file(&candidate.file))
            .collect::<Vec<_>>()
    };

    if let Some(to_file_path) = to_file_path.as_deref() {
        if !target_candidates.iter().any(|candidate| {
            trace_candidate_matches_file(project_root.as_deref(), &candidate.file, to_file_path)
        }) {
            let candidates_json =
                match serialized_value(&req.id, "trace_to_symbol candidates", &target_candidates) {
                    Ok(value) => value,
                    Err(response) => return response,
                };
            return Response::error_with_data(
                &req.id,
                "target_symbol_not_in_file",
                format!(
                    "trace_to_symbol: target symbol '{}' is not defined in toFile: {}",
                    to_symbol,
                    to_file_arg.unwrap_or("<unknown>")
                ),
                serde_json::json!({ "candidates": candidates_json }),
            );
        }
    } else {
        match target_candidates.len() {
            0 => {
                return Response::error(
                    &req.id,
                    "target_symbol_not_found",
                    format!("trace_to_symbol: target symbol '{}' not found", to_symbol),
                );
            }
            1 => {}
            _ => {
                let candidates_json = match serialized_value(
                    &req.id,
                    "trace_to_symbol candidates",
                    &target_candidates,
                ) {
                    Ok(value) => value,
                    Err(response) => return response,
                };
                return Response::error_with_data(
                    &req.id,
                    "ambiguous_target",
                    format!(
                        "trace_to_symbol: target symbol '{}' exists in multiple files; pass 'toFile' to disambiguate",
                        to_symbol
                    ),
                    serde_json::json!({ "candidates": candidates_json }),
                );
            }
        }
    }

    match trace_to_symbol_result(
        &store,
        &file_path,
        symbol,
        to_symbol,
        to_file_path.as_deref(),
        depth,
        include_tests,
    ) {
        Ok(result) => {
            note_callgraph_served(ctx, "trace_to_symbol", 0, "ok");
            serialized_response(&req.id, "trace_to_symbol", &result)
        }
        Err(error) => store_error_response(&req.id, "trace_to_symbol", error),
    }
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn resolve_request_path(project_root: Option<&Path>, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_relative() {
        project_root
            .map(|root| root.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

fn trace_candidate_matches_file(
    project_root: Option<&Path>,
    candidate_file: &str,
    target_file: &Path,
) -> bool {
    let candidate_path = resolve_request_path(project_root, candidate_file);
    canonicalize_for_compare(&candidate_path) == canonicalize_for_compare(target_file)
}

fn canonicalize_for_compare(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn validate_callgraph_path(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    let file_path = ctx.validate_path(&req.id, Path::new(file))?;

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
            return Err(Response::error(
                &req.id,
                "path_outside_project_root",
                format!(
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            ));
        }
    }

    Ok(file_path)
}
