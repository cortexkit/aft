use std::path::Path;
use std::time::Instant;

use crate::commands::callgraph_borrowed::{disclose_borrowed_answer, BorrowedOp, SymbolLookup};
use crate::commands::callgraph_store_adapter::serialized_response;
use crate::commands::callgraph_store_adapter::{
    callers_result, index_refusal_response, note_callgraph_served, store_error_response,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};
use crate::{slog_info, slog_warn};

/// Handle a `callers` request.
pub fn handle_callers(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut response = answer_callers(req, ctx);
    disclose_borrowed_answer(
        ctx,
        req,
        &mut response,
        SymbolLookup::Graph,
        BorrowedOp::Callers,
    );
    response
}

fn answer_callers(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "callers: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "callers: missing required param 'symbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .min(100) as usize;

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
                    "Callgraph target is not indexed: path is outside project_root. Got: {} (project_root: {}); use grep or aft_search with pattern to find references",
                    file_path.display(),
                    project_root.display(),
                ),
            );
        }
    }

    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "callers", error)
        }
        other => return index_refusal_response(&req.id, "callers", ctx, &other),
    };

    if let Err(response) = super::callgraph_store_adapter::ensure_target_indexed(
        &req.id, "callers", ctx, &store, &file_path,
    ) {
        return response;
    }

    let started = Instant::now();
    let include_tests = include_tests_param(req);
    let outcome = callers_result(&store, &file_path, symbol, depth, include_tests);
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Ok(result) => {
            slog_info!(
                "callers: '{}' in {} → {} sites in {}ms",
                result.symbol,
                file_path.display(),
                result.total_callers,
                elapsed_ms
            );
            note_callgraph_served(
                ctx,
                "callers",
                elapsed_ms.min(u64::MAX as u128) as u64,
                "ok",
            );
            serialized_response(&req.id, "callers", &result)
        }
        Err(error) => {
            slog_warn!(
                "callers: '{}' failed after {}ms: {}",
                symbol,
                elapsed_ms,
                error
            );
            store_error_response(&req.id, "callers", error)
        }
    }
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}
