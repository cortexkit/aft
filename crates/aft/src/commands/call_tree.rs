use std::path::Path;

use crate::commands::callgraph_store_adapter::suspended_response;
use crate::commands::callgraph_store_adapter::{
    building_response, call_tree_result, note_callgraph_building, note_callgraph_served,
    serialized_response, store_error_response, unavailable_for,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};

/// Handle a `call_tree` request.
pub fn handle_call_tree(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "call_tree: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "call_tree: missing required param 'symbol'",
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
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            );
        }
    }

    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Building => {
            note_callgraph_building(ctx, "call_tree");
            return building_response(&req.id, "call_tree");
        }
        CallgraphStoreAccess::Suspended(suspension) => {
            return suspended_response(&req.id, "call_tree", &suspension)
        }
        CallgraphStoreAccess::Unavailable => return unavailable_for(&req.id, "call_tree", ctx),
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "call_tree", error)
        }
    };

    match call_tree_result(&store, &file_path, symbol, depth, include_tests_param(req)) {
        Ok(tree) => {
            note_callgraph_served(ctx, "call_tree", 0, "ok");
            serialized_response(&req.id, "call_tree", &tree)
        }
        Err(error) => store_error_response(&req.id, "call_tree", error),
    }
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}
