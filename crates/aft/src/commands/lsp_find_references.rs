use std::path::Path;

use serde::Deserialize;

use lsp_types::request::References;
use lsp_types::{Location, ReferenceContext, ReferenceParams};

use crate::context::AppContext;
use crate::lsp::position::{build_text_document_position, lsp_range_to_aft, uri_to_path};
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct LspFindReferencesParams {
    file: String,
    line: u32,
    character: u32,
    #[serde(default = "default_include_declaration")]
    include_declaration: bool,
}

fn default_include_declaration() -> bool {
    true
}

/// Handle an `lsp_find_references` request.
///
/// Params:
///   - `file` (string, required) — source file path
///   - `line` (integer, required, 1-based) — cursor line
///   - `character` (integer, required, 1-based) — cursor column
///   - `include_declaration` (bool, optional) — include the symbol declaration itself (default: true)
///
/// Returns: `{ references: [{ file, line, column, end_line, end_column }], total }`
pub fn handle_lsp_find_references(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspFindReferencesParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_find_references: invalid params: {err}"),
            );
        }
    };

    if params.line == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_find_references: 'line' must be >= 1",
        );
    }

    if params.character == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_find_references: 'character' must be >= 1",
        );
    }

    let file_path = match ctx.validate_path(&req.id, Path::new(&params.file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let server_keys = {
        let mut lsp = ctx.lsp();
        let config = ctx.config();
        match lsp.ensure_file_open(&file_path, &config) {
            Ok(keys) => keys,
            Err(err) => {
                return Response::error(
                    &req.id,
                    "lsp_error",
                    format!("lsp_find_references: failed to open file: {err}"),
                );
            }
        }
    };

    if server_keys.is_empty() {
        return Response::error(
            &req.id,
            "no_server",
            "lsp_find_references: no LSP server available for this file",
        );
    }

    let canonical_path = match std::fs::canonicalize(&file_path) {
        Ok(path) => path,
        Err(err) => {
            return Response::error(
                &req.id,
                "lsp_error",
                format!("lsp_find_references: cannot canonicalize path: {err}"),
            );
        }
    };

    let position_params =
        match build_text_document_position(&canonical_path, params.line, params.character) {
            Ok(position) => position,
            Err(err) => {
                return Response::error(
                    &req.id,
                    "lsp_error",
                    format!("lsp_find_references: failed to build position: {err}"),
                );
            }
        };

    let reference_params = ReferenceParams {
        text_document_position: position_params,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: params.include_declaration,
        },
    };

    ctx.lsp().drain_events();

    let result = {
        let mut lsp = ctx.lsp();
        let config = ctx.config();
        let client = match lsp.client_for_file_mut(&canonical_path, &config) {
            Some(client) => client,
            None => {
                return Response::error(
                    &req.id,
                    "no_server",
                    "lsp_find_references: no active LSP client for file",
                );
            }
        };
        client.send_request::<References>(reference_params)
    };

    match result {
        Ok(Some(locations)) => {
            let references: Vec<serde_json::Value> =
                locations.iter().map(reference_to_json).collect();
            let total = references.len();
            Response::success(
                &req.id,
                serde_json::json!({
                    "references": references,
                    "total": total,
                }),
            )
        }
        Ok(None) => Response::success(
            &req.id,
            serde_json::json!({
                "references": [],
                "total": 0,
            }),
        ),
        Err(err) => Response::error(
            &req.id,
            "lsp_error",
            format!("lsp_find_references: request failed: {err}"),
        ),
    }
}

fn reference_to_json(location: &Location) -> serde_json::Value {
    let (line, column, end_line, end_column) = lsp_range_to_aft(&location.range);
    let file = uri_to_path(&location.uri)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| location.uri.as_str().to_string());

    serde_json::json!({
        "file": file,
        "line": line,
        "column": column,
        "end_line": end_line,
        "end_column": end_column,
    })
}
