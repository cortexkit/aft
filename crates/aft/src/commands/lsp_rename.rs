use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use lsp_types::request::Rename;
use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Range, RenameParams, WorkspaceEdit,
};
use serde::Deserialize;

use crate::backup::CapturedRegularFile;
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

struct RenameRollback {
    checkpoint_name: String,
    op_id: String,
    files: Vec<PathBuf>,
    captures: HashMap<PathBuf, CapturedRegularFile>,
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
        let config = ctx.config();
        match crate::lsp::manager::ensure_file_open_unlocked(|| ctx.lsp(), &file_path, &config) {
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

    let config = ctx.config();
    let Some(result) = crate::lsp::manager::send_file_request_unlocked::<Rename, _>(
        || ctx.lsp(),
        &canonical_path,
        &config,
        rename_params,
    ) else {
        return Response::error(
            &req.id,
            "no_server",
            "lsp_rename: no active LSP client for file",
        );
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

    let rollback = snapshot_affected_files(&file_changes, session, ctx, &op_id)
        .map_err(|err| Response::error(req_id, "lsp_error", format!("rename failed: {err}")))?;
    let result = apply_collected_changes(&file_changes, &rollback.captures, ctx);

    match result {
        Ok(changes) => {
            delete_rename_checkpoint(ctx, session, &rollback.checkpoint_name);
            Ok(changes)
        }
        Err(err) => {
            rollback_rename(ctx, session, &rollback);
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
) -> Result<RenameRollback, LspError> {
    let files = file_changes.keys().cloned().collect::<Vec<_>>();
    let mut captures = HashMap::new();
    for path in &files {
        if let Some(capture) = CapturedRegularFile::read(path).map_err(LspError::Io)? {
            captures.insert(path.clone(), capture);
        }
    }
    let checkpoint_name = format!("lsp_rename-{op_id}");
    {
        let backup = ctx.backup().lock();
        let mut checkpoint = ctx.checkpoint().lock();
        checkpoint
            .create_from_captures(
                session,
                &checkpoint_name,
                files.clone(),
                &backup,
                &mut captures,
            )
            .map_err(|err| {
                LspError::NotFound(format!(
                    "failed to create rollback checkpoint '{}': {err}",
                    checkpoint_name
                ))
            })?;
    }

    let snapshot_result = (|| -> Result<(), LspError> {
        let mut backup = ctx.backup().lock();
        for path in &files {
            let result = if let Some(capture) = captures.get(path) {
                backup.snapshot_with_op_from_capture(
                    session,
                    path,
                    "lsp_rename",
                    Some(op_id),
                    capture,
                )
            } else {
                backup.snapshot_with_op(session, path, "lsp_rename", Some(op_id))
            };
            result.map_err(|err| {
                LspError::NotFound(format!("failed to snapshot '{}': {err}", path.display()))
            })?;
        }
        Ok(())
    })();

    if let Err(error) = snapshot_result {
        delete_rename_checkpoint(ctx, session, &checkpoint_name);
        return Err(error);
    }

    Ok(RenameRollback {
        checkpoint_name,
        op_id: op_id.to_string(),
        files,
        captures,
    })
}

fn rollback_rename(ctx: &AppContext, session: &str, rollback: &RenameRollback) {
    let _view_intent =
        crate::views::intent::record_paths(rollback.files.iter().map(|path| path.as_path()));
    let restored = ctx
        .checkpoint()
        .lock()
        .restore(session, &rollback.checkpoint_name)
        .is_ok();
    if restored {
        ctx.backup()
            .lock()
            .discard_operation_entries(session, &rollback.op_id);
        for path in rollback.files.iter().rev() {
            if let Ok(content) = std::fs::read_to_string(path) {
                ctx.lsp_notify_file_changed(path, &content);
            }
        }
        delete_rename_checkpoint(ctx, session, &rollback.checkpoint_name);
    }
}

fn delete_rename_checkpoint(ctx: &AppContext, session: &str, checkpoint_name: &str) {
    ctx.checkpoint().lock().delete(session, checkpoint_name);
}

fn apply_collected_changes(
    file_changes: &BTreeMap<PathBuf, Vec<PendingTextEdit>>,
    captures: &HashMap<PathBuf, CapturedRegularFile>,
    ctx: &AppContext,
) -> Result<Vec<FileChange>, LspError> {
    let _view_intent =
        crate::views::intent::record_paths(file_changes.keys().map(|path| path.as_path()));
    let mut results = Vec::with_capacity(file_changes.len());

    for (path, edits) in file_changes {
        let original_content = if let Some(capture) = captures.get(path) {
            std::str::from_utf8(capture.bytes())
                .map_err(|error| {
                    LspError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                })?
                .to_owned()
        } else {
            std::fs::read_to_string(path).map_err(LspError::Io)?
        };
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
    if edits.is_empty() {
        return Ok(source.to_owned());
    }
    let mut requested: BTreeMap<u32, std::collections::BTreeSet<u32>> = BTreeMap::new();
    for edit in edits {
        for position in [edit.range.start, edit.range.end] {
            requested
                .entry(position.line)
                .or_default()
                .insert(position.character);
        }
    }
    let positions = resolve_workspace_positions(source, &requested);
    let mut resolved = Vec::with_capacity(edits.len());
    for edit in edits {
        let Some(&start) = positions.get(&(edit.range.start.line, edit.range.start.character))
        else {
            return apply_text_edits_legacy(source, edits);
        };
        let Some(&end) = positions.get(&(edit.range.end.line, edit.range.end.character)) else {
            return apply_text_edits_legacy(source, edits);
        };
        if start > end {
            return apply_text_edits_legacy(source, edits);
        }
        resolved.push((start, end, edit));
    }
    resolved.sort_by(|left, right| compare_ranges_desc(&left.2.range, &right.2.range));
    let mut cursor = 0;
    let mut content = String::with_capacity(source.len());
    for (start, end, edit) in resolved.into_iter().rev() {
        // LSP normally supplies non-overlapping baseline ranges. Preserve the
        // previous sequential semantics for malformed overlapping edits rather
        // than changing their result or introducing a new rejection.
        if start < cursor {
            return apply_text_edits_legacy(source, edits);
        }
        content.push_str(&source[cursor..start]);
        content.push_str(&edit.new_text);
        cursor = end;
    }
    content.push_str(&source[cursor..]);
    Ok(content)
}

/// Resolve requested UTF-16 columns in one pass per requested line. Splitting
/// inclusively retains CRLF bytes, and columns inside a surrogate pair still
/// clamp to the beginning of that code point as in the sequential path.
fn resolve_workspace_positions(
    source: &str,
    requested: &BTreeMap<u32, std::collections::BTreeSet<u32>>,
) -> HashMap<(u32, u32), usize> {
    let mut positions = HashMap::new();
    let mut start = 0;
    let mut line_count = 0;
    for (line, segment) in source.split_inclusive('\n').enumerate() {
        #[cfg(test)]
        RENAME_SCAN_BYTES.with(|count| count.set(count.get() + segment.len()));
        line_count = line + 1;
        if let Some(columns) = requested.get(&(line as u32)) {
            let text = segment.strip_suffix('\n').unwrap_or(segment);
            let mut columns = columns.iter().copied().peekable();
            let mut utf16 = 0;
            for (byte, ch) in text.char_indices() {
                #[cfg(test)]
                RENAME_SCAN_BYTES.with(|count| count.set(count.get() + ch.len_utf8()));
                let next = utf16 + ch.len_utf16() as u32;
                while columns.peek().is_some_and(|column| *column < next) {
                    positions.insert((line as u32, columns.next().unwrap()), start + byte);
                }
                utf16 = next;
                if columns.peek().is_none() {
                    break;
                }
            }
            // Oversized columns clamp against the *mutated* line in the old
            // path, so they cannot safely use baseline offsets.
            for column in columns {
                if column == utf16 {
                    positions.insert((line as u32, column), start + text.len());
                }
            }
        }
        start += segment.len();
    }
    if source.is_empty() {
        if let Some(columns) = requested.get(&0) {
            if columns.contains(&0) {
                positions.insert((0, 0), 0);
            }
        }
    } else if source.ends_with('\n') {
        positions.insert((line_count as u32, 0), source.len());
    }
    positions
}

fn apply_text_edits_legacy(source: &str, edits: &[PendingTextEdit]) -> Result<String, LspError> {
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
        #[cfg(test)]
        RENAME_SCAN_BYTES.with(|count| count.set(count.get() + segment.len()));
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

#[cfg(test)]
thread_local! {
    static RENAME_SCAN_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    #[test]
    fn rename_large_workspace_edits_scan_source_once() {
        let row = "let café = \"🌍\";\r\n";
        let source = row.repeat(20_000);
        let edits = (19_000..20_000)
            .map(|line| PendingTextEdit {
                range: Range::new(Position::new(line, 4), Position::new(line, 8)),
                new_text: "renamed".into(),
            })
            .collect::<Vec<_>>();
        RENAME_SCAN_BYTES.with(|count| count.set(0));
        let actual = apply_text_edits(&source, &edits).unwrap();
        let expected = format!(
            "{}{}",
            row.repeat(19_000),
            "let renamed = \"🌍\";\r\n".repeat(1_000)
        );
        assert_eq!(actual, expected);
        let scanned = RENAME_SCAN_BYTES.with(|count| count.get());
        assert_eq!(scanned, 430_000, "indexed workspace edit scan bytes");
        eprintln!(
            "indexed workspace edit: {scanned} scan bytes for {} source bytes",
            source.len()
        );
        assert!(
            scanned <= source.len() * 2,
            "workspace edit scanned {scanned} bytes for {} source bytes",
            source.len()
        );
    }

    #[test]
    fn rename_matches_sequential_utf16_boundary_and_overlap_semantics() {
        for source in ["", "a", "a\r\n🌍 café\r\n", "a\n\n", "🌍 café"] {
            let mut seed = 19u64;
            for _ in 0..512 {
                let mut edits = Vec::new();
                for _ in 0..4 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let line = ((seed >> 32) % 4) as u32;
                    let start = ((seed >> 16) % 10) as u32;
                    let end = ((seed >> 8) % 10) as u32;
                    edits.push(PendingTextEdit {
                        range: Range::new(Position::new(line, start), Position::new(line, end)),
                        new_text: if seed % 2 == 0 { "new\n" } else { "" }.into(),
                    });
                }
                let actual = apply_text_edits(source, &edits).map_err(|error| error.to_string());
                let expected =
                    apply_text_edits_legacy(source, &edits).map_err(|error| error.to_string());
                assert_eq!(actual, expected, "source={source:?}, edits={edits:?}");
            }
        }
    }
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
