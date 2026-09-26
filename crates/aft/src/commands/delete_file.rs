//! Handler for the `delete_file` command: remove file(s) or directory with backup.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;
use serde_json::Value;

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
/// A recursive delete whose undo backup would copy more than
/// `RECURSIVE_DELETE_BACKUP_MAX_FILES` files or
/// `RECURSIVE_DELETE_BACKUP_MAX_BYTES` bytes (a budget shared by the whole
/// call) is refused with `recursive_delete_backup_too_large` before anything
/// is deleted.
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
    let mut budget = RecursiveDeleteBackupBudget::for_request(ctx);

    // Batch mode: `files: [...]`
    if let Some(files) = req.params.get("files").and_then(|v| v.as_array()) {
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        for value in files {
            let Some(file) = value.as_str() else {
                skipped.push(serde_json::json!({"file": value, "reason": "not a string"}));
                continue;
            };
            match delete_one_or_dir(req, ctx, file, recursive, &op_id, &mut budget) {
                Ok(result) => deleted.push(result),
                Err(resp) => skipped.push(serde_json::json!({
                    "file": file,
                    "reason": resp.data.get("message").and_then(|v| v.as_str()).unwrap_or("delete failed"),
                })),
            }
        }
        if deleted.is_empty() && !skipped.is_empty() {
            let message = format!(
                "delete failed for all {} file(s):\n{}",
                skipped.len(),
                skipped
                    .iter()
                    .map(|entry| {
                        let file = entry
                            .get("file")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| entry.get("file").map(Value::to_string))
                            .unwrap_or_default();
                        let reason = entry
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("delete failed");
                        format!("  {file}: {reason}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            return Response::error_with_data(
                req.id.clone(),
                "delete_failed",
                message,
                serde_json::json!({
                    "complete": false,
                    "all_failed": true,
                    "deleted": deleted,
                    "skipped_files": skipped,
                }),
            );
        }
        let mut result = serde_json::json!({
            "complete": skipped.is_empty(),
            "deleted": deleted,
            "skipped_files": skipped,
        });
        edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), &op_id, None);
        return Response::success(&req.id, result);
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

    match delete_one_or_dir(req, ctx, file, recursive, &op_id, &mut budget) {
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
    budget: &mut RecursiveDeleteBackupBudget,
) -> Result<serde_json::Value, Response> {
    let path = match ctx.validate_write_location(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return Err(resp),
    };

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

    if !path.exists() {
        return Err(Response::error(
            &req.id,
            "file_not_found",
            format!("delete_file: file not found: {}", file),
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
        return delete_directory(req, ctx, &path, file, op_id, budget);
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
        // A failed remove leaves this file unchanged. Discard its snapshot so
        // the failed request does not become a phantom undo operation.
        ctx.backup()
            .lock()
            .discard_latest_operation_entry_for_path(req.session(), op_id, &path);
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
    edit::attach_backup_skipped_reason(
        &mut result,
        ctx,
        req.session(),
        op_id,
        Some(path.as_path()),
    );
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
    budget: &mut RecursiveDeleteBackupBudget,
) -> Result<serde_json::Value, Response> {
    // A vanished mounted child can make std::fs::ReadDir::drop panic after
    // closedir returns ENXIO, aborting the daemon. Capture the root device before
    // either recursive pass so neither validation nor backup collection crosses it.
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!(
                "delete_file: failed to establish filesystem boundary for '{}': {}",
                original, e
            ),
        )
    })?;
    // Bound the backup before anything else walks the tree: validation below
    // reads every directory, so it runs only on a tree already known to fit.
    let collected = match collect_files_within_budget(path, &boundary, budget) {
        Ok(collected) => collected,
        Err(CollectError::OverBudget(exceeded)) => {
            return Err(over_budget_response(req, original, exceeded, budget));
        }
        Err(CollectError::Io(e)) => {
            return Err(Response::error(
                &req.id,
                "io_error",
                format!(
                    "delete_file: failed to walk directory '{}': {}",
                    original, e
                ),
            ));
        }
    };

    let unsupported_paths =
        validate_directory_for_recursive_delete(path, &boundary).map_err(|e| {
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

    let files_to_backup = collected.files;

    let mut backup_ids: Vec<String> = Vec::new();
    let mut backed_up_paths: Vec<PathBuf> = Vec::new();
    for file_path in &files_to_backup {
        match edit::auto_backup(
            ctx,
            req.session(),
            file_path,
            "delete_file: pre-delete backup (directory contents)",
            Some(op_id),
        ) {
            Ok(Some(id)) => {
                backup_ids.push(id);
                backed_up_paths.push(file_path.clone());
            }
            Ok(None) => {}
            Err(e) => {
                // Directory mutation has not started, so snapshots already
                // captured by this failed request must not enter undo history.
                discard_delete_backups(ctx, req.session(), op_id, &backed_up_paths);
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
        // Recursive removal may fail after deleting part of the tree. Keep
        // backups for missing files, but discard entries for files left intact.
        let not_deleted_paths = backed_up_paths
            .iter()
            .filter(|file_path| std::fs::symlink_metadata(file_path).is_ok())
            .cloned()
            .collect::<Vec<_>>();
        discard_delete_backups(ctx, req.session(), op_id, &not_deleted_paths);
        return Err(Response::error(
            &req.id,
            "io_error",
            format!(
                "delete_file: failed to remove directory '{}': {}",
                original, e
            ),
        ));
    }

    budget.files_left = budget.files_left.saturating_sub(collected.entries_counted);
    budget.bytes_left = budget.bytes_left.saturating_sub(collected.bytes_counted);

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

    let mut result = serde_json::json!({
        "file": original,
        "deleted": true,
        "is_directory": true,
        "files_deleted": files_to_backup.len(),
        "backup_ids": backup_ids,
    });
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), op_id, None);
    Ok(result)
}

fn discard_delete_backups(ctx: &AppContext, session: &str, op_id: &str, paths: &[PathBuf]) {
    let mut backup = ctx.backup().lock();
    for path in paths {
        backup.discard_latest_operation_entry_for_path(session, op_id, path);
    }
}

/// Guardrail for recursive deletes: the backup/undo format currently records
/// only file contents. Reject directory trees that contain entries undo cannot
/// restore atomically (symlinks and empty directories) before taking backups or
/// deleting anything.
fn validate_directory_for_recursive_delete(
    dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
) -> std::io::Result<Vec<String>> {
    let mut unsupported_paths = Vec::new();
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        unsupported_paths.push(dir.display().to_string());
        return Ok(unsupported_paths);
    }
    validate_directory_entries(dir, boundary, &mut unsupported_paths)?;
    Ok(unsupported_paths)
}

fn validate_directory_entries(
    dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
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
            if !boundary.should_descend(&path)? {
                // Report this as unsupported before mutation. Silently skipping it
                // would leave the mounted directory behind after partial deletion.
                unsupported_paths.push(path.display().to_string());
                continue;
            }
            validate_directory_entries(&path, boundary, unsupported_paths)?;
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
        "aft_delete with recursive: true does not yet support directory trees containing symlinks, empty directories, hard links, mounted directories from another filesystem, sockets, device nodes, or other non-regular files. Restore would not recover these entries atomically.",
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

/// How much one `delete_file` call may still copy into the undo store for
/// recursive directory deletes. Shared by every directory in a batch, because
/// the whole call holds the root's write lane while it copies.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecursiveDeleteBackupBudget {
    /// Files still allowed. Every non-directory entry counts, including
    /// entries later refused as unsupported, so the walk itself stays bounded.
    files_left: usize,
    /// Bytes still allowed, counting only files small enough to be copied.
    bytes_left: u64,
    /// The backup store's per-file limit; larger files are skipped by the
    /// store and so cost no copy.
    per_file_limit: Option<u64>,
    /// Whether backups are captured at all. With backups disabled by user
    /// config nothing is copied, so there is nothing to bound.
    enabled: bool,
}

impl RecursiveDeleteBackupBudget {
    pub(crate) fn new(
        policy: crate::backup::BackupPolicy,
        max_files: usize,
        max_bytes: u64,
    ) -> Self {
        Self {
            files_left: max_files,
            bytes_left: max_bytes,
            per_file_limit: policy.max_file_size,
            enabled: policy.enabled && policy.max_file_size != Some(0),
        }
    }

    fn for_request(ctx: &AppContext) -> Self {
        Self::new(
            ctx.backup().lock().policy(),
            crate::backup::RECURSIVE_DELETE_BACKUP_MAX_FILES,
            crate::backup::RECURSIVE_DELETE_BACKUP_MAX_BYTES,
        )
    }
}

/// Which budget a recursive delete walk ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetLimit {
    Files,
    Bytes,
}

/// A walk stopped because the tree needs more backup than the budget allows.
/// The counts are what the walk had seen when it stopped, so they are lower
/// bounds on the tree's real size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetExceeded {
    pub(crate) limit: BudgetLimit,
    pub(crate) files_counted: usize,
    pub(crate) bytes_counted: u64,
}

#[derive(Debug)]
pub(crate) enum CollectError {
    Io(std::io::Error),
    OverBudget(BudgetExceeded),
}

impl From<std::io::Error> for CollectError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Files a recursive delete will back up, with what they cost.
#[derive(Debug, Default)]
pub(crate) struct CollectedFiles {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) entries_counted: usize,
    pub(crate) bytes_counted: u64,
}

/// Walk a directory recursively, collecting all regular file paths, and stop
/// the moment the tree needs more backup than `budget` allows. The walk must
/// stop at the cap rather than count the whole tree first: a tree large enough
/// to refuse can also be large enough that counting it is itself slow.
///
/// Symlinks and other non-regular entries are counted but not collected; the
/// validation pass that follows refuses them. Directories on another
/// filesystem are not entered, and validation refuses them as well.
pub(crate) fn collect_files_within_budget(
    dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    budget: &RecursiveDeleteBackupBudget,
) -> Result<CollectedFiles, CollectError> {
    let mut collected = CollectedFiles::default();
    collect_files_into(dir, boundary, budget, &mut collected)?;
    Ok(collected)
}

fn collect_files_into(
    dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    budget: &RecursiveDeleteBackupBudget,
    out: &mut CollectedFiles,
) -> Result<(), CollectError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if boundary.should_descend(&path)? {
                collect_files_into(&path, boundary, budget, out)?;
            }
            continue;
        }

        out.entries_counted += 1;
        if budget.enabled && out.entries_counted > budget.files_left {
            return Err(CollectError::OverBudget(BudgetExceeded {
                limit: BudgetLimit::Files,
                files_counted: out.entries_counted,
                bytes_counted: out.bytes_counted,
            }));
        }
        if !file_type.is_file() {
            continue;
        }
        // `DirEntry::metadata` does not follow symlinks, and only regular
        // files reach this point.
        let len = entry.metadata()?.len();
        let copied = budget.per_file_limit.is_none_or(|limit| len <= limit);
        if copied {
            out.bytes_counted = out.bytes_counted.saturating_add(len);
            if budget.enabled && out.bytes_counted > budget.bytes_left {
                return Err(CollectError::OverBudget(BudgetExceeded {
                    limit: BudgetLimit::Bytes,
                    files_counted: out.entries_counted,
                    bytes_counted: out.bytes_counted,
                }));
            }
        }
        out.files.push(path);
    }
    Ok(())
}

fn format_mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn over_budget_response(
    req: &RawRequest,
    original: &str,
    exceeded: BudgetExceeded,
    budget: &RecursiveDeleteBackupBudget,
) -> Response {
    let max_files = crate::backup::RECURSIVE_DELETE_BACKUP_MAX_FILES;
    let max_bytes = crate::backup::RECURSIVE_DELETE_BACKUP_MAX_BYTES;
    // Earlier directories in the same batch call already spent part of the
    // budget; say so, or the numbers below would not add up for the caller.
    let used_files = max_files.saturating_sub(budget.files_left);
    let used_bytes = max_bytes.saturating_sub(budget.bytes_left);
    let counted = match exceeded.limit {
        BudgetLimit::Files => format!("at least {} files", exceeded.files_counted),
        BudgetLimit::Bytes => format!(
            "at least {} in {} files",
            format_mib(exceeded.bytes_counted),
            exceeded.files_counted
        ),
    };
    let earlier = if used_files > 0 || used_bytes > 0 {
        format!(
            " Earlier directories in this call already used {} files and {}.",
            used_files,
            format_mib(used_bytes)
        )
    } else {
        String::new()
    };
    Response::error_with_data(
        req.id.clone(),
        "recursive_delete_backup_too_large",
        format!(
            "delete_file: refusing to delete '{original}': its undo backup would copy {counted} \
             (limit per call: {max_files} files and {}); counting stopped at the limit.{earlier} \
             Nothing was deleted. Delete it in smaller pieces to keep undo, or, when no undo \
             is needed, remove it with bash `rm -rf`.",
            format_mib(max_bytes)
        ),
        serde_json::json!({
            "limit": match exceeded.limit {
                BudgetLimit::Files => "files",
                BudgetLimit::Bytes => "bytes",
            },
            "files_counted_at_least": exceeded.files_counted,
            "bytes_counted_at_least": exceeded.bytes_counted,
            "max_files": max_files,
            "max_bytes": max_bytes,
            "counting_stopped_early": true,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::BackupPolicy;
    use crate::walk_boundary::DeviceBoundary;

    fn write_tree(root: &Path, files: usize, bytes_each: usize) {
        for index in 0..files {
            let dir = root.join(format!("d{}", index / 50));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("f{index}")), vec![b'x'; bytes_each]).unwrap();
        }
    }

    fn collect(
        root: &Path,
        policy: BackupPolicy,
        max_files: usize,
        max_bytes: u64,
    ) -> Result<CollectedFiles, CollectError> {
        let boundary = DeviceBoundary::for_root(root).unwrap();
        let budget = RecursiveDeleteBackupBudget::new(policy, max_files, max_bytes);
        collect_files_within_budget(root, &boundary, &budget)
    }

    #[test]
    fn file_budget_stops_the_walk_one_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 1_500, 8);

        let Err(CollectError::OverBudget(exceeded)) =
            collect(dir.path(), BackupPolicy::default(), 1_000, u64::MAX)
        else {
            panic!("a 1,500-file tree must exceed a 1,000-file budget");
        };
        assert_eq!(exceeded.limit, BudgetLimit::Files);
        // Counting the whole tree before comparing would report 1,500 here.
        assert_eq!(exceeded.files_counted, 1_001);
    }

    #[test]
    fn byte_budget_counts_only_files_the_store_would_copy() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 200, 1_024);

        let Err(CollectError::OverBudget(exceeded)) =
            collect(dir.path(), BackupPolicy::default(), usize::MAX, 100 * 1_024)
        else {
            panic!("200 KiB of files must exceed a 100 KiB budget");
        };
        assert_eq!(exceeded.limit, BudgetLimit::Bytes);
        assert_eq!(exceeded.files_counted, 101);
        assert_eq!(exceeded.bytes_counted, 101 * 1_024);

        // With a per-file limit below every file's size the store copies
        // nothing, so the same tree fits a byte budget of zero.
        let small_files_only = BackupPolicy {
            max_file_size: Some(512),
            ..BackupPolicy::default()
        };
        let collected = collect(dir.path(), small_files_only, usize::MAX, 0).unwrap();
        assert_eq!(collected.files.len(), 200);
        assert_eq!(collected.bytes_counted, 0);
    }

    #[test]
    fn tree_within_budget_collects_every_file() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 120, 16);

        let collected = collect(dir.path(), BackupPolicy::default(), 120, 120 * 16).unwrap();
        assert_eq!(collected.files.len(), 120);
        assert_eq!(collected.entries_counted, 120);
        assert_eq!(collected.bytes_counted, 120 * 16);
    }

    #[test]
    fn disabled_backups_leave_the_walk_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 30, 16);

        let disabled = BackupPolicy {
            enabled: false,
            ..BackupPolicy::default()
        };
        let collected = collect(dir.path(), disabled, 1, 1).unwrap();
        assert_eq!(collected.files.len(), 30);
    }
}
