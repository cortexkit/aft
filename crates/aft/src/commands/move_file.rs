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

    let src_path = match ctx.validate_write_location(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let dst_path = match ctx.validate_write_location(&req.id, Path::new(destination)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let source_metadata = match std::fs::symlink_metadata(&src_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // When the source is missing AND the destination already exists, the
            // most likely cause is that this rename was already done earlier in
            // the session (or by another process). Surfacing this distinction
            // saves the agent a round-trip to discover it via `ls` or stat.
            if std::fs::symlink_metadata(&dst_path).is_ok() {
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
        Err(error) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("move_file: failed to stat source file: {}", error),
            );
        }
    };

    if source_metadata.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("move_file: '{}' is a directory, not a file", file),
        );
    }

    if std::fs::symlink_metadata(&dst_path).is_ok() {
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

    if let Err(e) = ctx.backup().lock().snapshot_op_tombstone(
        req.session(),
        &op_id,
        &dst_path,
        "move_file: destination created during move",
    ) {
        ctx.backup()
            .lock()
            .discard_operation_entries(req.session(), &op_id);
        return Response::error(&req.id, e.code(), e.to_string());
    }

    // Create parent directories for destination
    if let Some(parent) = dst_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("move_file: failed to create directories: {}", e),
                );
            }
        }
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
                .lock()
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
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), &op_id, None);

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
        Err(rename_error) => {
            let source_is_symlink = fs::symlink_metadata(src_path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            if source_is_symlink {
                return move_symlink_after_rename_failure(src_path, dst_path, &rename_error);
            }

            match fs::copy(src_path, dst_path) {
                Ok(_) => remove_source_after_fallback(src_path),
                Err(_) => MoveOutcome::Failed(rename_error.to_string()),
            }
        }
    }
}

fn move_symlink_after_rename_failure(
    src_path: &Path,
    dst_path: &Path,
    rename_error: &std::io::Error,
) -> MoveOutcome {
    let target = match fs::read_link(src_path) {
        Ok(target) => target,
        Err(_) => return MoveOutcome::Failed(rename_error.to_string()),
    };

    // std::fs::copy follows a symlink and would silently turn the destination
    // into a regular file. Recreate the link itself, preserving its target text:
    // a relative target stays relative and can legitimately dangle after a move.
    let create_result = create_symlink(&target, src_path, dst_path);
    let create_result = if create_result
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
        && rename_error.kind() == std::io::ErrorKind::CrossesDevices
    {
        match fs::remove_file(dst_path) {
            Ok(()) => create_symlink(&target, src_path, dst_path),
            Err(error) => Err(error),
        }
    } else {
        create_result
    };

    match create_result {
        Ok(()) => remove_source_after_fallback(src_path),
        Err(_) => MoveOutcome::Failed(rename_error.to_string()),
    }
}

fn remove_source_after_fallback(src_path: &Path) -> MoveOutcome {
    match fs::remove_file(src_path) {
        Ok(()) => MoveOutcome::Moved,
        Err(remove_error) => MoveOutcome::CopiedSourceDeleteFailed(remove_error.to_string()),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, _src_path: &Path, dst_path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dst_path)
}

#[cfg(windows)]
fn create_symlink(target: &Path, src_path: &Path, dst_path: &Path) -> std::io::Result<()> {
    let resolved_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        src_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(target)
    };
    if fs::metadata(resolved_target).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, dst_path)
    } else {
        // Dangling targets cannot reveal their type; file links are the safest
        // default and match this command's file-only contract.
        std::os::windows::fs::symlink_file(target, dst_path)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _src_path: &Path, _dst_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlink moves are unsupported on this platform",
    ))
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

    #[cfg(unix)]
    use super::move_symlink_after_rename_failure;
    #[cfg(unix)]
    use std::io::{Error, ErrorKind};
    #[cfg(unix)]
    use std::path::Path;

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

    #[cfg(unix)]
    #[test]
    fn symlink_fallback_preserves_relative_target_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_dir = temp.path().join("source");
        let destination_dir = temp.path().join("destination");
        std::fs::create_dir_all(&source_dir).expect("source directory");
        std::fs::create_dir_all(&destination_dir).expect("destination directory");
        std::fs::write(source_dir.join("target.txt"), "target contents\n").expect("target file");
        let source = source_dir.join("link.txt");
        let destination = destination_dir.join("link.txt");
        let target = Path::new("target.txt");
        std::os::unix::fs::symlink(target, &source).expect("source symlink");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert_eq!(outcome, MoveOutcome::Moved);
        assert!(std::fs::symlink_metadata(&source).is_err());
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&destination).expect("link target"),
            target
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_fallback_moves_dangling_link() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-link");
        let destination = temp.path().join("destination-link");
        let target = Path::new("missing/target.txt");
        std::os::unix::fs::symlink(target, &source).expect("dangling source symlink");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert_eq!(outcome, MoveOutcome::Moved);
        assert!(std::fs::symlink_metadata(&source).is_err());
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&destination).expect("link target"),
            target
        );
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_symlink_fallback_replaces_existing_destination() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-link");
        let destination = temp.path().join("destination-link");
        let target = Path::new("target.txt");
        std::os::unix::fs::symlink(target, &source).expect("source symlink");
        std::fs::write(&destination, "stale destination\n").expect("existing destination");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert_eq!(outcome, MoveOutcome::Moved);
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&destination).expect("link target"),
            target
        );
    }
}
