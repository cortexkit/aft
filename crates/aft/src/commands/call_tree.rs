use std::path::Path;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle a `call_tree` request.
///
/// Expects:
/// - `file` (string, required) — path to the source file
/// - `symbol` (string, required) — name of the symbol to trace
/// - `depth` (number, optional, default 5) — max traversal depth
///
/// Returns a nested call tree with fields: `name`, `file`, `line`,
/// `signature`, `resolved`, `children`.
///
/// Returns error if:
/// - required params missing
/// - call graph not initialized (configure not called)
/// - symbol not found in the file
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

    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "call_tree: project not configured — send 'configure' first",
            );
        }
    };

    let file_path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    // Build file data first to check if the symbol exists
    match graph.build_file(&file_path) {
        Ok(data) => {
            // Check if the symbol exists in the file, even when it is a private leaf.
            let has_symbol = data.symbol_metadata.contains_key(symbol)
                || data.exported_symbols.contains(&symbol.to_string());
            if !has_symbol {
                return Response::error(
                    &req.id,
                    "symbol_not_found",
                    format!("call_tree: symbol '{}' not found in {}", symbol, file),
                );
            }
        }
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    }

    match graph.forward_tree(&file_path, symbol, depth) {
        Ok(tree) => {
            let tree_json = serde_json::to_value(&tree).unwrap_or_default();
            Response::success(&req.id, tree_json)
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}
