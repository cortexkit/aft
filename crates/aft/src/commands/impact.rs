use std::path::Path;

use crate::commands::callgraph_borrowed::{disclose_borrowed_answer, BorrowedOp, SymbolLookup};
use crate::commands::callgraph_store_adapter::serialized_response;
use crate::commands::callgraph_store_adapter::{
    impact_result, index_refusal_response, note_callgraph_served, store_error_response,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};

/// Handle an `impact` request.
pub fn handle_impact(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut response = answer_impact(req, ctx);
    disclose_borrowed_answer(
        ctx,
        req,
        &mut response,
        SymbolLookup::Graph,
        BorrowedOp::Impact,
    );
    response
}

fn answer_impact(req: &RawRequest, ctx: &AppContext) -> Response {
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
            return store_error_response(&req.id, "impact", error)
        }
        other => return index_refusal_response(&req.id, "impact", ctx, &other),
    };

    if let Err(response) = super::callgraph_store_adapter::ensure_target_indexed(
        &req.id, "impact", ctx, &store, &file_path,
    ) {
        return response;
    }

    match impact_result(&store, &file_path, symbol, depth, include_tests_param(req)) {
        Ok(result) => {
            note_callgraph_served(ctx, "impact", 0, "ok");
            serialized_response(&req.id, "impact", &result)
        }
        Err(error) => store_error_response(&req.id, "impact", error),
    }
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}
