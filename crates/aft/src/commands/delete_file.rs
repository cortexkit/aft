//! Handler for the `delete_file` command: remove file(s) or directory with backup.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// Handle a `delete_file` request.
///
/// Params:
///   - `file` (string) — single file/dir path
///   - `files` (string[]) — multiple paths (file or dir mixed); takes precedence over `file`
///   - `recursive` (bool, optional, default false) — required to delete a
///     directory. Refuses dir deletion when false to prevent accidental wipes
///     when an agent passes a directory path expecting file semantics.
///
/// All deletes inside a single tool call share one operation id, so a single
/// `aft_safety undo` (without filePath) restores everything atomically.
///
/// Returns single-file: `{ file, deleted, backup_id? }`
/// Returns directory:   `{ file, deleted, is_directory, files_deleted, backup_ids }`
/// Returns batch:       `{ complete, deleted: [...], skipped_files: [...] }`
pub fn handle_delete_file(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let recursive = req
        .params
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Batch mode: `files: [...]`
    if let Some(files) = req.params.get("files").and_then(|v| v.as_array()) {
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        for value in files {
            let Some(file) = value.as_str() else {
                skipped.push(serde_json::json!({"file": value, "reason": "not a string"}));
                continue;
            };
            match delete_one_or_dir(req, ctx, file, recursive, &op_id) {
                Ok(result) => deleted.push(result),
                Err(resp) => skipped.push(serde_json::json!({
                    "file": file,
                    "reason": resp.data.get("message").and_then(|v| v.as_str()).unwrap_or("delete failed"),
                })),
            }
        }
        return Response::success(
            &req.id,
            serde_json::json!({
                "complete": skipped.is_empty(),
                "deleted": deleted,
                "skipped_files": skipped,
            }),
        );
    }

    // Single-target mode: `file: "..."`
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "delete_file: missing required param 'file' or 'files'",
            );
        }
    };

    match delete_one_or_dir(req, ctx, file, recursive, &op_id) {
        Ok(result) => Response::success(&req.id, result),
        Err(resp) => resp,
    }
}

/// Delete a single path (file or directory). Returns the per-target result
/// payload on success (shape varies for file vs directory), or a ready-made
/// error `Response` on failure for the caller to either propagate (single
/// mode) or aggregate into `skipped_files` (batch mode).
fn delete_one_or_dir(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    recursive: bool,
    op_id: &str,
) -> Result<serde_json::Value, Response> {
    let requested_path = Path::new(file);
    if is_symlink(requested_path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to inspect '{}': {}", file, e),
        )
    })? {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            format!(
                "delete_file: refusing to delete symlink '{}'; symlink undo is not supported",
                file
            ),
        ));
    }

    let path = match ctx.validate_path(&req.id, requested_path) {
        Ok(path) => path,
        Err(resp) => return Err(resp),
    };

    if !path.exists() {
        return Err(Response::error(
            &req.id,
            "file_not_found",
            format!("delete_file: file not found: {}", file),
        ));
    }

    if is_symlink(&path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to inspect '{}': {}", file, e),
        )
    })? {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            format!(
                "delete_file: refusing to delete symlink '{}'; symlink undo is not supported",
                file
            ),
        ));
    }

    if path.is_dir() {
        if !recursive {
            return Err(Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "delete_file: '{}' is a directory. Pass recursive: true to delete it with all contents.",
                    file
                ),
            ));
        }
        return delete_directory(req, ctx, &path, file, op_id);
    }

    if !path.is_file() {
        return Err(Response::error(
            &req.id,
            "unsupported_directory_contents",
            format!(
                "delete_file: refusing to delete unsupported non-regular file '{}'; undo cannot restore this file type",
                file
            ),
        ));
    }

    if has_multiple_hard_links(&path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to inspect '{}': {}", file, e),
        )
    })? {
        return Err(Response::error(
            &req.id,
            "unsupported_directory_contents",
            format!(
                "delete_file: refusing to delete hard-linked file '{}'; undo cannot restore hard-link topology",
                file
            ),
        ));
    }

    // Backup before deletion
    let backup_id = edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "delete_file: pre-delete backup",
        Some(op_id),
    )
    .map_err(|e| Response::error(&req.id, e.code(), e.to_string()))?;

    // Delete the file
    if let Err(e) = std::fs::remove_file(&path) {
        return Err(Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to delete: {}", e),
        ));
    }

    ctx.lsp_notify_watched_config_file(path.as_path(), FileChangeType::DELETED);

    log::debug!("delete_file: {}", file);

    let mut result = serde_json::json!({
        "file": file,
        "deleted": true,
    });
    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }
    Ok(result)
}

fn is_symlink(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn has_multiple_hard_links(path: &Path) -> std::io::Result<bool> {
    Ok(std::fs::metadata(path)?.nlink() > 1)
}

#[cfg(not(unix))]
fn has_multiple_hard_links(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// Recursively delete a directory after backing up every file inside.
///
/// Every file backup uses the same `op_id` so a single `aft_safety undo`
/// restores the entire tree atomically. Guardrails reject symlinks and empty
/// directories until backup metadata can preserve those node types.
fn delete_directory(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    original: &str,
    op_id: &str,
) -> Result<serde_json::Value, Response> {
    let unsupported_paths = validate_directory_for_recursive_delete(path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!(
                "delete_file: failed to validate directory '{}': {}",
                original, e
            ),
        )
    })?;
    if !unsupported_paths.is_empty() {
        return Err(Response::error(
            &req.id,
            "unsupported_directory_contents",
            unsupported_directory_contents_message(&unsupported_paths),
        ));
    }

    let mut files_to_backup: Vec<PathBuf> = Vec::new();
    if let Err(e) = collect_files(path, &mut files_to_backup) {
        return Err(Response::error(
            &req.id,
            "io_error",
            format!(
                "delete_file: failed to walk directory '{}': {}",
                original, e
            ),
        ));
    }

    let mut backup_ids: Vec<String> = Vec::new();
    for file_path in &files_to_backup {
        match edit::auto_backup(
            ctx,
            req.session(),
            file_path,
            "delete_file: pre-delete backup (directory contents)",
            Some(op_id),
        ) {
            Ok(Some(id)) => backup_ids.push(id),
            Ok(None) => {}
            Err(e) => {
                return Err(Response::error(
                    &req.id,
                    e.code(),
                    format!(
                        "delete_file: backup failed for '{}' inside '{}': {}",
                        file_path.display(),
                        original,
                        e
                    ),
                ));
            }
        }
    }

    if let Err(e) = std::fs::remove_dir_all(path) {
        return Err(Response::error(
            &req.id,
            "io_error",
            format!(
                "delete_file: failed to remove directory '{}': {}",
                original, e
            ),
        ));
    }

    // Notify LSP for every file that disappeared so watched-file diagnostics
    // refresh.
    for file_path in &files_to_backup {
        ctx.lsp_notify_watched_config_file(file_path.as_path(), FileChangeType::DELETED);
    }

    log::debug!(
        "delete_file: recursively removed directory '{}' ({} file(s))",
        original,
        files_to_backup.len()
    );

    Ok(serde_json::json!({
        "file": original,
        "deleted": true,
        "is_directory": true,
        "files_deleted": files_to_backup.len(),
        "backup_ids": backup_ids,
    }))
}

/// Guardrail for recursive deletes: the backup/undo format currently records
/// only file contents. Reject directory trees that contain entries undo cannot
/// restore atomically (symlinks and empty directories) before taking backups or
/// deleting anything.
fn validate_directory_for_recursive_delete(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut unsupported_paths = Vec::new();
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        unsupported_paths.push(dir.display().to_string());
        return Ok(unsupported_paths);
    }
    validate_directory_entries(dir, &mut unsupported_paths)?;
    Ok(unsupported_paths)
}

fn validate_directory_entries(
    dir: &Path,
    unsupported_paths: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        entries.push(entry?);
    }

    if entries.is_empty() {
        unsupported_paths.push(dir.display().to_string());
        return Ok(());
    }

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            unsupported_paths.push(path.display().to_string());
        } else if file_type.is_dir() {
            validate_directory_entries(&path, unsupported_paths)?;
        } else if file_type.is_file() {
            if has_multiple_hard_links(&path)? {
                unsupported_paths.push(path.display().to_string());
            }
        } else {
            unsupported_paths.push(path.display().to_string());
        }
    }

    Ok(())
}

fn unsupported_directory_contents_message(paths: &[String]) -> String {
    const MAX_PATHS: usize = 5;

    let mut message = String::from(
        "aft_delete with recursive: true does not yet support directory trees containing symlinks, empty directories, hard links, sockets, device nodes, or other non-regular files. Restore would not recover these entries atomically.",
    );
    message.push_str(" Offending path(s): ");
    message.push_str(
        &paths
            .iter()
            .take(MAX_PATHS)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", "),
    );
    if paths.len() > MAX_PATHS {
        message.push_str(&format!(", ... and {} more", paths.len() - MAX_PATHS));
    }
    message
}

/// Walk a directory recursively, collecting all regular file paths.
/// Skips symlinked directories to avoid following loops; symlinked files
/// are included.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            out.push(path);
        } else if file_type.is_dir() {
            collect_files(&path, out)?;
        }
    }
    Ok(())
}
