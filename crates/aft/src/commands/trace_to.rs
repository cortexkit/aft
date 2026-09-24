use std::path::Path;

#[path = "../list_surfaces/trace.rs"]
pub mod trace;

use crate::commands::callgraph_store_adapter::{
    index_refusal_response, note_callgraph_served, serialized_value, store_error_response,
    trace_to_result,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_to` request.
pub fn handle_trace_to(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to: missing required param 'symbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
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
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "trace_to", error)
        }
        other => return index_refusal_response(&req.id, "trace_to", ctx, &other),
    };

    match trace_to_result(&store, &file_path, symbol, depth, include_tests_param(req)) {
        Ok(result) => {
            note_callgraph_served(ctx, "trace_to", 0, "ok");
            let envelope = trace::build_trace_to_envelope(
                result.paths.len(),
                result.total_paths,
                result.max_depth_reached,
                result.total_paths_is_lower_bound,
            );
            match serialized_value(&req.id, "trace_to", &result) {
                Ok(mut value) => {
                    trace::attach_trace_to_envelope(&mut value, envelope.as_ref());
                    Response::success(&req.id, value)
                }
                Err(response) => response,
            }
        }
        Err(error) => store_error_response(&req.id, "trace_to", error),
    }
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}
