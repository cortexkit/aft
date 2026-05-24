use std::path::Path;

use lsp_types::request::PrepareRenameRequest;
use lsp_types::{Position, PrepareRenameResponse, Range};
use serde::Deserialize;

use crate::context::AppContext;
use crate::lsp::position::{build_text_document_position, lsp_range_to_aft};
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct LspPrepareRenameParams {
    file: String,
    line: u32,
    character: u32,
}

/// Handle the `lsp_prepare_rename` command.
/// Checks if a symbol at the given position can be renamed.
pub fn handle_lsp_prepare_rename(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspPrepareRenameParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_prepare_rename: invalid params: {err}"),
            );
        }
    };

    if params.line == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_prepare_rename: 'line' must be >= 1",
        );
    }

    if params.character == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_prepare_rename: 'character' must be >= 1",
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
                    format!("lsp_prepare_rename: failed to open file: {err}"),
                );
            }
        }
    };

    if server_keys.is_empty() {
        return Response::error(
            &req.id,
            "no_server",
            "lsp_prepare_rename: no LSP server available for this file",
        );
    }

    let canonical_path = match std::fs::canonicalize(&file_path) {
        Ok(path) => path,
        Err(err) => {
            return Response::error(
                &req.id,
                "lsp_error",
                format!("lsp_prepare_rename: cannot canonicalize path: {err}"),
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
                    format!("lsp_prepare_rename: failed to build position: {err}"),
                );
            }
        };

    let request_position = position_params.position;

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
                    "lsp_prepare_rename: no active LSP client for file",
                );
            }
        };
        client.send_request::<PrepareRenameRequest>(position_params)
    };

    match result {
        Ok(Some(response)) => {
            match build_prepare_rename_response(&canonical_path, request_position, response) {
                Ok(body) => Response::success(&req.id, body),
                Err(err) => Response::error(
                    &req.id,
                    "lsp_error",
                    format!("lsp_prepare_rename: failed to interpret response: {err}"),
                ),
            }
        }
        Ok(None) | Err(_) => cannot_rename_response(&req.id, "Cannot rename this element"),
    }
}

fn build_prepare_rename_response(
    file_path: &Path,
    request_position: Position,
    response: PrepareRenameResponse,
) -> Result<serde_json::Value, std::io::Error> {
    let source = std::fs::read_to_string(file_path)?;

    match response {
        PrepareRenameResponse::Range(range) => Ok(can_rename_body(
            &range,
            placeholder_for_range(&source, &range),
        )),
        PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
            Ok(can_rename_body(&range, placeholder))
        }
        PrepareRenameResponse::DefaultBehavior { default_behavior } => {
            if !default_behavior {
                return Ok(serde_json::json!({
                    "can_rename": false,
                    "reason": "Cannot rename this element",
                }));
            }

            if let Some((range, placeholder)) = identifier_at_position(&source, request_position) {
                Ok(can_rename_body(&range, placeholder))
            } else {
                Ok(serde_json::json!({
                    "can_rename": true,
                }))
            }
        }
    }
}

fn can_rename_body(range: &Range, placeholder: String) -> serde_json::Value {
    let (start_line, start_column, end_line, end_column) = lsp_range_to_aft(range);
    serde_json::json!({
        "can_rename": true,
        "range": {
            "start_line": start_line,
            "start_column": start_column,
            "end_line": end_line,
            "end_column": end_column,
        },
        "placeholder": placeholder,
    })
}

fn cannot_rename_response(req_id: &str, reason: &str) -> Response {
    Response::success(
        req_id,
        serde_json::json!({
            "can_rename": false,
            "reason": reason,
        }),
    )
}

fn placeholder_for_range(source: &str, range: &Range) -> String {
    let start = line_col_to_byte_lsp(source, range.start.line, range.start.character);
    let end = line_col_to_byte_lsp(source, range.end.line, range.end.character);
    if start >= end {
        return String::new();
    }

    source.get(start..end).unwrap_or("").to_string()
}

fn identifier_at_position(source: &str, position: Position) -> Option<(Range, String)> {
    let line_text = source.lines().nth(position.line as usize)?;
    let byte = utf16_column_to_byte(line_text, position.character);
    let bytes = line_text.as_bytes();

    if bytes.is_empty() {
        return None;
    }

    let mut start = byte.min(bytes.len());
    while start > 0 && is_identifier_byte(bytes[start.saturating_sub(1)]) {
        start -= 1;
    }

    let mut end = byte.min(bytes.len());
    while end < bytes.len() && is_identifier_byte(bytes[end]) {
        end += 1;
    }

    if start == end {
        return None;
    }

    let placeholder = line_text.get(start..end)?.to_string();
    Some((
        Range {
            start: Position::new(position.line, start as u32),
            end: Position::new(position.line, end as u32),
        },
        placeholder,
    ))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn line_col_to_byte_lsp(source: &str, line: u32, character: u32) -> usize {
    let target_line = line as usize;
    let mut line_start = 0;

    // Keep the UTF-16 conversion local, but preserve raw newline bytes by iterating
    // split_inclusive('\n') segments instead of source.lines(); that keeps CRLF offsets accurate.
    for (index, segment) in source.split_inclusive('\n').enumerate() {
        let line_text = segment.strip_suffix('\n').unwrap_or(segment);
        if index == target_line {
            return line_start + utf16_column_to_byte(line_text, character);
        }
        line_start += segment.len();
    }

    if source.is_empty() && target_line == 0 {
        return 0;
    }

    source.len()
}

fn utf16_column_to_byte(line: &str, character: u32) -> usize {
    let target = character as usize;
    let mut utf16_offset = 0;

    for (byte_offset, ch) in line.char_indices() {
        if utf16_offset >= target {
            return byte_offset;
        }

        let next = utf16_offset + ch.len_utf16();
        if next > target {
            return byte_offset;
        }

        utf16_offset = next;
    }

    line.len()
}
