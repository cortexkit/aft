//! Handler for the `write` command: full file write with auto-backup.

use std::path::Path;

use lsp_types::FileChangeType;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// Handle a `write` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `content` (string, required) — content to write
///   - `create_dirs` (bool, optional, default true) — create parent dirs if missing
///
/// Returns: `{ file, created, syntax_valid?, backup_id? }`
pub fn handle_write(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "write: missing required param 'file'",
            );
        }
    };

    let content = match req.params.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "write: missing required param 'content'",
            );
        }
    };

    let create_dirs = req
        .params
        .get("create_dirs")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let existed = path.exists();

    // Capture pre-write content when the file exists so we can detect no-op
    // writes (file content byte-identical to original) and emit honest
    // `no_op: true` for UIs. Used by diff metadata too when requested.
    // See GitHub #45.
    let original = if existed {
        match std::fs::read_to_string(path.as_path()) {
            Ok(content) => content,
            Err(error) => {
                crate::slog_warn!(
                    "write: failed to read existing file before diff for {}: {}",
                    file,
                    error
                );
                String::new()
            }
        }
    } else {
        String::new()
    };

    // Auto-backup existing files before overwriting. For create-only writes,
    // record a tombstone so operation undo removes the created file.
    let backup_id = if existed {
        match edit::auto_backup(
            ctx,
            req.session(),
            path.as_path(),
            "write: pre-write backup",
            Some(&op_id),
        ) {
            Ok(id) => id,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        }
    } else {
        match ctx.backup().borrow_mut().snapshot_op_tombstone(
            req.session(),
            &op_id,
            path.as_path(),
            "write: file created by write",
        ) {
            Ok(id) => Some(id),
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        }
    };

    // Create parent directories if requested
    if create_dirs {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    if !existed {
                        ctx.backup()
                            .borrow_mut()
                            .discard_operation_entries(req.session(), &op_id);
                    }
                    return Response::error(
                        &req.id,
                        "invalid_request",
                        format!("write: failed to create directories: {}", e),
                    );
                }
            }
        }
    }

    // Write, format, and validate via shared pipeline
    let mut write_result =
        match edit::write_format_validate(path.as_path(), content, &ctx.config(), &req.params) {
            Ok(r) => r,
            Err(e) => {
                if !existed {
                    ctx.backup()
                        .borrow_mut()
                        .discard_operation_entries(req.session(), &op_id);
                }
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

    if let Ok(final_content) = std::fs::read_to_string(path.as_path()) {
        let config_change_type = if existed {
            FileChangeType::CHANGED
        } else {
            FileChangeType::CREATED
        };
        ctx.lsp_notify_watched_config_file(path.as_path(), config_change_type);
        write_result.lsp_outcome = ctx.lsp_post_write(path.as_path(), &final_content, &req.params);
    }

    log::debug!("write: {}", file);

    let mut result = serde_json::json!({
        "file": file,
        "created": !existed,
        "formatted": write_result.formatted,
    });

    if let Some(valid) = write_result.syntax_valid {
        result["syntax_valid"] = serde_json::json!(valid);
    }

    if let Some(ref reason) = write_result.format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if write_result.validate_requested {
        result["validation_errors"] = serde_json::json!(write_result.validation_errors);
    }
    if let Some(ref reason) = write_result.validate_skipped_reason {
        result["validate_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    write_result.append_lsp_diagnostics_to(&mut result);

    // Read final on-disk content once for no_op detection + diff metadata.
    // Honest reporting: when the file existed AND the post-write content is
    // byte-identical to `original`, surface `no_op: true` so UIs can render
    // "wrote, but no net change" instead of a bare "File updated". See
    // GitHub #45.
    let final_content =
        std::fs::read_to_string(path.as_path()).unwrap_or_else(|_| content.to_string());
    if existed && original == final_content {
        result["no_op"] = serde_json::json!(true);
    }

    if edit::wants_diff(&req.params) {
        result["diff"] = edit::compute_diff_info(&original, &final_content);
    }

    Response::success(&req.id, result)
}
