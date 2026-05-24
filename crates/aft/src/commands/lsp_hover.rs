use std::path::Path;

use serde::Deserialize;

use lsp_types::request::HoverRequest;
use lsp_types::{HoverContents, MarkedString, MarkupKind};

use crate::context::AppContext;
use crate::lsp::position::{build_text_document_position, lsp_range_to_aft};
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct LspHoverParams {
    file: String,
    line: u32,
    character: u32,
}

/// Handle an `lsp_hover` request.
///
/// Params:
///   - `file` (string, required) — source file path
///   - `line` (integer, required, 1-based) — cursor line
///   - `character` (integer, required, 0-based) — cursor column
///
/// Returns: `{ contents: string | null, language?: string, range?: { start_line, start_column, end_line, end_column } }`
pub fn handle_lsp_hover(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspHoverParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_hover: invalid params: {err}"),
            );
        }
    };

    if params.line == 0 {
        return Response::error(&req.id, "invalid_request", "lsp_hover: 'line' must be >= 1");
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
                    format!("lsp_hover: failed to open file: {err}"),
                );
            }
        }
    };

    if server_keys.is_empty() {
        return Response::error(
            &req.id,
            "no_server",
            "lsp_hover: no LSP server available for this file",
        );
    }

    let canonical_path = match std::fs::canonicalize(&file_path) {
        Ok(path) => path,
        Err(err) => {
            return Response::error(
                &req.id,
                "lsp_error",
                format!("lsp_hover: cannot canonicalize path: {err}"),
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
                    format!("lsp_hover: failed to build position: {err}"),
                );
            }
        };

    let hover_params = lsp_types::HoverParams {
        text_document_position_params: position_params,
        work_done_progress_params: Default::default(),
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
                    "lsp_hover: no active LSP client for file",
                );
            }
        };
        client.send_request::<HoverRequest>(hover_params)
    };

    match result {
        Ok(Some(hover)) => {
            let (contents_text, language) = extract_hover_contents(&hover.contents);
            let mut body = serde_json::json!({ "contents": contents_text });

            if let Some(lang) = language {
                body["language"] = serde_json::json!(lang);
            }

            if let Some(range) = hover.range {
                let (start_line, start_column, end_line, end_column) = lsp_range_to_aft(&range);
                body["range"] = serde_json::json!({
                    "start_line": start_line,
                    "start_column": start_column,
                    "end_line": end_line,
                    "end_column": end_column,
                });
            }

            Response::success(&req.id, body)
        }
        Ok(None) => Response::success(&req.id, serde_json::json!({ "contents": null })),
        Err(err) => Response::error(
            &req.id,
            "lsp_error",
            format!("lsp_hover: request failed: {err}"),
        ),
    }
}

/// Extract text content and optional language from HoverContents.
fn extract_hover_contents(contents: &HoverContents) -> (String, Option<String>) {
    match contents {
        HoverContents::Scalar(marked) => extract_marked_string(marked),
        HoverContents::Array(items) => {
            let mut parts = Vec::new();
            let mut language = None;
            for item in items {
                let (text, lang) = extract_marked_string(item);
                if language.is_none() {
                    language = lang;
                }
                parts.push(text);
            }
            (parts.join("\n\n"), language)
        }
        HoverContents::Markup(markup) => (
            markup.value.clone(),
            match markup.kind {
                MarkupKind::Markdown => extract_language_from_markup(&markup.value),
                MarkupKind::PlainText => None,
            },
        ),
    }
}

fn extract_marked_string(marked: &MarkedString) -> (String, Option<String>) {
    match marked {
        MarkedString::String(text) => (text.clone(), None),
        MarkedString::LanguageString(language_string) => (
            language_string.value.clone(),
            Some(language_string.language.clone()),
        ),
    }
}

fn extract_language_from_markup(markup: &str) -> Option<String> {
    let first_line = markup.lines().next()?.trim();
    let language = first_line.strip_prefix("```")?.trim();
    if language.is_empty() {
        None
    } else {
        Some(language.to_string())
    }
}
