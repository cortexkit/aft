//! Handler for the `delete_file` command: remove file(s) or directory with backup.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;
use serde_json::Value;

use crate::commands::delete_tree::{
    delete_recorded_tree, plan_file_backups, walk_tree, BudgetExceeded, BudgetLimit, CollectError,
    FileBackup, NodeKind, RecursiveDeleteBackupBudget, TreeEntry, TreeManifest, UnsupportedKind,
};
use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

#[cfg(test)]
#[derive(Default, Debug)]
struct DeleteOperations {
    entries: usize,
    notifications: usize,
    unlinks: usize,
}

#[cfg(test)]
thread_local! {
    static DELETE_OPERATIONS: std::cell::RefCell<DeleteOperations> = Default::default();
}

#[cfg(test)]
pub(crate) fn count_unlink_for_test() {
    DELETE_OPERATIONS.with(|ops| ops.borrow_mut().unlinks += 1);
}

#[cfg(test)]
type DeleteGate = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
static DELETE_GATES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, DeleteGate>>,
> = std::sync::LazyLock::new(Default::default);

#[cfg(test)]
pub(crate) fn install_delete_gate_for_test(
    path: &Path,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    DELETE_GATES
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (started_tx, release_rx));
    (started_rx, release_tx)
}

#[cfg(test)]
pub(crate) fn delete_gate_for_test(path: &Path) {
    let gate = DELETE_GATES.lock().unwrap().remove(path);
    if let Some((started, release)) = gate {
        let _ = started.send(());
        let _ = release.recv_timeout(std::time::Duration::from_secs(10));
    }
}

#[cfg(test)]
mod incident_tests {
    use super::*;

    fn context(root: &Path) -> AppContext {
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.to_path_buf());
        // Keep fixtures unbacked independently of the integration-only temp override.
        config.backup.enabled = Some(false);
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
        ctx
    }

    #[test]
    fn large_temp_delete_has_linear_operations() {
        let project = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let files = 30_000;
        for i in 0..files {
            let dir = tree.path().join(format!("d{}", i / 300));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("f{i}")), b"fixture").unwrap();
        }
        let ctx = context(project.path());
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "large-delete", "command": "delete_file",
            "file": tree.path(), "recursive": true
        }))
        .unwrap();
        DELETE_OPERATIONS.with(|ops| *ops.borrow_mut() = DeleteOperations::default());
        let started = std::time::Instant::now();
        let response = handle_delete_file(&req, &ctx);
        let elapsed = started.elapsed();
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["files_deleted"], files);
        DELETE_OPERATIONS.with(|ops| {
            let ops = ops.borrow();
            eprintln!(
                "large temp delete: {} entries, {:.3}s, {:.0} entries/s; {ops:?}",
                files + 101,
                elapsed.as_secs_f64(),
                (files + 101) as f64 / elapsed.as_secs_f64()
            );
            assert_eq!(
                ops.entries,
                files + 100,
                "preflight must visit each descendant once"
            );
            assert_eq!(
                ops.notifications, 0,
                "outside-root entries must not trigger per-file LSP work"
            );
            assert_eq!(
                ops.unlinks,
                files + 101,
                "one unlink per recorded entry, with no second enumeration"
            );
        });
    }

    fn delete_request(path: &Path) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "incident-delete", "command": "delete_file",
            "file": path, "recursive": true
        }))
        .unwrap()
    }

    fn bind_beside_delete(inside: bool) {
        use crate::executor::{Executor, Lane};
        use crate::path_identity::ProjectRootId;
        use crate::response_finalize::DispatchOutcome;
        use std::sync::Arc;
        use std::time::Duration;
        let project = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let tree = if inside {
            project.path().join("tree")
        } else {
            external.path().join("tree")
        };
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("leaf"), b"fixture").unwrap();
        let tree = std::fs::canonicalize(tree).unwrap();
        let ctx = Arc::new(context(project.path()));
        let root = ProjectRootId::from_path(project.path()).unwrap();
        let executor = Executor::new();
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started, release) = install_delete_gate_for_test(&tree);
        let (pending_tx, pending_rx) = std::sync::mpsc::sync_channel(1);
        let delete_ctx = Arc::clone(&ctx);
        let req = delete_request(&tree);
        let delete = executor.submit(
            root.clone(),
            Lane::Mutating,
            "subc-delete-incident".into(),
            Box::new(move |_| {
                match handle_delete_deferred_with_restriction(&req, delete_ctx, false) {
                    DispatchOutcome::Immediate(response) => response,
                    DispatchOutcome::Deferred(pending) => {
                        pending_tx.send(pending).unwrap();
                        Response::success(&req.id, serde_json::json!({"response_deferred": true}))
                    }
                }
            }),
        );
        started
            .recv_timeout(Duration::from_secs(3))
            .expect("real delete reached removal");
        let bind = executor.submit(
            root,
            Lane::Mutating,
            "subc-bind-beside-delete".into(),
            Box::new(|_| Response::success("bind", serde_json::json!({"bound": true}))),
        );
        let early = bind.recv_timeout(Duration::from_millis(300));
        // Release before asserting so a red control never strands the worker.
        release.send(()).unwrap();
        if inside {
            assert!(
                early.is_err(),
                "inside-root delete must keep the writer barrier"
            );
            assert!(bind.recv_timeout(Duration::from_secs(3)).unwrap().success);
        } else {
            assert!(
                early
                    .expect("outside-root delete must not hold the bind barrier")
                    .success
            );
            let mut pending = pending_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("external delete deferred");
            // Blocking here is only a test rendezvous: the gate has been released.
            assert!(delete.recv_timeout(Duration::from_secs(3)).unwrap().success);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(response) = (pending.poll)(&ctx) {
                    assert!(response.success, "{:?}", response.data);
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "detached delete did not finish"
                );
                std::thread::yield_now();
            }
        }
        assert!(!tree.exists());
    }

    #[test]
    fn bind_beside_outside_root_delete_proceeds() {
        bind_beside_delete(false);
    }

    #[test]
    fn bind_beside_inside_root_delete_waits() {
        bind_beside_delete(true);
    }

    #[test]
    fn cancelled_delete_stops_between_entries_and_reports_remaining() {
        let project = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::write(tree.path().join(format!("f{i}")), b"fixture").unwrap();
        }
        let ctx = context(project.path());
        let token = crate::executor::JobCancellation::new();
        let _guard = crate::executor::install_job_cancellation(token.clone());
        crate::commands::delete_tree::DELETE_OBSERVER.with(|observer| {
            *observer.borrow_mut() = Some(Box::new(move |deleted| {
                if deleted == 7 {
                    token.request_cancel();
                }
            }));
        });
        let response = handle_delete_file(&delete_request(tree.path()), &ctx);
        crate::commands::delete_tree::DELETE_OBSERVER
            .with(|observer| *observer.borrow_mut() = None);
        assert!(!response.success, "cancel must interrupt deletion");
        assert_eq!(response.data["code"], "request_cancelled");
        assert_eq!(
            response.data["files_deleted"], 7,
            "no more unlinks after Cancel checkpoint"
        );
        assert_eq!(response.data["directories_deleted"], 0);
        assert_eq!(response.data["remaining_entries"], 194); // 193 files and the root
        assert_eq!(std::fs::read_dir(tree.path()).unwrap().count(), 193);
        assert_eq!(
            response.data["remaining_root"],
            tree.path().display().to_string()
        );
        assert_eq!(response.data["complete"], false);
    }

    #[test]
    fn external_delete_admission_rejects_ancestors_mixed_batches_and_parent_links() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = parent.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let ctx = context(&root);
        assert!(external_unbacked_request(&delete_request(&outside), &ctx).is_some());
        assert!(external_unbacked_request(&delete_request(parent.path()), &ctx).is_none());
        let mut batch = delete_request(&outside);
        batch.params["files"] = serde_json::json!([outside, root]);
        assert!(external_unbacked_request(&batch, &ctx).is_none());
        #[cfg(unix)]
        {
            let link = parent.path().join("link");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            std::fs::write(root.join("leaf"), b"fixture").unwrap();
            assert!(external_unbacked_request(&delete_request(&link.join("leaf")), &ctx).is_none());
        }
    }
}

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
    handle_delete_file_with_skip(req, ctx, None)
}

fn handle_delete_file_with_skip(
    req: &RawRequest,
    ctx: &AppContext,
    forced_skip: Option<crate::backup::BackupSkippedReason>,
) -> Response {
    let op_id = crate::backup::new_op_id();
    let recursive = req
        .params
        .get("recursive")
        .and_then(crate::subc_translate::model_boolean)
        .unwrap_or(false);
    let mut budget = budget_for_request(ctx);

    let parsed_files = match req.params.get("files") {
        Some(Value::String(raw)) => match serde_json::from_str::<Vec<String>>(raw) {
            Ok(files) => Some(serde_json::json!(files)),
            Err(_) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "delete_file: 'files' must be an array of paths",
                )
            }
        },
        _ => None,
    };
    // Batch mode: `files: [...]`
    if let Some(files) = parsed_files
        .as_ref()
        .or_else(|| req.params.get("files"))
        .and_then(Value::as_array)
    {
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        for value in files {
            let Some(file) = value.as_str() else {
                skipped.push(serde_json::json!({"file": value, "reason": "not a string"}));
                continue;
            };
            match delete_one_or_dir(req, ctx, file, recursive, &op_id, &mut budget, forced_skip) {
                Ok(result) => deleted.push(result),
                Err(resp) => skipped.push(serde_json::json!({
                    "file": file,
                    "reason": resp.data.get("message").and_then(|v| v.as_str()).unwrap_or("delete failed"),
                    "details": resp.data,
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

    match delete_one_or_dir(req, ctx, file, recursive, &op_id, &mut budget, forced_skip) {
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
    forced_skip: Option<crate::backup::BackupSkippedReason>,
) -> Result<serde_json::Value, Response> {
    if crate::executor::current_job_cancelled() {
        return Err(Response::error_with_data(
            req.id.clone(),
            "request_cancelled",
            "delete_file: request cancelled before deleting this target",
            serde_json::json!({"file": file, "complete": false, "files_deleted": 0, "remaining_root": file}),
        ));
    }
    let path = match ctx.validate_write_location(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return Err(resp),
    };

    // Inspect the entry itself, never what a symlink points at: a link must be
    // treated as a link even when its target is a directory or is missing.
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Response::error(
                &req.id,
                "file_not_found",
                format!("delete_file: file not found: {}", file),
            ));
        }
        Err(e) => {
            return Err(Response::error(
                &req.id,
                "io_error",
                format!("delete_file: failed to inspect '{}': {}", file, e),
            ));
        }
    };
    let _view_intent = crate::views::intent::record_paths([path.as_path()]);
    let is_symlink = metadata.file_type().is_symlink();
    let is_dir = metadata.is_dir();
    let no_backup = forced_skip.or_else(|| no_backup_reason(ctx, &path, is_dir));

    if is_symlink && no_backup.is_none() {
        let unsupported = crate::commands::delete_tree::symlink_support(&path).map_err(|e| {
            Response::error(
                &req.id,
                "io_error",
                format!("delete_file: failed to read symlink '{}': {}", file, e),
            )
        })?;
        if let Some(kind) = unsupported {
            log_refusal("invalid_request", &path, &[(kind, 1)]);
            return Err(Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "delete_file: refusing to delete symlink '{}': {}",
                    file,
                    unsupported_symlink_reason(kind)
                ),
            ));
        }
    }

    if is_dir {
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
        return delete_directory(req, ctx, &path, file, op_id, budget, no_backup);
    }

    if let Some(reason) = no_backup {
        // Nothing here would be backed up, so there is no undo whose shape a
        // symlink, hard link, or special file could break: delete the entry
        // itself (never a link's target) and report why undo is unavailable.
        return delete_entry_without_backup(req, ctx, &path, file, op_id, reason);
    }

    let mut warnings = Vec::new();
    let backup_id = if is_symlink {
        // The symlink itself is backed up (its target text), never the file
        // it points at.
        edit::auto_backup(
            ctx,
            req.session(),
            &path,
            "delete_file: pre-delete backup",
            Some(op_id),
        )
    } else if metadata.is_file() {
        if file_link_count(&metadata) > 1 {
            // Because the other hard links are outside this delete, undo
            // restores this file as an independent copy that no longer shares
            // changes with those links.
            warnings.push(detached_hard_link_warning(file));
            ctx.backup().lock().snapshot_detached_hard_link_with_op(
                req.session(),
                &path,
                "delete_file: pre-delete backup",
                op_id,
            )
        } else {
            edit::auto_backup(
                ctx,
                req.session(),
                &path,
                "delete_file: pre-delete backup",
                Some(op_id),
            )
        }
    } else {
        match crate::commands::delete_tree::special_file_kind(&metadata.file_type()) {
            None => {
                warnings.push(socket_warning(file));
                Ok(None)
            }
            Some(kind) => {
                log_refusal("unsupported_directory_contents", &path, &[(kind, 1)]);
                return Err(Response::error(
                    &req.id,
                    "unsupported_directory_contents",
                    format!(
                        "delete_file: refusing to delete '{}': {}",
                        file,
                        unsupported_kind_reason(kind)
                    ),
                ));
            }
        }
    }
    .map_err(|e| Response::error(&req.id, e.code(), e.to_string()))?;

    // `remove_file` unlinks a symlink itself, never its target.
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
    if !warnings.is_empty() {
        result["warnings"] = serde_json::json!(warnings);
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

/// An external, unbacked delete needs no project epoch write gate: it cannot
/// change this root's files, indexes, config or undo snapshots. Validate and
/// resolve all locations under the admitted config before releasing the gate.
/// Ancestors of the root and mixed batches remain writers, as do external
/// backed-up deletes (their undo state still belongs to the actor).
pub(crate) fn handle_delete_deferred_with_restriction(
    req: &RawRequest,
    ctx: std::sync::Arc<AppContext>,
    force_restrict: bool,
) -> crate::response_finalize::DispatchOutcome {
    use crate::response_finalize::{DispatchOutcome, PendingResponse};
    let Some((request, reason)) = external_unbacked_request(req, &ctx) else {
        return DispatchOutcome::Immediate(handle_delete_file(req, &ctx));
    };
    let config = ctx.config();
    let cancellation = crate::executor::current_job_cancellation()
        .unwrap_or_else(crate::executor::JobCancellation::new);
    let worker_cancellation = cancellation.clone();
    let (tx, rx) = crate::response_finalize::pending_response_channel();
    std::thread::spawn(move || {
        let _config_pin = ctx.pin_config_to(config);
        let _cancellation = crate::executor::install_job_cancellation(worker_cancellation);
        let _restrict = force_restrict.then(|| ctx.force_restrict_guard(&request.id));
        // Preserve the admitted no-backup decision even if a concurrent bind
        // changes the policy; this continuation must never acquire snapshots.
        let response = handle_delete_file_with_skip(&request, &ctx, Some(reason));
        let _ = tx.send(response);
    });
    DispatchOutcome::Deferred(
        PendingResponse::from_receiver(
            req.id.clone(),
            req.session().to_string(),
            "delete".to_string(),
            rx,
            |_, response| response.ok(),
        )
        .with_cancellation(cancellation),
    )
}

fn outside_project_root(ctx: &AppContext, path: &Path) -> bool {
    let config = ctx.config();
    let Some(root) = config
        .project_root
        .as_ref()
        .and_then(|root| std::fs::canonicalize(root).ok())
    else {
        return false;
    };
    // Resolve ancestors, not the final symlink: that link is unlinked, never
    // followed. Fail closed if identity cannot be established.
    let Some(path) = resolved_delete_location(path) else {
        return false;
    };
    !path.starts_with(&root) && !root.starts_with(&path)
}

fn resolved_delete_location(path: &Path) -> Option<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Some(std::fs::canonicalize(parent).ok()?.join(path.file_name()?))
}

fn external_unbacked_request(
    req: &RawRequest,
    ctx: &AppContext,
) -> Option<(RawRequest, crate::backup::BackupSkippedReason)> {
    let files: Vec<String> = match req.params.get("files") {
        Some(Value::String(raw)) => serde_json::from_str(raw).ok()?,
        Some(value) => serde_json::from_value(value.clone()).ok()?,
        None => vec![req.params.get("file")?.as_str()?.to_owned()],
    };
    if files.is_empty() {
        return None;
    }
    let mut resolved = Vec::new();
    let mut reason = None;
    for file in files {
        let path = ctx
            .validate_write_location(&req.id, Path::new(&file))
            .ok()?;
        let path = resolved_delete_location(&path)?;
        if !outside_project_root(ctx, &path) {
            return None;
        }
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        let skip = no_backup_reason(ctx, &path, metadata.is_dir())?;
        if reason.is_some_and(|reason| reason != skip) {
            return None;
        }
        reason = Some(skip);
        resolved.push(path);
    }
    let mut params = req.params.clone();
    if req.params.get("files").is_some() {
        params["files"] = serde_json::json!(resolved);
    } else {
        params["file"] = serde_json::json!(resolved[0]);
    }
    Some((
        RawRequest {
            id: req.id.clone(),
            command: req.command.clone(),
            lsp_hints: req.lsp_hints.clone(),
            session_id: req.session_id.clone(),
            params,
        },
        reason?,
    ))
}

/// Why nothing at `path` would be backed up, if that holds for the whole
/// entry. A directory is judged by itself; any other entry by the directory
/// that holds it, because judging a symlink by its own path would resolve the
/// link and judge wherever it points instead.
fn no_backup_reason(
    ctx: &AppContext,
    path: &Path,
    is_dir: bool,
) -> Option<crate::backup::BackupSkippedReason> {
    let container = if is_dir {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    ctx.backup().lock().whole_tree_skip_reason(container)
}

/// Delete one non-directory entry that would not be backed up anyway.
fn delete_entry_without_backup(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    file: &str,
    op_id: &str,
    reason: crate::backup::BackupSkippedReason,
) -> Result<serde_json::Value, Response> {
    // `remove_file` unlinks a symlink itself, never its target.
    std::fs::remove_file(path).map_err(|e| {
        Response::error(
            &req.id,
            "io_error",
            format!("delete_file: failed to delete: {}", e),
        )
    })?;
    ctx.backup()
        .lock()
        .record_skipped_without_snapshot(req.session(), path, op_id, reason);
    if !outside_project_root(ctx, path) {
        ctx.lsp_notify_watched_config_file(path, FileChangeType::DELETED);
    }
    let mut result = serde_json::json!({
        "file": file,
        "deleted": true,
    });
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), op_id, Some(path));
    Ok(result)
}

#[cfg(unix)]
fn file_link_count(metadata: &std::fs::Metadata) -> u64 {
    metadata.nlink()
}

#[cfg(not(unix))]
fn file_link_count(_metadata: &std::fs::Metadata) -> u64 {
    1
}

fn detached_hard_link_warning(path: &str) -> String {
    format!(
        "{path}: this file was hard-linked to paths outside this delete; undo restores its content as an independent copy that no longer shares data with them"
    )
}

fn socket_warning(path: &str) -> String {
    format!(
        "{path}: socket deleted and not restorable by undo (a socket holds no data, and one recreated by undo would have no process listening on it)"
    )
}

/// Recursively delete a directory after backing up every entry inside.
///
/// The tree is walked once into a manifest, without following symlinks or
/// entering another filesystem. Every entry is backed up under the same
/// `op_id`, so a single `aft_safety undo` restores the whole tree: directories
/// (including empty ones, with their modes), file contents, hard links
/// (relinked), and symlinks (their exact target text, never the target).
/// Sockets are deleted with a warning that undo does not restore them. Mount
/// points, FIFOs, device nodes and symlinks undo cannot recreate exactly are
/// refused before anything is backed up or deleted.
fn delete_directory(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    original: &str,
    op_id: &str,
    budget: &mut RecursiveDeleteBackupBudget,
    no_backup: Option<crate::backup::BackupSkippedReason>,
) -> Result<serde_json::Value, Response> {
    // A vanished mounted child can make std::fs::ReadDir::drop panic after
    // closedir returns ENXIO, aborting the daemon. Capture the root device
    // before walking so the walk never crosses it.
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
    if let Some(reason) = no_backup {
        return delete_directory_without_backups(
            req, ctx, path, original, op_id, &boundary, reason,
        );
    }

    let manifest = match walk_tree(path, &boundary, budget) {
        Ok(manifest) => manifest,
        Err(CollectError::OverBudget(exceeded)) => {
            return Err(over_budget_response(req, original, exceeded, budget));
        }
        Err(CollectError::Io(e)) => {
            return Err(Response::error(
                &req.id,
                if e.kind() == std::io::ErrorKind::Interrupted {
                    "request_cancelled"
                } else {
                    "io_error"
                },
                format!(
                    "delete_file: failed to walk directory '{}': {}",
                    original, e
                ),
            ));
        }
    };
    if !manifest.unsupported.is_empty() {
        log_refusal(
            "unsupported_directory_contents",
            path,
            &manifest.unsupported_counts(),
        );
        return Err(Response::error(
            &req.id,
            "unsupported_directory_contents",
            unsupported_contents_message(&manifest),
        ));
    }

    let file_plan = plan_file_backups(&manifest);
    let mut warnings = Vec::new();
    let mut backup_ids: Vec<String> = Vec::new();
    let mut backed_up_paths: Vec<PathBuf> = Vec::new();
    let description = "delete_file: pre-delete backup (directory contents)";
    // Entries are backed up in walk order: each directory before its
    // contents, and each hard link after the path whose content it shares.
    for entry in &manifest.entries {
        if crate::executor::current_job_cancelled() {
            discard_delete_backups(ctx, req.session(), op_id, &backed_up_paths);
            return Err(Response::error_with_data(
                req.id.clone(),
                "request_cancelled",
                "delete_file: request cancelled during backup; nothing deleted",
                serde_json::json!({"complete": false, "files_deleted": 0, "remaining_root": original}),
            ));
        }
        let entry_path = entry.path.as_path();
        let display = entry_path.display().to_string();
        let snapshot = match entry.kind {
            NodeKind::Directory => ctx.backup().lock().snapshot_directory_with_op(
                req.session(),
                entry_path,
                description,
                op_id,
            ),
            NodeKind::Symlink => {
                edit::auto_backup(ctx, req.session(), entry_path, description, Some(op_id))
            }
            NodeKind::Socket => {
                warnings.push(socket_warning(&display));
                Ok(None)
            }
            NodeKind::Unbacked => unreachable!("backup manifests never contain unbacked leaves"),
            NodeKind::File { .. } => match file_plan.get(&entry.path) {
                Some(FileBackup::LinkTo(first)) => ctx.backup().lock().snapshot_hard_link_with_op(
                    req.session(),
                    entry_path,
                    first,
                    description,
                    op_id,
                ),
                Some(FileBackup::Content { detached: true }) => {
                    warnings.push(detached_hard_link_warning(&display));
                    ctx.backup().lock().snapshot_detached_hard_link_with_op(
                        req.session(),
                        entry_path,
                        description,
                        op_id,
                    )
                }
                _ => edit::auto_backup(ctx, req.session(), entry_path, description, Some(op_id)),
            },
        };
        match snapshot {
            Ok(Some(id)) => {
                backup_ids.push(id);
                backed_up_paths.push(entry.path.clone());
            }
            Ok(None) => {}
            Err(e) => {
                // Nothing has been deleted yet, so snapshots already captured
                // by this failed request must not enter undo history.
                discard_delete_backups(ctx, req.session(), op_id, &backed_up_paths);
                return Err(Response::error(
                    &req.id,
                    e.code(),
                    format!(
                        "delete_file: backup failed for '{}' inside '{}': {}",
                        display, original, e
                    ),
                ));
            }
        }
    }

    #[cfg(debug_assertions)]
    inject_entry_for_tests(path);

    if let Err(stopped) = delete_recorded_tree(&manifest) {
        // Keep backups for entries that are gone, so undo can bring them back,
        // and discard those for entries still present, so undo does not
        // overwrite them with an older copy.
        let not_deleted_paths = backed_up_paths
            .iter()
            .filter(|entry_path| std::fs::symlink_metadata(entry_path).is_ok())
            .cloned()
            .collect::<Vec<_>>();
        discard_delete_backups(ctx, req.session(), op_id, &not_deleted_paths);
        for entry in &manifest.entries {
            if entry.kind != NodeKind::Directory && std::fs::symlink_metadata(&entry.path).is_err()
            {
                ctx.lsp_notify_watched_config_file(&entry.path, FileChangeType::DELETED);
            }
        }
        crate::slog_warn!(
            "delete_file stopped recursive delete of '{}' partway at '{}': {}",
            original,
            stopped.path.display(),
            stopped.reason
        );
        return Err(Response::error_with_data(
            req.id.clone(),
            if stopped.cancelled { "request_cancelled" } else { "io_error" },
            format!(
                "delete_file: stopped deleting '{}' partway: could not remove '{}': {}. Entries already removed can be restored with undo; '{}' and whatever still holds it were left in place.",
                original,
                stopped.path.display(),
                stopped.reason,
                stopped.path.display()
            ),
            serde_json::json!({
                "partial": true,
                "complete": false,
                "files_deleted": stopped.files_deleted,
                "directories_deleted": stopped.directories_deleted,
                "remaining_entries": manifest.entries.len() - stopped.files_deleted - stopped.directories_deleted,
                "remaining_root": original,
                "stopped_at": stopped.path.display().to_string(),
            }),
        ));
    }

    budget.files_left = budget.files_left.saturating_sub(manifest.entries_counted);
    budget.bytes_left = budget.bytes_left.saturating_sub(manifest.bytes_counted);

    // Notify LSP for every entry that disappeared so watched-file diagnostics
    // refresh.
    let mut files_deleted = 0usize;
    let mut directories_deleted = 0usize;
    for entry in &manifest.entries {
        if entry.kind == NodeKind::Directory {
            directories_deleted += 1;
        } else {
            files_deleted += 1;
            ctx.lsp_notify_watched_config_file(&entry.path, FileChangeType::DELETED);
        }
    }

    log::debug!(
        "delete_file: recursively removed directory '{}' ({} file(s), {} directories)",
        original,
        files_deleted,
        directories_deleted
    );

    let mut result = serde_json::json!({
        "file": original,
        "deleted": true,
        "is_directory": true,
        "files_deleted": files_deleted,
        "directories_deleted": directories_deleted,
        "backup_ids": backup_ids,
    });
    if !warnings.is_empty() {
        result["warnings"] = serde_json::json!(warnings);
    }
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), op_id, None);
    Ok(result)
}

/// Test hook for the race between backup and removal: in debug builds, when
/// `AFT_TEST_RECURSIVE_DELETE_INJECT` names a path relative to the tree root,
/// create that file after the backups and before anything is removed.
#[cfg(debug_assertions)]
fn inject_entry_for_tests(root: &Path) {
    if let Some(relative) = std::env::var_os("AFT_TEST_RECURSIVE_DELETE_INJECT") {
        let _ = std::fs::write(root.join(relative), "created during the delete");
    }
}

fn discard_delete_backups(ctx: &AppContext, session: &str, op_id: &str, paths: &[PathBuf]) {
    let mut backup = ctx.backup().lock();
    for path in paths {
        backup.discard_latest_operation_entry_for_path(session, op_id, path);
    }
}

/// Recursively delete a directory none of whose entries would be backed up
/// (it is under a system temp directory, or backups are disabled).
///
/// With no undo to keep whole, symlinks, hard links, empty directories and
/// special files need no refusal and there is no copy to budget. A directory
/// on another filesystem is still refused: removing the tree would descend into
/// it and delete that filesystem's contents.
fn delete_directory_without_backups(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    original: &str,
    op_id: &str,
    boundary: &crate::walk_boundary::DeviceBoundary,
    reason: crate::backup::BackupSkippedReason,
) -> Result<serde_json::Value, Response> {
    let mut manifest = TreeManifest::default();
    manifest.entries.push(TreeEntry {
        path: path.to_path_buf(),
        kind: NodeKind::Directory,
    });
    let mut mounts = Vec::new();
    let mut visits = 0;
    collect_for_unbacked_delete(path, boundary, &mut manifest, &mut mounts, &mut visits).map_err(|e| {
        Response::error_with_data(
            req.id.clone(),
            if e.kind() == std::io::ErrorKind::Interrupted { "request_cancelled" } else { "io_error" },
            format!(
                "delete_file: failed to walk directory '{}': {}",
                original, e
            ),
            serde_json::json!({"complete": false, "files_deleted": 0, "remaining_root": original, "remaining_entries": null}),
        )
    })?;
    if !mounts.is_empty() {
        log_refusal(
            "unsupported_directory_contents",
            path,
            &[(UnsupportedKind::OtherFilesystem, mounts.len())],
        );
        return Err(Response::error(
            &req.id,
            "unsupported_directory_contents",
            other_filesystem_message(&mounts),
        ));
    }

    // Reuse preflight instead of enumerating the tree again in remove_dir_all.
    // Descriptor-relative removal never follows links and can stop between
    // entries, including for an explicit Cancel after some unlinks succeeded.
    let stopped = delete_recorded_tree(&manifest).err();
    ctx.backup()
        .lock()
        .record_skipped_without_snapshot(req.session(), path, op_id, reason);
    let files = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind != NodeKind::Directory)
        .count();
    let directories = manifest.entries.len() - files;
    // External temp trees do not belong to this actor's workspace. Sending
    // thousands of config probes (including global TS invalidations) for them
    // cannot refresh its indexes and needlessly competes for LSP/config locks.
    if !outside_project_root(ctx, path) {
        for entry in &manifest.entries {
            if entry.kind != NodeKind::Directory
                && (stopped.is_none() || std::fs::symlink_metadata(&entry.path).is_err())
            {
                #[cfg(test)]
                DELETE_OPERATIONS.with(|ops| ops.borrow_mut().notifications += 1);
                ctx.lsp_notify_watched_config_file(&entry.path, FileChangeType::DELETED);
            }
        }
    }
    if let Some(stopped) = stopped {
        crate::slog_warn!(
            "delete_file stopped unbacked delete root='{}' at='{}' cancelled={} files_deleted={} directories_deleted={} remaining_recorded_entries={}: {}",
            original, stopped.path.display(), stopped.cancelled, stopped.files_deleted,
            stopped.directories_deleted,
            manifest.entries.len() - stopped.files_deleted - stopped.directories_deleted,
            stopped.reason
        );
        return Err(Response::error_with_data(req.id.clone(),
            if stopped.cancelled { "request_cancelled" } else { "io_error" },
            format!("delete_file: stopped deleting '{original}' at '{}': {}. Removed {} file(s) and {} directories; {} recorded entries remain at '{original}'. No undo is available.",
                stopped.path.display(), stopped.reason, stopped.files_deleted, stopped.directories_deleted,
                manifest.entries.len() - stopped.files_deleted - stopped.directories_deleted),
            serde_json::json!({
                "file": original, "partial": stopped.files_deleted + stopped.directories_deleted > 0,
                "complete": false, "files_deleted": stopped.files_deleted,
                "directories_deleted": stopped.directories_deleted,
                "remaining_entries": manifest.entries.len() - stopped.files_deleted - stopped.directories_deleted,
                "remaining_root": original, "stopped_at": stopped.path,
                "backup_skipped_reason": reason.as_str(),
            })));
    }

    let mut result = serde_json::json!({
        "file": original,
        "deleted": true,
        "is_directory": true,
        "files_deleted": files,
        "directories_deleted": directories,
        "backup_ids": Vec::<String>::new(),
    });
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), op_id, None);
    Ok(result)
}

/// Collect every non-directory entry (for change notifications) and every
/// directory on another filesystem, without following symlinks.
fn collect_for_unbacked_delete(
    dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    manifest: &mut TreeManifest,
    mounts: &mut Vec<String>,
    visits: &mut usize,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        *visits += 1;
        if crate::executor::current_job_cancellation().is_some_and(|token| {
            token.cancel_already_requested()
                || (*visits % 64 == 1 && token.cancel_requested_before_commit())
        }) {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        }
        let entry = entry?;
        #[cfg(test)]
        DELETE_OPERATIONS.with(|ops| ops.borrow_mut().entries += 1);
        let path = entry.path();
        // `DirEntry::file_type` does not follow symlinks.
        if entry.file_type()?.is_dir() {
            if boundary.should_descend(&path)? {
                manifest.entries.push(TreeEntry {
                    path: path.clone(),
                    kind: NodeKind::Directory,
                });
                collect_for_unbacked_delete(&path, boundary, manifest, mounts, visits)?;
            } else {
                mounts.push(path.display().to_string());
            }
        } else {
            manifest.entries.push(TreeEntry {
                path,
                kind: NodeKind::Unbacked,
            });
        }
    }
    Ok(())
}

fn other_filesystem_message(paths: &[String]) -> String {
    let mut message = String::from(
        "aft_delete refuses to delete a directory tree that contains a mount point of another filesystem: removing the tree would delete that filesystem's contents. Unmount it first.",
    );
    append_offending_paths(&mut message, paths);
    message
}

fn append_offending_paths(message: &mut String, paths: &[String]) {
    const MAX_PATHS: usize = 5;
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
}

fn unsupported_contents_message(manifest: &TreeManifest) -> String {
    let mut kinds = manifest
        .unsupported_counts()
        .into_iter()
        .map(|(kind, _)| unsupported_kind_reason(kind))
        .collect::<Vec<_>>();
    kinds.dedup();
    let mut message = format!(
        "aft_delete with recursive: true refuses this directory tree because it contains entries undo cannot restore or that removing would reach beyond the tree: {}. Nothing was deleted.",
        kinds.join("; ")
    );
    let paths = manifest
        .unsupported
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect::<Vec<_>>();
    append_offending_paths(&mut message, &paths);
    message
}

/// Plain-language reason a kind of entry is refused.
fn unsupported_kind_reason(kind: UnsupportedKind) -> &'static str {
    match kind {
        UnsupportedKind::OtherFilesystem => {
            "a mount point of another filesystem (removing it would delete that filesystem's contents; unmount it first)"
        }
        UnsupportedKind::Fifo => "a named pipe (FIFO), which undo does not recreate",
        UnsupportedKind::Device => {
            "a device node, which undo could not recreate without special privileges"
        }
        UnsupportedKind::SymlinkNonUtf8Target | UnsupportedKind::WindowsSymlink => {
            "a symlink undo cannot recreate exactly"
        }
        UnsupportedKind::Other => "a special file undo cannot recreate",
    }
}

fn unsupported_symlink_reason(kind: UnsupportedKind) -> &'static str {
    match kind {
        UnsupportedKind::SymlinkNonUtf8Target => {
            "its target is not valid UTF-8, and undo could not recreate it exactly"
        }
        UnsupportedKind::WindowsSymlink => {
            "undo cannot yet recreate symlinks on Windows with their file or directory type"
        }
        other => unsupported_kind_reason(other),
    }
}

/// Log a refusal with its code and how many offending entries of each kind
/// it found, so refusals can be counted from the daemon log.
fn log_refusal(code: &str, target: &Path, counts: &[(UnsupportedKind, usize)]) {
    let counts = counts
        .iter()
        .map(|(kind, count)| format!("{}={}", kind.as_str(), count))
        .collect::<Vec<_>>()
        .join(" ");
    crate::slog_warn!(
        "delete_file refused '{}': code={} offending: {}",
        target.display(),
        code,
        counts
    );
}

fn budget_for_request(ctx: &AppContext) -> RecursiveDeleteBackupBudget {
    RecursiveDeleteBackupBudget::new(
        ctx.backup().lock().policy(),
        crate::backup::RECURSIVE_DELETE_BACKUP_MAX_FILES,
        crate::backup::RECURSIVE_DELETE_BACKUP_MAX_BYTES,
    )
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
        BudgetLimit::Files => format!("at least {} entries", exceeded.files_counted),
        BudgetLimit::Bytes => format!(
            "at least {} in {} entries",
            format_mib(exceeded.bytes_counted),
            exceeded.files_counted
        ),
    };
    let earlier = if used_files > 0 || used_bytes > 0 {
        format!(
            " Earlier directories in this call already used {} entries and {}.",
            used_files,
            format_mib(used_bytes)
        )
    } else {
        String::new()
    };
    crate::slog_warn!(
        "delete_file refused recursive delete of '{}': code=recursive_delete_backup_too_large limit={} entries_counted_at_least={} bytes_counted_at_least={}",
        original,
        match exceeded.limit {
            BudgetLimit::Files => "entries",
            BudgetLimit::Bytes => "bytes",
        },
        exceeded.files_counted,
        exceeded.bytes_counted
    );
    Response::error_with_data(
        req.id.clone(),
        "recursive_delete_backup_too_large",
        format!(
            "delete_file: refusing to delete '{original}': its undo backup would record {counted} \
             (limit per call: {max_files} entries, counting files, directories and links, and {}); \
             counting stopped at the limit.{earlier} \
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
