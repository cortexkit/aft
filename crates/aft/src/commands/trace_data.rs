use std::path::Path;

use crate::commands::callgraph_store_adapter::{
    index_refusal_response, note_callgraph_served, serialized_value, store_error_response,
    trace_data_result,
};
use crate::commands::trace_to::trace;
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_data` request.
///
/// Traces how an expression flows through variable assignments within a
/// function body and across function boundaries via argument-to-parameter
/// matching. Destructuring, spread, and unresolved calls produce approximate
/// hops and stop tracking.
///
/// Expects:
/// - `file` (string, required) — path to the source file containing the symbol
/// - `symbol` (string, required) — name of the function containing the expression
/// - `expression` (string, required) — the expression/variable name to track
/// - `depth` (number, optional, default 5) — maximum cross-file hop depth
///
/// Returns `TraceDataResult` with fields: `expression`, `origin_file`,
/// `origin_symbol`, `hops` (array of DataFlowHop), `depth_limited`.
///
/// Returns error if:
/// - required params missing
/// - call graph not initialized (configure not called)
/// - symbol not found in the file
pub fn handle_trace_data(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_data: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_data: missing required param 'symbol'",
            );
        }
    };

    let expression = match req.params.get("expression").and_then(|v| v.as_str()) {
        Some(e) => e,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_data: missing required param 'expression'",
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
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "trace_data", error)
        }
        other => return index_refusal_response(&req.id, "trace_data", ctx, &other),
    };

    match trace_data_result(
        &store,
        &file_path,
        symbol,
        expression,
        depth,
        ctx.symbol_cache(),
    ) {
        Ok(result) => {
            note_callgraph_served(ctx, "trace_data", 0, "ok");
            let envelope =
                trace::build_trace_data_envelope(result.hops.len(), result.depth_limited);
            match serialized_value(&req.id, "trace_data", &result) {
                Ok(mut value) => {
                    trace::attach_trace_data_envelope(&mut value, envelope.as_ref());
                    Response::success(&req.id, value)
                }
                Err(response) => response,
            }
        }
        Err(error) => store_error_response(&req.id, "trace_data", error),
    }
}
