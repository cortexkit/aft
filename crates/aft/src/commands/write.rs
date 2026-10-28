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

    if super::github_comments::is_github_resource_path(file) {
        return super::github_comments::handle_comment_write(req, ctx, file, content);
    }

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
    let (original_bytes, observation) = if existed {
        match edit::read_edit_bytes(path.as_path()) {
            Ok((content, observation)) => (content, Some(observation)),
            Err(error) => {
                crate::slog_warn!(
                    "write: failed to read existing file before diff for {}: {}",
                    file,
                    error
                );
                return Response::error(&req.id, "io_error", error.to_string());
            }
        }
    } else {
        (std::sync::Arc::<[u8]>::from([]), None)
    };
    // Binary originals participate in identity and recovery, but cannot supply
    // an honest text-diff baseline.
    let original = std::str::from_utf8(&original_bytes).ok();

    if edit::wants_preview(&req.params) {
        let mut result = serde_json::json!({
            "file": file,
            "created": !existed,
            "formatted": false,
        });
        if existed && original_bytes.as_ref() == content.as_bytes() {
            result["no_op"] = serde_json::json!(true);
        }
        if let Some(original) = original {
            edit::attach_preview_diff(&mut result, &req.params, file, original, content);
        }
        return Response::success(&req.id, result);
    }

    if existed && original_bytes.as_ref() == content.as_bytes() {
        return edit::no_change_response(&req.id);
    }

    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);

    // Auto-backup existing files before overwriting. For create-only writes,
    // record a tombstone so operation undo removes the created file.
    let backup_id = if existed {
        let backup = if let Some(observation) = observation.as_ref() {
            edit::auto_backup_observed(
                ctx,
                req.session(),
                path.as_path(),
                "write: pre-write backup",
                Some(&op_id),
                observation,
            )
        } else {
            edit::auto_backup(
                ctx,
                req.session(),
                path.as_path(),
                "write: pre-write backup",
                Some(&op_id),
            )
        };
        match backup {
            Ok(id) => id,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        }
    } else {
        match ctx.backup().lock().snapshot_op_tombstone(
            req.session(),
            &op_id,
            path.as_path(),
            "write: file created by write",
        ) {
            Ok(id) => id,
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
                            .lock()
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
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let written = if let (Some(observation), Some(original)) = (observation.as_ref(), original) {
        edit::write_format_validate_observed(
            tool_policy,
            path.as_path(),
            content,
            &ctx.config(),
            &req.params,
            original,
            observation,
        )
    } else {
        edit::write_format_validate(
            tool_policy,
            path.as_path(),
            content,
            &ctx.config(),
            &req.params,
        )
    };
    let mut write_result = match written {
        Ok(r) => r,
        Err(e) => {
            if !existed {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if write_result.rolled_back {
        // A write takes one snapshot of this path. Do not spend rollback's
        // cleanup allowance discovering unrelated stacks in the session.
        if let Err(error) = ctx
            .backup()
            .lock()
            .try_discard_latest_operation_entry_for_path(req.session(), &op_id, path.as_path())
        {
            return Response::error_with_data(
                &req.id,
                error.code(),
                format!(
                    "write: file was rolled back, but undo cleanup failed; its backup may still be in undo history: {error}"
                ),
                serde_json::json!({
                    "file": file,
                    "rolled_back": true,
                    "undo_cleanup_failed": true,
                    "backup_id": backup_id,
                }),
            );
        }
    }

    if let Some(final_content) = write_result.final_content(path.as_path()) {
        let config_change_type = if existed {
            FileChangeType::CHANGED
        } else {
            FileChangeType::CREATED
        };
        ctx.lsp_notify_watched_config_file(path.as_path(), config_change_type);
        write_result.lsp_outcome = ctx.lsp_post_write(path.as_path(), final_content, &req.params);
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
    write_result.append_created_syntax_error_to(&mut result);

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
    edit::attach_backup_skipped_reason(
        &mut result,
        ctx,
        req.session(),
        &op_id,
        Some(path.as_path()),
    );

    write_result.append_lsp_diagnostics_to(&mut result);
    write_result.append_reformatted_excerpt_to(&mut result);

    // Read final on-disk content once for no_op detection + diff metadata.
    // Honest reporting: when the file existed AND the post-write content is
    // byte-identical to `original`, surface `no_op: true` so UIs can render
    // "wrote, but no net change" instead of a bare "File updated". See
    // GitHub #45.
    let final_content = write_result
        .final_content(path.as_path())
        .unwrap_or(content);
    if existed && original_bytes.as_ref() == final_content.as_bytes() {
        result["no_op"] = serde_json::json!(true);
    }

    if let Some(original) = original {
        edit::attach_edit_diff(&mut result, &req.params, file, original, final_content);
    }
    Response::success(&req.id, result)
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[test]
    fn backup_cancelled_rollback_restores_prior_success() {
        cancelled_rollback(RollbackCleanupSetup::Normal);
    }

    #[test]
    fn backup_cancelled_rollback_does_not_need_session_discovery() {
        cancelled_rollback(RollbackCleanupSetup::NoSessionDiscovery);
    }

    #[test]
    fn backup_cancelled_rollback_reports_cleanup_failure() {
        cancelled_rollback(RollbackCleanupSetup::DiskWriteFailure);
    }

    #[derive(Clone, Copy)]
    enum RollbackCleanupSetup {
        Normal,
        NoSessionDiscovery,
        DiskWriteFailure,
    }

    fn cancelled_rollback(setup: RollbackCleanupSetup) {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        for released in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cancelled.ts");
            let original = "const value = 0;\n";
            let successful = "const value = 1;\n";
            std::fs::write(&path, original).unwrap();
            let ctx = Arc::new(AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    format_on_edit: false,
                    ..Default::default()
                },
            ));
            ctx.backup()
                .lock()
                .set_storage_dir(dir.path().join("storage"), 72);
            let first: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "successful", "command": "write", "file": path, "content": successful,
            }))
            .unwrap();
            let first_response = handle_write(&first, &ctx);
            assert!(first_response.success, "{first_response:?}");
            let successful_backup_id = first_response.data["backup_id"].as_str().unwrap();
            ctx.backup().lock().record_post_mutation_states();
            if released {
                ctx.backup().lock().release_after_next_snapshot_for_tests();
            }
            let failed: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "cancelled", "command": "write", "file": path, "content": "const value = {;\n", "validate": "full",
            })).unwrap();
            let cancellation = crate::executor::JobCancellation::new();
            let guard = crate::executor::install_job_cancellation(cancellation.clone());
            let observed = Arc::new(AtomicBool::new(false));
            let hook_observed = Arc::clone(&observed);
            let hook_ctx = Arc::clone(&ctx);
            let hook_path = path.clone();
            let response = edit::with_full_validation_start_hook(
                Box::new(move || {
                    // Cancel at the real post-write checker boundary, after syntax
                    // rollback restored the file but before backup discard runs.
                    assert_eq!(std::fs::read_to_string(&hook_path).unwrap(), successful);
                    assert_eq!(
                        hook_ctx.backup().lock().resident_entry_count(),
                        if released { 0 } else { 2 }
                    );
                    match setup {
                        RollbackCleanupSetup::Normal => {}
                        RollbackCleanupSetup::NoSessionDiscovery => {
                            // Rollback knows the one path written. An unavailable
                            // session-wide scan must not leave its backup in undo.
                            hook_ctx
                                .backup()
                                .lock()
                                .set_discovery_entry_limit_for_tests(0);
                        }
                        RollbackCleanupSetup::DiskWriteFailure => {
                            hook_ctx.backup().lock().fail_next_disk_write_for_tests()
                        }
                    }
                    cancellation.request_cancel();
                    assert!(crate::executor::current_job_cancelled());
                    hook_observed.store(true, Ordering::Relaxed);
                }),
                || handle_write(&failed, &ctx),
            );
            assert!(
                observed.load(Ordering::Relaxed),
                "post-write checker hook did not run"
            );
            assert!(
                crate::executor::current_job_cancelled(),
                "cleanup must not clear the caller's cancellation"
            );
            drop(guard);
            if matches!(setup, RollbackCleanupSetup::DiskWriteFailure) {
                assert!(!response.success, "{response:?}");
                assert_eq!(response.data["rolled_back"], true, "{response:?}");
                assert_eq!(response.data["undo_cleanup_failed"], true, "{response:?}");
                assert!(response.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("injected backup disk write failure"));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), successful);
                continue;
            }
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["rolled_back"], true, "{response:?}");
            ctx.backup()
                .lock()
                .set_discovery_entry_limit_for_tests(usize::MAX);
            ctx.backup().lock().record_post_mutation_states();
            let restored = ctx
                .backup()
                .lock()
                .restore_last_operation(first.session())
                .unwrap();
            assert_eq!(restored.restored.len(), 1);
            assert_eq!(
                restored.restored[0].backup_id, successful_backup_id,
                "undo must select the successful write's backup, released={released}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "cancelled rollback must not leave its failed edit in undo"
            );
        }
    }

    #[test]
    fn backup_discard_after_session_release_keeps_previous_successful_undo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollback.ts");
        let original = "const value = 0;\n";
        let successful = "const value = 1;\n";
        std::fs::write(&path, original).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
        );
        ctx.backup()
            .lock()
            .set_storage_dir(dir.path().join("storage"), 72);
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "successful", "command": "write", "file": path, "content": successful,
        }))
        .unwrap();
        let response = handle_write(&request, &ctx);
        assert!(response.success, "{response:?}");
        ctx.backup().lock().record_post_mutation_states();
        ctx.backup().lock().release_after_next_snapshot_for_tests();

        let failed: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "failed", "command": "write", "file": path, "content": "const value = {;\n",
        }))
        .unwrap();
        let response = handle_write(&failed, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["rolled_back"], true, "{response:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), successful);
        ctx.backup().lock().record_post_mutation_states();
        ctx.backup()
            .lock()
            .restore_last_operation(request.session())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "undo must reverse the successful edit, not pop the rolled-back write"
        );
    }

    #[test]
    fn binary_write_reuses_observed_backup_and_restores_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.txt");
        let original = [0xff, 0xfe, b'\n'];
        std::fs::write(&path, original).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
        );
        crate::backup::reset_capture_read_count(&path);
        let request: RawRequest = serde_json::from_value(serde_json::json!({"id": "binary-undo", "command": "write", "file": path, "content": "after"})).unwrap();
        let response = handle_write(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        #[cfg(unix)]
        assert_eq!(crate::backup::capture_read_count(&path), 1);
        // DiskFileVersion only proves identity on Unix. Windows must read a
        // fresh backup rather than trust bytes captured without an identity token.
        #[cfg(windows)]
        assert_eq!(crate::backup::capture_read_count(&path), 2);
        let mut store = ctx.backup().lock();
        let history = store.history(request.session(), &path);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content_bytes().unwrap().as_ref(), original);
        store.record_post_mutation_states();
        store.restore_last_operation(request.session()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn binary_write_of_empty_content_is_not_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.txt");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({"id": "binary-write", "command": "write", "file": path, "content": ""})).unwrap();
        let response = handle_write(&request, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_ne!(response.data["no_op"], true);
        assert!(response.data.get("diff").is_none());
    }
}
