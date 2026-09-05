use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use std::path::Path;

/// Handle the `undo` command: restore the latest operation, or one file when requested.
///
/// Params: `file` (string, optional) — path to a single file to undo.
/// Returns: `{ path, backup_id }` on success, or `no_undo_history` error.
pub fn handle_undo(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut backup = ctx.backup().lock();

    let Some(file) = req.params.get("file").and_then(|v| v.as_str()) else {
        if let Some(reason) = backup.take_latest_skipped_reason_for_undo(req.session(), None) {
            return backup_skipped_response(req, reason);
        }
        return match backup.restore_last_operation(req.session()) {
            Ok(operation) => Response::success(
                &req.id,
                serde_json::json!({
                    "operation": true,
                    "op_id": operation.op_id,
                    "restored_count": operation.restored.len(),
                    "restored": operation.restored.into_iter().map(|file| {
                        serde_json::json!({
                            "path": file.path.display().to_string(),
                            "backup_id": file.backup_id,
                        })
                    }).collect::<Vec<_>>(),
                    "warnings": operation.warnings,
                }),
            ),
            Err(e) => Response::error(&req.id, e.code(), e.to_string()),
        };
    };

    // Resolve relative paths against the bound project root BEFORE validation so
    // the backup key matches the path the mutating tool recorded. A relative path
    // passed straight to `canonicalize_key` would be joined against the daemon's
    // cwd, missing the stack and reporting a false `no_undo_history`.
    let input = ctx.resolve_relative_path(Path::new(file));
    let resolved = match ctx.validate_write_location(&req.id, &input) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    if let Some(reason) =
        backup.take_latest_skipped_reason_for_undo(req.session(), Some(resolved.as_path()))
    {
        return backup_skipped_response(req, reason);
    }

    match backup.restore_latest(req.session(), &resolved) {
        Ok((entry, warning)) => {
            let mut result = serde_json::json!({
                "path": file,
                "backup_id": entry.backup_id,
            });
            if let Some(w) = warning {
                result["warning"] = serde_json::Value::String(w);
            }
            Response::success(&req.id, result)
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn backup_skipped_response(
    req: &RawRequest,
    reason: crate::backup::BackupSkippedReason,
) -> Response {
    Response::error_with_data(
        &req.id,
        "no_undo_history",
        format!(
            "undo is unavailable for this change because its backup snapshot was skipped ({})",
            reason.as_str()
        ),
        serde_json::json!({ "backup_skipped_reason": reason.as_str() }),
    )
}

/// Handle the `undo_preview` command: return paths the next undo would touch without restoring.
///
/// Params: `file`/`filePath` (string, optional) — when provided, previews that per-file stack;
/// otherwise previews the most recent operation in the session.
/// Returns: `{ paths, count }` on success, or `no_undo_history` error.
pub fn handle_undo_preview(req: &RawRequest, ctx: &AppContext) -> Response {
    let backup = ctx.backup().lock();

    let path = match req
        .params
        .get("file")
        .or_else(|| req.params.get("filePath"))
        .and_then(|v| v.as_str())
    {
        Some(file) => {
            let input = ctx.resolve_relative_path(Path::new(file));
            match ctx.validate_write_location(&req.id, &input) {
                Ok(path) => Some(path),
                Err(response) => return response,
            }
        }
        None => None,
    };

    if let Some(reason) = backup.latest_skipped_reason_for_undo(req.session(), path.as_deref()) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "paths": path.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "count": path.iter().count(),
                "backup_skipped_reason": reason.as_str(),
                "undo_unavailable": true,
            }),
        );
    }

    let preview = path
        .as_deref()
        .map(|path| {
            backup
                .preview_latest_path(req.session(), path)
                .map(|path| vec![path])
                .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
        })
        .unwrap_or_else(|| {
            backup
                .preview_last_operation_paths(req.session())
                .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
        });

    match preview {
        Ok(paths) => Response::success(
            &req.id,
            serde_json::json!({
                "paths": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "count": paths.len(),
            }),
        ),
        Err(response) => response,
    }
}
