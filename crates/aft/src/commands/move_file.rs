//! Handler for the `move_file` command: rename/move a file with backup.

use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// Handle a `move_file` request.
///
/// Params:
///   - `file` (string, required) — source file path
///   - `destination` (string, required) — destination file path
///
/// Returns: `{ file, destination, moved, backup_id }`
pub fn handle_move_file(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_file: missing required param 'file'",
            );
        }
    };

    let destination = match req.params.get("destination").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_file: missing required param 'destination'",
            );
        }
    };

    let src_path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let dst_path = match ctx.validate_path(&req.id, Path::new(destination)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    if !src_path.exists() {
        // When the source is missing AND the destination already exists, the
        // most likely cause is that this rename was already done earlier in
        // the session (or by another process). Surfacing this distinction
        // saves the agent a round-trip to discover it via `ls` or stat.
        if dst_path.exists() {
            return Response::error(
                &req.id,
                "file_not_found",
                format!(
                    "move_file: source file not found: {}. Destination '{}' already exists \
                     — was this file already moved earlier? Verify with `read` before retrying.",
                    file, destination
                ),
            );
        }
        return Response::error(
            &req.id,
            "file_not_found",
            format!("move_file: source file not found: {}", file),
        );
    }

    if src_path.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("move_file: '{}' is a directory, not a file", file),
        );
    }

    if dst_path.exists() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("move_file: destination already exists: {}", destination),
        );
    }

    // Backup source before moving
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        src_path.as_path(),
        "move_file: pre-move backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Create parent directories for destination
    if let Some(parent) = dst_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                ctx.backup()
                    .borrow_mut()
                    .discard_operation_entries(req.session(), &op_id);
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("move_file: failed to create directories: {}", e),
                );
            }
        }
    }

    if let Err(e) = ctx.backup().borrow_mut().snapshot_op_tombstone(
        req.session(),
        &op_id,
        &dst_path,
        "move_file: destination created during move",
    ) {
        return Response::error(&req.id, e.code(), e.to_string());
    }

    // Move the file
    let move_outcome = match move_file_on_disk(&src_path, &dst_path) {
        MoveOutcome::Moved => MoveOutcome::Moved,
        MoveOutcome::CopiedSourceDeleteFailed(message) => {
            crate::slog_warn!("move_file: copied but failed to remove source: {}", message);
            MoveOutcome::CopiedSourceDeleteFailed(message)
        }
        MoveOutcome::Failed(message) => {
            ctx.backup()
                .borrow_mut()
                .discard_operation_entries(req.session(), &op_id);
            return Response::error(
                &req.id,
                "io_error",
                format!("move_file: failed to move file: {}", message),
            );
        }
    };

    log::debug!("move_file: {} -> {}", file, destination);

    if move_outcome == MoveOutcome::Moved {
        let source_for_lsp = unresolved_existing_path(&src_path, Path::new(file));
        ctx.lsp_notify_watched_config_file(&source_for_lsp, FileChangeType::DELETED);
    }
    ctx.lsp_notify_watched_config_file(&dst_path, FileChangeType::CREATED);

    let mut result = move_success_result(file, destination, move_outcome);

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    Response::success(&req.id, result)
}

#[derive(Debug, PartialEq, Eq)]
enum MoveOutcome {
    Moved,
    CopiedSourceDeleteFailed(String),
    Failed(String),
}

fn move_file_on_disk(src_path: &Path, dst_path: &Path) -> MoveOutcome {
    match fs::rename(src_path, dst_path) {
        Ok(()) => MoveOutcome::Moved,
        Err(rename_error) => match fs::copy(src_path, dst_path) {
            Ok(_) => match fs::remove_file(src_path) {
                Ok(()) => MoveOutcome::Moved,
                Err(remove_error) => {
                    MoveOutcome::CopiedSourceDeleteFailed(remove_error.to_string())
                }
            },
            Err(_) => MoveOutcome::Failed(rename_error.to_string()),
        },
    }
}

fn unresolved_existing_path(resolved_path: &Path, requested_path: &Path) -> PathBuf {
    if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        resolved_path.to_path_buf()
    }
}

fn move_success_result(
    file: &str,
    destination: &str,
    move_outcome: MoveOutcome,
) -> serde_json::Value {
    let mut result = serde_json::json!({
        "file": file,
        "destination": destination,
        "moved": true,
    });

    if let MoveOutcome::CopiedSourceDeleteFailed(message) = move_outcome {
        result["complete"] = serde_json::json!(false);
        result["source_delete_failed"] = serde_json::json!(true);
        result["warning"] = serde_json::json!(format!(
            "destination was written, but source file could not be deleted after copy: {message}. Both paths now exist; retry deleting the source or accept the duplicate."
        ));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{move_success_result, MoveOutcome};

    #[test]
    fn copied_but_source_delete_failed_shape_marks_partial_success() {
        let result = move_success_result(
            "src.txt",
            "dst.txt",
            MoveOutcome::CopiedSourceDeleteFailed("permission denied".to_string()),
        );

        assert_eq!(result["moved"], true);
        assert_eq!(result["complete"], false);
        assert_eq!(result["source_delete_failed"], true);
        assert!(result["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("Both paths now exist")));
    }
}
