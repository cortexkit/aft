use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lsp_types::request::Rename;
use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Range, RenameParams, WorkspaceEdit,
};
use serde::Deserialize;

use crate::context::AppContext;
use crate::lsp::position::{build_text_document_position, uri_to_path};
use crate::lsp::LspError;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct LspRenameCommandParams {
    file: String,
    line: u32,
    character: u32,
    new_name: String,
}

#[derive(Debug, Clone)]
struct PendingTextEdit {
    range: Range,
    new_text: String,
}

#[derive(Debug)]
struct FileChange {
    file: PathBuf,
    edits: usize,
}

/// Handle the `lsp_rename` command.
/// Renames a symbol across the workspace via LSP, applying all changes atomically.
///
/// Params:
///   - `file` (string, required) — source file path
///   - `line` (integer, required, 1-based) — cursor line
///   - `character` (integer, required, 1-based) — cursor column
///   - `new_name` (string, required) — replacement symbol name
pub fn handle_lsp_rename(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspRenameCommandParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_rename: invalid params: {err}"),
            );
        }
    };

    if params.line == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_rename: 'line' must be >= 1",
        );
    }

    if params.character == 0 {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_rename: 'character' must be >= 1",
        );
    }

    if params.new_name.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_rename: 'new_name' must not be empty",
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
                    format!("lsp_rename: failed to open file: {err}"),
                );
            }
        }
    };

    if server_keys.is_empty() {
        return Response::error(
            &req.id,
            "no_server",
            "lsp_rename: no LSP server available for this file",
        );
    }

    let canonical_path = match std::fs::canonicalize(&file_path) {
        Ok(path) => path,
        Err(err) => {
            return Response::error(
                &req.id,
                "lsp_error",
                format!("lsp_rename: cannot canonicalize path: {err}"),
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
                    format!("lsp_rename: failed to build position: {err}"),
                );
            }
        };

    let rename_params = RenameParams {
        text_document_position: position_params,
        new_name: params.new_name,
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
                    "lsp_rename: no active LSP client for file",
                );
            }
        };
        client.send_request::<Rename>(rename_params)
    };

    let workspace_edit = match result {
        Ok(Some(edit)) => edit,
        Ok(None) => {
            return Response::error(&req.id, "lsp_error", "rename failed: LSP returned no edits");
        }
        Err(err) => {
            return Response::error(&req.id, "lsp_error", format!("rename failed: {err}"));
        }
    };

    match apply_workspace_edit(&workspace_edit, ctx, req.session(), &req.id) {
        Ok(changes) => {
            let total_files = changes.len();
            let total_edits: usize = changes.iter().map(|change| change.edits).sum();
            let changes_json: Vec<serde_json::Value> = changes
                .iter()
                .map(|change| {
                    serde_json::json!({
                        "file": change.file.display().to_string(),
                        "edits": change.edits,
                    })
                })
                .collect();

            Response::success(
                &req.id,
                serde_json::json!({
                    "renamed": true,
                    "changes": changes_json,
                    "total_files": total_files,
                    "total_edits": total_edits,
                }),
            )
        }
        Err(resp) => resp,
    }
}

fn apply_workspace_edit(
    edit: &WorkspaceEdit,
    ctx: &AppContext,
    session: &str,
    req_id: &str,
) -> Result<Vec<FileChange>, Response> {
    let op_id = crate::backup::new_op_id();
    let file_changes = collect_workspace_edit_changes(edit, ctx, req_id)?;
    if file_changes.is_empty() {
        return Err(Response::error(
            req_id,
            "lsp_error",
            "rename failed: workspace edit did not contain any text edits",
        ));
    }

    let snapshotted = snapshot_affected_files(&file_changes, session, ctx, &op_id)
        .map_err(|err| Response::error(req_id, "lsp_error", format!("rename failed: {err}")))?;
    let result = apply_collected_changes(&file_changes, ctx);

    match result {
        Ok(changes) => Ok(changes),
        Err(err) => {
            rollback_rename(ctx, session, &snapshotted);
            Err(Response::error(
                req_id,
                "lsp_error",
                format!("rename failed: {err}"),
            ))
        }
    }
}

fn collect_workspace_edit_changes(
    edit: &WorkspaceEdit,
    ctx: &AppContext,
    req_id: &str,
) -> Result<BTreeMap<PathBuf, Vec<PendingTextEdit>>, Response> {
    let mut file_changes: BTreeMap<PathBuf, Vec<PendingTextEdit>> = BTreeMap::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            let path = path_for_uri(uri, ctx, req_id)?;
            let entry = file_changes.entry(path).or_default();
            for text_edit in edits {
                entry.push(PendingTextEdit {
                    range: text_edit.range,
                    new_text: text_edit.new_text.clone(),
                });
            }
        }
    }

    if !file_changes.is_empty() {
        return Ok(file_changes);
    }

    if let Some(document_changes) = &edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for document_edit in edits {
                    let path = path_for_uri(&document_edit.text_document.uri, ctx, req_id)?;
                    let entry = file_changes.entry(path).or_default();
                    for edit in &document_edit.edits {
                        match edit {
                            OneOf::Left(text_edit) => entry.push(PendingTextEdit {
                                range: text_edit.range,
                                new_text: text_edit.new_text.clone(),
                            }),
                            OneOf::Right(annotated_edit) => entry.push(PendingTextEdit {
                                range: annotated_edit.text_edit.range,
                                new_text: annotated_edit.text_edit.new_text.clone(),
                            }),
                        }
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for operation in ops {
                    match operation {
                        DocumentChangeOperation::Edit(document_edit) => {
                            let path = path_for_uri(&document_edit.text_document.uri, ctx, req_id)?;
                            let entry = file_changes.entry(path).or_default();
                            for edit in &document_edit.edits {
                                match edit {
                                    OneOf::Left(text_edit) => entry.push(PendingTextEdit {
                                        range: text_edit.range,
                                        new_text: text_edit.new_text.clone(),
                                    }),
                                    OneOf::Right(annotated_edit) => entry.push(PendingTextEdit {
                                        range: annotated_edit.text_edit.range,
                                        new_text: annotated_edit.text_edit.new_text.clone(),
                                    }),
                                }
                            }
                        }
                        DocumentChangeOperation::Op(_) => {
                            return Err(Response::error(
                                req_id,
                                "lsp_error",
                                "rename failed: workspace edit contains unsupported file operation",
                            ));
                        }
                    }
                }
            }
        }
    }

    Ok(file_changes)
}

fn snapshot_affected_files(
    file_changes: &BTreeMap<PathBuf, Vec<PendingTextEdit>>,
    session: &str,
    ctx: &AppContext,
    op_id: &str,
) -> Result<Vec<PathBuf>, LspError> {
    let mut backup = ctx.backup().borrow_mut();
    let mut snapshotted = Vec::with_capacity(file_changes.len());

    for path in file_changes.keys() {
        backup
            .snapshot_with_op(session, path, "lsp_rename", Some(op_id))
            .map_err(|err| {
                LspError::NotFound(format!("failed to snapshot '{}': {err}", path.display()))
            })?;
        snapshotted.push(path.clone());
    }

    Ok(snapshotted)
}

fn apply_collected_changes(
    file_changes: &BTreeMap<PathBuf, Vec<PendingTextEdit>>,
    ctx: &AppContext,
) -> Result<Vec<FileChange>, LspError> {
    let mut results = Vec::with_capacity(file_changes.len());

    for (path, edits) in file_changes {
        let original_content = std::fs::read_to_string(&path).map_err(LspError::Io)?;
        let updated_content = apply_text_edits(&original_content, edits)?;

        std::fs::write(path, &updated_content).map_err(LspError::Io)?;
        ctx.lsp_notify_file_changed(path, &updated_content);

        results.push(FileChange {
            file: path.clone(),
            edits: edits.len(),
        });
    }

    Ok(results)
}

fn apply_text_edits(source: &str, edits: &[PendingTextEdit]) -> Result<String, LspError> {
    let mut sorted = edits.to_vec();
    sorted.sort_by(|left, right| compare_ranges_desc(&left.range, &right.range));

    let mut content = source.to_string();
    for edit in sorted {
        let start =
            line_col_to_byte_lsp(&content, edit.range.start.line, edit.range.start.character)?;
        let end = line_col_to_byte_lsp(&content, edit.range.end.line, edit.range.end.character)?;

        if start > end || end > content.len() {
            return Err(LspError::NotFound(
                "workspace edit contained invalid range".to_string(),
            ));
        }

        content.replace_range(start..end, &edit.new_text);
    }

    Ok(content)
}

fn compare_ranges_desc(left: &Range, right: &Range) -> std::cmp::Ordering {
    right
        .start
        .line
        .cmp(&left.start.line)
        .then(right.start.character.cmp(&left.start.character))
        .then(right.end.line.cmp(&left.end.line))
        .then(right.end.character.cmp(&left.end.character))
}

fn path_for_uri(uri: &lsp_types::Uri, ctx: &AppContext, req_id: &str) -> Result<PathBuf, Response> {
    let path = uri_to_path(uri).ok_or_else(|| {
        Response::error(
            req_id,
            "lsp_error",
            format!(
                "rename failed: failed to resolve file path from URI '{}'",
                uri.as_str()
            ),
        )
    })?;
    ctx.validate_path(req_id, &path)
}

fn line_col_to_byte_lsp(source: &str, line: u32, character: u32) -> Result<usize, LspError> {
    let target_line = line as usize;
    let mut line_start = 0;

    // Keep the UTF-16 conversion local, but preserve raw newline bytes by iterating
    // split_inclusive('\n') segments instead of source.lines(); that keeps CRLF offsets accurate.
    for (index, segment) in source.split_inclusive('\n').enumerate() {
        let line_text = segment.strip_suffix('\n').unwrap_or(segment);
        if index == target_line {
            return Ok(line_start + utf16_column_to_byte(line_text, character));
        }
        line_start += segment.len();
    }

    if target_line == 0 && source.is_empty() {
        return Ok(0);
    }

    if target_line == source.lines().count() && source.ends_with('\n') && character == 0 {
        return Ok(source.len());
    }

    Err(LspError::NotFound(format!(
        "line {} is out of bounds for workspace edit",
        line + 1
    )))
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

fn rollback_rename(ctx: &AppContext, session: &str, snapshotted: &[PathBuf]) {
    for path in snapshotted.iter().rev() {
        let restored = ctx.backup().borrow_mut().restore_latest(session, path);
        if let Ok((entry, _)) = restored {
            ctx.lsp_notify_file_changed(path, &entry.content);
        }
    }
}
