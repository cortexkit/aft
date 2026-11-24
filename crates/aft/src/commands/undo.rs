use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[test]
    fn edit_review_opus_9_undo_preview_probes_without_backup_mutex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, b"original").unwrap();
        let ctx = std::sync::Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        ctx.backup()
            .lock()
            .set_storage_dir(dir.path().join("backup"), 72);
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "preview-lock", "command": "undo_preview", "file": file,
        }))
        .unwrap();
        ctx.backup()
            .lock()
            .snapshot(req.session(), &file, "original")
            .unwrap();
        let checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _hook = install_authorization_hook_for_test(&req.id, {
            let ctx = ctx.clone();
            let checked = checked.clone();
            std::sync::Arc::new(move || {
                assert!(ctx.backup().try_lock().is_some());
                checked.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        });
        let response = handle_undo_preview(&req, &ctx);
        assert!(response.success, "{response:?}");
        assert!(checked.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn undo_filepath_alias_restores_only_requested_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request: RawRequest = serde_json::from_value(
            serde_json::json!({"id": "alias-undo", "command": "undo", "filePath": first}),
        )
        .unwrap();
        for path in [&first, &second] {
            std::fs::write(path, "before").unwrap();
            ctx.backup()
                .lock()
                .snapshot_with_op(request.session(), path, "before", Some("two-files"))
                .unwrap();
            std::fs::write(path, "after").unwrap();
        }
        ctx.backup().lock().record_post_mutation_states();
        let response = handle_undo(&request, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "before");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "after");
    }
}

/// Name for the checkpoint that preserves files changed outside AFT before
/// an undo overwrites them: `external-change-<file>-<nanoseconds>`, with
/// `-and-<n>-more` when an operation undo covers several files.
fn external_change_checkpoint_name(paths: &[PathBuf]) -> String {
    let file = paths
        .first()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let more = match paths.len() {
        0 | 1 => String::new(),
        count => format!("-and-{}-more", count - 1),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("external-change-{file}{more}-{nanos}")
}

/// Save `paths` as one session-scoped checkpoint. Every file must make it
/// in; a partial checkpoint would let undo overwrite the file left out.
fn save_external_change(
    ctx: &AppContext,
    session: &str,
    paths: &[PathBuf],
) -> Result<String, AftError> {
    let name = external_change_checkpoint_name(paths);
    let mut checkpoints = ctx.checkpoint().lock();
    let info = checkpoints.create_for_files(session, &name, paths.to_vec())?;
    if let Some((path, error)) = info.skipped.first() {
        checkpoints.delete(session, &name);
        return Err(AftError::IoError {
            path: path.display().to_string(),
            message: format!(
                "undo refused: the file changed outside AFT and could not be saved to a \
                 checkpoint before the restore: {error}"
            ),
        });
    }
    Ok(name)
}

/// How many times an operation undo re-plans after the operation changed
/// while its authorization was pending, before refusing.
const MAX_UNDO_PLAN_ATTEMPTS: usize = 3;

/// Authorize stored backup keys that an operation undo will write or remove.
///
/// Stored keys are just as untrusted as explicit tool arguments: authority
/// may have changed since the snapshot was taken. The restore works on the
/// stored keys themselves, so each must already be the absolute location the
/// validator checks; a relative one would be checked at the project root but
/// written relative to the daemon's working directory.
fn authorize_stored_keys(
    ctx: &AppContext,
    req_id: &str,
    paths: &[PathBuf],
) -> Result<(), Response> {
    #[cfg(test)]
    authorization_hook_for_test(req_id);
    for path in paths {
        if !path.is_absolute() {
            return Err(Response::error(
                req_id,
                "invalid_backup_key",
                format!(
                    "undo: stored backup path '{}' is not absolute; nothing was restored",
                    path.display()
                ),
            ));
        }
        ctx.validate_write_location(req_id, path)?;
    }
    Ok(())
}

/// A test hook run at the start of each authorization pass of the request
/// with this id, used to block an authorization or to change the backups
/// while one is pending.
#[cfg(test)]
type AuthorizationHook = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static AUTHORIZATION_HOOKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, AuthorizationHook>>,
> = std::sync::LazyLock::new(Default::default);

/// Install `hook` for the request with this id until the returned guard is
/// dropped.
#[cfg(test)]
pub(crate) fn install_authorization_hook_for_test(
    req_id: &str,
    hook: AuthorizationHook,
) -> impl Drop {
    struct Remove(String);
    impl Drop for Remove {
        fn drop(&mut self) {
            AUTHORIZATION_HOOKS.lock().unwrap().remove(&self.0);
        }
    }
    AUTHORIZATION_HOOKS
        .lock()
        .unwrap()
        .insert(req_id.to_string(), hook);
    Remove(req_id.to_string())
}

#[cfg(test)]
pub(super) fn authorization_hook_for_test(req_id: &str) {
    // Released before the hook runs: a hook that blocks must not stop other
    // requests from looking up their own.
    let hook = AUTHORIZATION_HOOKS.lock().unwrap().get(req_id).cloned();
    if let Some(hook) = hook {
        hook();
    }
}

/// Handle the `undo` command: restore the latest operation, or one file when requested.
///
/// Params: `file` (string, optional) — path to a single file to undo.
/// Returns: `{ path, backup_id }` on success, or `no_undo_history` error. When
/// the file had changed outside AFT, its content is first saved as a
/// checkpoint, named in `external_change_checkpoint` and in `warning`.
///
/// Authorization can run bounded filesystem probes or return a request for user
/// approval. Keep those probes outside the backup store and stack locks so
/// other edits and undos do not queue behind them. An operation undo plans under the locks,
/// releases them, authorizes the plan, then restores only if the plan is
/// still current, planning again (a bounded number of times) if not.
pub fn handle_undo(req: &RawRequest, ctx: &AppContext) -> Response {
    let session = req.session();
    let mut save = |paths: &[PathBuf]| save_external_change(ctx, session, paths);

    let Some(file) = req
        .params
        .get("file")
        .or_else(|| req.params.get("filePath"))
        .and_then(|v| v.as_str())
    else {
        if let Some(reason) = ctx
            .backup()
            .lock()
            .take_latest_skipped_reason_for_undo(req.session(), None)
        {
            return backup_skipped_response(req, reason);
        }
        for _ in 0..MAX_UNDO_PLAN_ATTEMPTS {
            let plan = match ctx.backup().lock().plan_last_operation(req.session()) {
                Ok(plan) => plan,
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            };
            if let Err(response) = authorize_stored_keys(ctx, &req.id, &plan.touched) {
                return response;
            }
            let _mutation = crate::runtime_drain::record_file_mutation(
                ctx,
                plan.touched.iter().map(|path| path.as_path()),
            );
            let restored =
                ctx.backup()
                    .lock()
                    .restore_planned_operation(req.session(), &plan, &mut save);
            match restored {
                // The operation changed while it was being authorized: that
                // decision was about something else, so plan and ask again.
                Ok(None) => continue,
                Ok(Some(operation)) => return operation_response(req, operation),
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            }
        }
        return Response::error_with_data(
            &req.id,
            "undo_plan_changed",
            format!(
                "undo: the latest operation kept changing while its permission was being checked ({MAX_UNDO_PLAN_ATTEMPTS} attempts); nothing was restored. Retry the undo."
            ),
            serde_json::json!({"retryable": true}),
        );
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

    let mut backup = ctx.backup().lock();
    if let Some(reason) =
        backup.take_latest_skipped_reason_for_undo(req.session(), Some(resolved.as_path()))
    {
        return backup_skipped_response(req, reason);
    }

    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [resolved.as_path()]);
    match backup.restore_latest_detailed(req.session(), &resolved, &mut save) {
        Ok(restored) => {
            let mut result = serde_json::json!({
                "path": file,
                "backup_id": restored.entry.backup_id,
            });
            if let Some(w) = restored.warning {
                result["warning"] = serde_json::Value::String(w);
            }
            if let Some(checkpoint) = restored.external_change_checkpoint {
                result["external_change_checkpoint"] = serde_json::Value::String(checkpoint);
            }
            Response::success(&req.id, result)
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn operation_response(req: &RawRequest, operation: crate::backup::RestoredOperation) -> Response {
    let mut result = serde_json::json!({
        "operation": true,
        "op_id": operation.op_id,
        "restored_count": operation.restored.len(),
        "restored": operation.restored.into_iter().map(|file| {
            let mut restored = serde_json::json!({
                "path": file.path.display().to_string(),
                "backup_id": file.backup_id,
            });
            if let Some(checkpoint) = file.external_change_checkpoint {
                restored["external_change_checkpoint"] =
                    serde_json::Value::String(checkpoint);
            }
            restored
        }).collect::<Vec<_>>(),
        "warnings": operation.warnings,
    });
    if let Some(checkpoint) = operation.external_change_checkpoint {
        result["external_change_checkpoint"] = serde_json::Value::String(checkpoint);
    }
    Response::success(&req.id, result)
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
    let path = match req
        .params
        .get("file")
        .or_else(|| req.params.get("filePath"))
        .and_then(|v| v.as_str())
    {
        Some(file) => {
            let input = ctx.resolve_relative_path(Path::new(file));
            #[cfg(test)]
            authorization_hook_for_test(&req.id);
            match ctx.validate_write_location(&req.id, &input) {
                Ok(path) => Some(path),
                Err(response) => return response,
            }
        }
        None => None,
    };

    let backup = ctx.backup().lock();
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
