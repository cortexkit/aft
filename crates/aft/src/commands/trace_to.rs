use std::path::Path;

#[path = "../list_surfaces/trace.rs"]
pub mod trace;

use crate::commands::callgraph_store_adapter::{
    note_callgraph_served, serialized_value, store_error_response, trace_to_result,
};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_to` request.
pub fn handle_trace_to(req: &RawRequest, ctx: &AppContext) -> Response {
    answer_trace_to(req, ctx)
}

fn answer_trace_to(req: &RawRequest, ctx: &AppContext) -> Response {
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

    let file_path = match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
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

    let (snapshot, data_view) = match crate::data_layer::tools::callgraph::ops_graph(
        &req.id,
        "trace_to",
        ctx,
        &[file_path.as_path()],
    ) {
        Ok(graph) => graph,
        Err(response) => return response,
    };
    let outcome = trace_to_result(
        &snapshot,
        &file_path,
        symbol,
        depth,
        include_tests_param(req),
    );
    if let Some(response) =
        crate::data_layer::tools::callgraph::failed_reads(&req.id, "trace_to", data_view.as_ref())
    {
        return response;
    }
    match outcome {
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
