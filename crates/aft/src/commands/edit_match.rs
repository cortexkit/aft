//! Handler for the `edit_match` command: content-based string matching with
//! disambiguation for multiple occurrences.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::backup::CapturedRegularFile;
use crate::context::AppContext;
use crate::edit::{self, validate_syntax};
use crate::format;

use crate::protocol::{RawRequest, Response};

/// Handle an `edit_match` request.
///
/// Params:
///   - `file` (string, required) — target file path or glob pattern (e.g. `**/*.ts`)
///   - `match` (string, required, non-empty) — literal string to find
///   - `replacement` (string, required) — replacement content
///   - `occurrence` (integer, optional, 1-based) — select a specific occurrence (single-file only)
///   - `replace_all` (bool, optional) — replace all occurrences (default: false)
///   - `op` (string, optional) — when `append`, appends `append_content`/`appendContent`
///
/// When `file` is a glob pattern:
///   - Applies match/replace across all matching files
///   - `replace_all` is implicitly true
///   - `occurrence` is ignored
///   - Returns: `{ ok, files: [{ file, replacements, formatted, format_skipped_reason?, ... }], total_replacements, total_files, format_skipped_count, format_skip_reasons }`
///
/// When `file` is a literal path:
///   - Original single-file behavior
///   - Returns: `{ file, replacements: 1, syntax_valid?, backup_id? }`
///
/// `syntax_valid` is absent when syntax validation could not run.
pub fn handle_edit_match(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    if req.params.get("op").and_then(|v| v.as_str()) == Some("append") {
        return handle_append(req, ctx, &op_id);
    }

    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_match: missing required param 'file'",
            );
        }
    };

    let match_str = match req.params.get("match").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_match: missing required param 'match'",
            );
        }
    };

    if match_str.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "edit_match: 'match' must be a non-empty string",
        );
    }

    let replacement = match req.params.get("replacement").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_match: missing required param 'replacement'",
            );
        }
    };

    // No custom escape interpretation. JSON transport already handles escape
    // sequences before the string reaches us. Adding unescape_str on top caused
    // double-interpretation that corrupted source code with literal escapes.

    // Detect glob pattern. Prefer the literal interpretation when the path
    // already exists on disk, even if its name contains glob metacharacters
    // such as `[`, `]`, `*`, `?`, or `{`. This prevents files in directories
    // with brackets (e.g. `src/[another]/file.rs`) from being misclassified as
    // globs.
    if should_treat_as_glob(file, ctx) {
        return handle_glob_edit_match(req, ctx, file, match_str, replacement, &op_id);
    }

    // Single-file path
    handle_single_file_edit_match(req, ctx, file, match_str, replacement, &op_id)
}

fn handle_append(req: &RawRequest, ctx: &AppContext, op_id: &str) -> Response {
    let file = match req
        .params
        .get("file")
        .or_else(|| req.params.get("filePath"))
        .and_then(|v| v.as_str())
    {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_match append: missing required param 'file'",
            );
        }
    };

    let append_content = match req
        .params
        .get("append_content")
        .or_else(|| req.params.get("appendContent"))
        .and_then(|v| v.as_str())
    {
        Some(content) => content,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_match append: missing required param 'appendContent'",
            );
        }
    };

    let create_dirs = req
        .params
        .get("create_dirs")
        .or_else(|| req.params.get("createDirs"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let existed = path.exists();

    if edit::wants_preview(&req.params) {
        if !create_dirs {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    return Response::error(
                        &req.id,
                        "write_error",
                        format!(
                            "edit_match append: failed to open {}: parent directory does not exist",
                            file
                        ),
                    );
                }
            }
        }

        let before_content = if existed {
            std::fs::read_to_string(path.as_path()).unwrap_or_default()
        } else {
            String::new()
        };
        let final_content = format!("{}{}", before_content, append_content);
        let mut result = serde_json::json!({
            "ok": true,
            "file": file,
            "created": !existed,
            "bytes_written": append_content.len(),
            "formatted": false,
        });
        if existed && before_content == final_content {
            result["no_op"] = serde_json::json!(true);
        }
        edit::attach_preview_diff(
            &mut result,
            &req.params,
            file,
            &before_content,
            &final_content,
        );
        return Response::success(&req.id, result);
    }

    if create_dirs {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return Response::error(
                        &req.id,
                        "invalid_request",
                        format!("edit_match append: failed to create directories: {}", error),
                    );
                }
            }
        }
    }
    let backup_id = if existed {
        match edit::auto_backup(
            ctx,
            req.session(),
            path.as_path(),
            "edit_match: append",
            Some(op_id),
        ) {
            Ok(id) => id,
            Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
        }
    } else {
        match ctx.backup().lock().snapshot_op_tombstone(
            req.session(),
            op_id,
            path.as_path(),
            "edit_match append: file created by append",
        ) {
            Ok(id) => id,
            Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
        }
    };

    // Capture before-content for diff computation if requested. Only read it
    // when the caller asked, since this allocates the whole file string.
    let want_diff = edit::wants_diff(&req.params);
    let before_content = if want_diff && existed {
        std::fs::read_to_string(path.as_path()).unwrap_or_default()
    } else {
        String::new()
    };

    let mut file_handle = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.as_path())
    {
        Ok(file_handle) => file_handle,
        Err(error) => {
            if !existed {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), op_id);
            }
            return Response::error(
                &req.id,
                "write_error",
                format!("edit_match append: failed to open {}: {}", file, error),
            );
        }
    };

    if let Err(error) = file_handle.write_all(append_content.as_bytes()) {
        if !existed {
            ctx.backup()
                .lock()
                .discard_operation_entries(req.session(), op_id);
        }
        return Response::error(
            &req.id,
            "write_error",
            format!("edit_match append: failed to write {}: {}", file, error),
        );
    }

    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o644))
        {
            return Response::error(
                &req.id,
                "write_error",
                format!(
                    "edit_match append: failed to set permissions on {}: {}",
                    file, error
                ),
            );
        }
    }

    // Run the project formatter on the appended file. `auto_format` honors
    // `config.format_on_edit` internally and returns `(false, None)` when
    // disabled, so we can call it unconditionally. Bug #4 of the v0.18.3
    // format_on_edit audit: append previously hardcoded `formatted: false,
    // format_skipped_reason: None` and bypassed the formatter entirely.
    // Agents that appended messy lines kept them messy with no signal.
    let config = ctx.config();
    let (formatted, format_skipped_reason) = format::auto_format(path.as_path(), &config);
    drop(config);

    // Re-read final content AFTER formatting so the LSP sees the formatted
    // text (matches `write_format_validate` ordering: write → format → validate
    // → notify LSP) and the diff in the response reflects what's actually
    // on disk. Reading once and reusing for LSP + diff also avoids a TOCTOU
    // window where the formatter could rewrite the file between reads.
    let final_content = std::fs::read_to_string(path.as_path()).unwrap_or_default();

    // Honor `diagnostics: true` like other write-style handlers (write,
    // edit_match, edit_symbol). When false/absent, this still notifies the
    // LSP layer that the file changed but doesn't wait for diagnostics.
    let lsp_outcome = ctx.lsp_post_write(path.as_path(), &final_content, &req.params);
    let syntax_valid = match edit::validate_syntax(path.as_path()) {
        Ok(result) => result,
        Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
    };

    let mut result = serde_json::json!({
        "ok": true,
        "file": file,
        "created": !existed,
        "bytes_written": append_content.len(),
        "syntax_valid": syntax_valid,
        "formatted": formatted,
    });

    if let Some(reason) = &format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }
    edit::attach_backup_skipped_reason(
        &mut result,
        ctx,
        req.session(),
        op_id,
        Some(path.as_path()),
    );

    // Honest reporting: when file content is byte-identical to the pre-append
    // state (rare for append, but possible with empty appendContent or
    // formatter-normalized whitespace), surface `no_op: true` so UIs can
    // render a clear "matched but no net change" instead of bare +0/-0.
    // See GitHub #45.
    if existed && before_content == final_content {
        result["no_op"] = serde_json::json!(true);
    }

    if want_diff {
        // For new files, before-content is empty; compute_diff_info handles
        // that correctly (additions = number of lines in append_content).
        // Diff reflects post-format content because we re-read after format.
        result["diff"] =
            edit::compute_diff_for_response(&req.params, &before_content, &final_content);
    }

    // Reuse the standard WriteResult formatter so append's response carries
    // the same `lsp_diagnostics`, `lsp_complete`, `lsp_pending_servers`, and
    // `lsp_exited_servers` shape as `write` and `edit_match` find/replace.
    if lsp_outcome.is_some() {
        let write_result = edit::WriteResult {
            syntax_valid,
            formatted,
            format_skipped_reason: format_skipped_reason.clone(),
            validate_requested: false,
            validation_errors: Vec::new(),
            validate_skipped_reason: None,
            // Append-mode does not currently snapshot pre-write validity for
            // rollback (handled by the shared write pipeline only). Surface
            // false until/unless append gains the same rollback flow.
            rolled_back: false,
            lsp_outcome,
            reformatted_excerpt: edit::compute_reformatted_excerpt(
                &format!("{before_content}{append_content}"),
                &final_content,
            ),
        };
        write_result.append_lsp_diagnostics_to(&mut result);
        write_result.append_reformatted_excerpt_to(&mut result);
    }

    Response::success(&req.id, result)
}

/// Returns true if the file path contains glob characters.
fn is_glob_pattern(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('{') || path.contains('[')
}

/// Returns true when `file` should be treated as a glob pattern rather than a
/// literal path. A path that exists on disk is always treated literally, even
/// if its name contains glob metacharacters such as `[`, `]`, `*`, `?`, or `{`.
/// This defends against directories or files with brackets in their
/// names being misclassified as glob patterns (see issue #132).
///
/// Resolution mirrors `ctx.validate_path` so the literal-vs-glob decision stays
/// consistent with the single-file path handler's interpretation.
fn should_treat_as_glob(file: &str, ctx: &AppContext) -> bool {
    if !is_glob_pattern(file) {
        return false;
    }
    match ctx.validate_path("literal-check", Path::new(file)) {
        Ok(candidate) => !candidate.exists(),
        Err(resp)
            if resp.data.get("code").and_then(|c| c.as_str()) == Some("path_outside_root") =>
        {
            false
        }
        Err(resp) => {
            log::debug!(
                "edit_match: validate_path failed for '{}', deferring to single-file handler: {:?}",
                file,
                resp.data
            );
            false
        }
    }
}

/// Handle a glob-based multi-file edit_match.
fn handle_glob_edit_match(
    req: &RawRequest,
    ctx: &AppContext,
    pattern: &str,
    match_str: &str,
    replacement: &str,
    op_id: &str,
) -> Response {
    // Resolve glob relative to project root (or cwd)
    let config = ctx.config();
    let root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    drop(config);
    let full_pattern = if is_absolute_glob_pattern(pattern) {
        pattern.to_string()
    } else {
        root.join(pattern).display().to_string()
    };
    #[cfg(windows)]
    let full_pattern = full_pattern.replace('\\', "/");

    let mut paths: Vec<std::path::PathBuf> =
        match crate::walk_boundary::expand_glob_same_file_system(&full_pattern) {
            Ok(paths) => paths.into_iter().filter(|path| path.is_file()).collect(),
            Err(e) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("edit_match: invalid glob pattern: {}", e),
                );
            }
        };
    paths.sort();

    if paths.is_empty() {
        return Response::error(
            &req.id,
            "match_not_found",
            format!("edit_match: no files matched glob '{}'", pattern),
        );
    }

    let config = ctx.config();
    let mut file_results: Vec<serde_json::Value> = Vec::new();
    let mut total_replacements: usize = 0;
    let mut total_files: usize = 0;

    // --- Phase 1: Bulk edit — backup + write all files (fast) ---
    struct PendingEdit {
        path: std::path::PathBuf,
        file_str: String,
        original_source: String,
        new_source: String,
        capture: Option<CapturedRegularFile>,
        count: usize,
    }
    let mut pending: Vec<PendingEdit> = Vec::new();

    for path in &paths {
        let (source, capture) = match CapturedRegularFile::read_text(path) {
            Ok(Some((capture, source))) => (source, Some(capture)),
            Ok(None) => match std::fs::read_to_string(path) {
                Ok(source) => (source, None),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let positions: Vec<usize> = source
            .match_indices(match_str)
            .map(|(idx, _)| idx)
            .collect();

        if positions.is_empty() {
            continue;
        }

        let count = positions.len();
        let new_source = source.replace(match_str, replacement);
        let file_str = path.display().to_string();

        // Backup before mutation
        let validated_path = match validate_glob_edit_path(ctx, &req.id, path) {
            Ok(validated) => validated,
            Err(resp) => return resp,
        };

        pending.push(PendingEdit {
            path: validated_path,
            file_str,
            original_source: source,
            new_source,
            capture,
            count,
        });
        total_replacements += count;
        total_files += 1;
    }

    if pending.is_empty() {
        return Response::error(
            &req.id,
            "match_not_found",
            format!(
                "edit_match: '{}' not found in any files matching '{}'",
                match_str, pattern
            ),
        );
    }

    if edit::wants_preview(&req.params) {
        let mut preview_diff = String::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;
        let files = pending
            .iter()
            .map(|edit| {
                let diff = edit::compute_diff_info(&edit.original_source, &edit.new_source);
                additions += diff.get("additions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                deletions += diff.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                preview_diff.push_str(&edit::build_unified_diff(
                    &edit.file_str,
                    &edit.original_source,
                    &edit.new_source,
                ));
                if !preview_diff.ends_with('\n') {
                    preview_diff.push('\n');
                }
                serde_json::json!({
                    "file": edit.file_str,
                    "replacements": edit.count,
                    "diff": diff,
                })
            })
            .collect::<Vec<_>>();

        return Response::success(
            &req.id,
            serde_json::json!({
                "ok": true,
                "preview": true,
                "files": files,
                "total_replacements": total_replacements,
                "total_files": total_files,
                "diff": {
                    "additions": additions,
                    "deletions": deletions,
                },
                "preview_diff": preview_diff,
            }),
        );
    }

    let mut captures = pending
        .iter()
        .filter_map(|edit| {
            edit.capture
                .clone()
                .map(|capture| (edit.path.clone(), capture))
        })
        .collect::<HashMap<_, _>>();
    let checkpoint_name = {
        let name = unique_glob_checkpoint_name(&req.id);
        let files = pending
            .iter()
            .map(|edit| edit.path.clone())
            .collect::<Vec<_>>();
        let checkpoint_result = {
            let backup = ctx.backup().lock();
            ctx.checkpoint().lock().create_from_captures(
                req.session(),
                &name,
                files,
                &backup,
                &mut captures,
            )
        };
        if let Err(e) = checkpoint_result {
            return Response::error(&req.id, e.code(), e.to_string());
        }
        Some(name)
    };

    for edit in &pending {
        let description = format!("glob_edit_match: {}", match_str);
        let backup_result = if let Some(capture) = captures.get(&edit.path) {
            edit::auto_backup_from_capture(
                ctx,
                req.session(),
                &edit.path,
                &description,
                Some(op_id),
                capture,
            )
        } else {
            edit::auto_backup(ctx, req.session(), &edit.path, &description, Some(op_id))
        };
        if let Err(e) = backup_result {
            if let Some(name) = &checkpoint_name {
                delete_glob_checkpoint(ctx, req.session(), name);
            }
            return Response::error(&req.id, e.code(), e.to_string());
        }
    }

    // Write all changed files under a checkpoint-backed transaction. If any
    // write fails, restore files already written so callers never observe a
    // partially-applied glob edit.
    let mut written_paths: Vec<PathBuf> = Vec::new();

    for edit in &pending {
        if let Err(e) = std::fs::write(&edit.path, &edit.new_source) {
            let mut rollback_ok = true;
            if let Some(name) = &checkpoint_name {
                rollback_ok =
                    restore_glob_checkpoint(ctx, req.session(), name, &written_paths).is_ok();
                delete_glob_checkpoint(ctx, req.session(), name);
            }
            if let Err(rollback_error) = std::fs::write(&edit.path, &edit.original_source) {
                crate::slog_warn!(
                    "glob edit_match rollback: failed to restore attempted file {}: {}",
                    edit.path.display(),
                    rollback_error
                );
                rollback_ok = false;
            }
            if rollback_ok {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), op_id);
            }
            return Response::error(
                &req.id,
                "write_error",
                format!("failed to write {}: {}", edit.file_str, e),
            );
        }
        written_paths.push(edit.path.clone());
    }

    // --- Phase 2: Format all changed files (after all writes are done) ---
    //
    // Atomicity rule for glob edit_match: if ANY file ends up syntax-invalid
    // after the replacement+format pass, we restore the entire batch from the
    // pre-edit checkpoint and return an error. The agent then sees a clear
    // "no files changed because the replacement would have broken N file(s)"
    // signal and can revise the replacement instead of being left with a
    // partially-applied glob and a per-file `syntax_valid: false` they may
    // miss. Single-file `edit_match` deliberately keeps the per-file syntax
    // honesty (the agent has full visibility on one file); the multi-file
    // glob path makes silent partial breakage too easy.
    let mut syntax_failures: Vec<String> = Vec::new();
    let mut format_skipped_count: usize = 0;
    let mut format_skip_reasons = std::collections::BTreeSet::new();
    for edit in &pending {
        let file_str = edit.path.display().to_string();
        let (formatted, format_skipped_reason) = format::auto_format(&edit.path, &config);
        if let Some(reason) = &format_skipped_reason {
            format_skipped_count += 1;
            format_skip_reasons.insert(reason.clone());
        }
        let syntax_valid = match validate_syntax(&edit.path) {
            Ok(valid) => valid,
            Err(e) => {
                if let Some(name) = &checkpoint_name {
                    let paths = pending
                        .iter()
                        .map(|edit| edit.path.clone())
                        .collect::<Vec<_>>();
                    if restore_glob_checkpoint(ctx, req.session(), name, &paths).is_ok() {
                        ctx.backup()
                            .lock()
                            .discard_operation_entries(req.session(), op_id);
                    }
                    delete_glob_checkpoint(ctx, req.session(), name);
                }
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

        if syntax_valid == Some(false) {
            syntax_failures.push(file_str.clone());
        }

        if let Ok(final_content) = std::fs::read_to_string(&edit.path) {
            ctx.lsp_notify_file_changed(&edit.path, &final_content);
        }

        let mut file_result = serde_json::json!({
            "file": file_str,
            "replacements": edit.count,
            "formatted": formatted,
            "syntax_valid": syntax_valid,
        });
        if let Some(reason) = format_skipped_reason {
            file_result["format_skipped_reason"] = serde_json::json!(reason);
        }
        file_results.push(file_result);
    }

    // If any file's post-edit content is syntax-invalid, roll the entire
    // batch back to the pre-edit checkpoint. Don't leave the project in a
    // partially-broken state across many files at once.
    if !syntax_failures.is_empty() {
        let mut rollback: Option<Result<(), String>> = None;
        if let Some(name) = &checkpoint_name {
            let paths = pending
                .iter()
                .map(|edit| edit.path.clone())
                .collect::<Vec<_>>();
            rollback = Some(restore_glob_checkpoint(ctx, req.session(), name, &paths));
            delete_glob_checkpoint(ctx, req.session(), name);
            // Re-notify LSP so any cached diagnostics for the rolled-back
            // files reflect the restored content, not the broken edits.
            for path in &paths {
                if let Ok(restored) = std::fs::read_to_string(path) {
                    ctx.lsp_notify_file_changed(path, &restored);
                }
            }
        }
        let summary = if syntax_failures.len() <= 5 {
            syntax_failures.join(", ")
        } else {
            format!(
                "{} (+{} more)",
                syntax_failures[..5].join(", "),
                syntax_failures.len() - 5
            )
        };
        if rollback.as_ref().map_or(true, |result| result.is_ok()) {
            ctx.backup()
                .lock()
                .discard_operation_entries(req.session(), op_id);
        }
        return match rollback {
            Some(Err(reason)) => Response::error_with_data(
                &req.id,
                "syntax_invalid",
                format!(
                    "edit_match (glob): replacement would leave {} of {} file(s) syntax-invalid; rollback FAILED: {}. Files may be in inconsistent state. Affected: {}",
                    syntax_failures.len(),
                    pending.len(),
                    reason,
                    summary
                ),
                serde_json::json!({ "rollback_succeeded": false }),
            ),
            _ => Response::error_with_data(
                &req.id,
                "syntax_invalid",
                format!(
                    "edit_match (glob): replacement would leave {} of {} file(s) syntax-invalid; rolled back. Affected: {}",
                    syntax_failures.len(),
                    pending.len(),
                    summary
                ),
                serde_json::json!({ "rollback_succeeded": true }),
            ),
        };
    }

    if let Some(name) = &checkpoint_name {
        delete_glob_checkpoint(ctx, req.session(), name);
    }

    log::debug!(
        "edit_match (glob): {} replacements across {} files",
        total_replacements,
        total_files
    );

    // Top-level format summary lets agents notice actionable glob formatting
    // skips (for example formatter_excluded_path) without scanning every file.
    let format_skip_reasons = format_skip_reasons.into_iter().collect::<Vec<_>>();

    let mut result = serde_json::json!({
        "ok": true,
        "files": file_results,
        "total_replacements": total_replacements,
        "total_files": total_files,
        "format_skipped_count": format_skipped_count,
        "format_skip_reasons": format_skip_reasons,
    });
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), op_id, None);
    Response::success(&req.id, result)
}

#[cfg(windows)]
fn is_absolute_glob_pattern(path: &str) -> bool {
    let bytes = path.as_bytes();
    Path::new(path).is_absolute()
        || (bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

#[cfg(not(windows))]
fn is_absolute_glob_pattern(path: &str) -> bool {
    Path::new(path).is_absolute()
}

fn unique_glob_checkpoint_name(request_id: &str) -> String {
    unique_glob_checkpoint_name_with_timestamp(request_id, current_timestamp_nanos())
}

fn current_timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn unique_glob_checkpoint_name_with_timestamp(request_id: &str, timestamp_nanos: u128) -> String {
    format!(
        "__glob_edit_match_{}_{}",
        sanitize_checkpoint_component(request_id),
        timestamp_nanos
    )
}

fn sanitize_checkpoint_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod checkpoint_name_tests {
    use super::unique_glob_checkpoint_name_with_timestamp;

    #[test]
    fn glob_checkpoint_name_includes_request_id() {
        let timestamp = 123_456;
        let first = unique_glob_checkpoint_name_with_timestamp("request-a", timestamp);
        let second = unique_glob_checkpoint_name_with_timestamp("request-b", timestamp);

        assert_ne!(first, second);
        assert_eq!(first, "__glob_edit_match_request-a_123456");
    }
}

fn restore_glob_checkpoint(
    ctx: &AppContext,
    session: &str,
    name: &str,
    paths: &[PathBuf],
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    match ctx
        .checkpoint()
        .lock()
        .restore_validated(session, name, paths)
    {
        Ok(_) => Ok(()),
        Err(e) => {
            crate::slog_warn!(
                "edit_match glob rollback: failed to restore checkpoint {}: {}",
                name,
                e
            );
            Err(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::Config;
    use crate::context::AppContext;
    use crate::language::StubProvider;
    use crate::protocol::RawRequest;

    use super::{handle_edit_match, restore_glob_checkpoint};

    #[test]
    fn glob_edit_reads_each_pre_edit_file_once_and_preserves_backup_bytes() {
        let cache = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");
        fs::write(&first, "first TARGET\n").unwrap();
        fs::write(&second, "second TARGET\n").unwrap();
        crate::backup::reset_capture_read_count(&first);
        crate::backup::reset_capture_read_count(&second);

        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        ctx.checkpoint()
            .lock()
            .set_lock_path_for_test(cache.path().join("checkpoint.lock"));
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "single-capture-glob",
            "command": "edit_match",
            "file": temp.path().join("*.txt").display().to_string(),
            "match": "TARGET",
            "replacement": "DONE",
            "replace_all": true
        }))
        .unwrap();

        let response = handle_edit_match(&request, &ctx);
        assert!(response.success, "glob edit failed: {:?}", response.data);
        assert_eq!(crate::backup::capture_read_count(&first), 1);
        assert_eq!(crate::backup::capture_read_count(&second), 1);

        let backup = ctx.backup().lock();
        assert_eq!(
            backup.history(request.session(), &first)[0]
                .content_bytes
                .as_ref(),
            b"first TARGET\n"
        );
        assert_eq!(
            backup.history(request.session(), &second)[0]
                .content_bytes
                .as_ref(),
            b"second TARGET\n"
        );
    }

    #[test]
    fn restore_glob_checkpoint_reports_failures() {
        // Isolate the checkpoint store's lock-file dir with a per-test
        // configured storage_dir. This used to mutate the process-global
        // AFT_CACHE_DIR env var instead, which raced parallel lib tests:
        // resolve_manifest_dir prefers AFT_CACHE_DIR over configured storage,
        // so a configure running in another test during this test's window
        // looked for its artifact-owner manifest in OUR tempdir, found
        // nothing, and claimed Owner where ReadOnly was expected (the
        // Windows-CI sibling_clone flake).
        let cache = tempfile::tempdir().unwrap();

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let a = root.join("a.ts");
        let b = root.join("b.ts");
        fs::write(&a, "const a = TARGET;\n").unwrap();

        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        ctx.checkpoint()
            .lock()
            .set_lock_path_for_test(cache.path().join("checkpoint.lock"));
        let backup = ctx.backup().lock();
        let checkpoint_name = ctx
            .checkpoint()
            .lock()
            .create(
                "default",
                "__edit_match_glob_missing_path__",
                vec![a.clone()],
                &backup,
            )
            .unwrap()
            .name;
        drop(backup);

        let result = restore_glob_checkpoint(&ctx, "default", &checkpoint_name, &[a, b]);
        ctx.checkpoint().lock().delete("default", &checkpoint_name);

        assert!(result.unwrap_err().contains("file not found"));
    }
}

fn delete_glob_checkpoint(ctx: &AppContext, session: &str, name: &str) {
    ctx.checkpoint().lock().delete(session, name);
}

fn validate_glob_edit_path(
    ctx: &AppContext,
    req_id: &str,
    path: &Path,
) -> Result<std::path::PathBuf, Response> {
    ctx.validate_path(req_id, path)
}

/// Fuzzy line matches include the final newline even when the needle omits it.
/// Restore that separator so replacing a line block cannot merge the next line.
fn fuzzy_replacement_restores_newline(
    source: &str,
    matched: &crate::fuzzy_match::FuzzyMatch,
    replacement: &str,
) -> bool {
    let byte_end = matched.byte_start.saturating_add(matched.byte_len);
    matched.pass >= 2
        && byte_end > 0
        && byte_end <= source.len()
        && source.as_bytes()[byte_end - 1] == b'\n'
        && !replacement.is_empty()
        && !replacement.ends_with('\n')
}

fn push_fuzzy_replacement(
    output: &mut String,
    source: &str,
    matched: &crate::fuzzy_match::FuzzyMatch,
    replacement: &str,
) {
    output.push_str(replacement);
    if fuzzy_replacement_restores_newline(source, matched, replacement) {
        output.push('\n');
    }
}

fn apply_sorted_non_overlapping_fuzzy_matches(
    source: &str,
    matches: &[crate::fuzzy_match::FuzzyMatch],
    replacement: &str,
) -> Result<String, crate::error::AftError> {
    let mut removed_bytes = 0usize;
    let mut replacement_bytes = 0usize;
    for matched in matches {
        let byte_end = matched.byte_start.saturating_add(matched.byte_len);
        edit::validate_byte_range(source, matched.byte_start, byte_end)?;
        removed_bytes = removed_bytes.saturating_add(matched.byte_len);
        replacement_bytes = replacement_bytes.saturating_add(replacement.len());
        if fuzzy_replacement_restores_newline(source, matched, replacement) {
            replacement_bytes = replacement_bytes.saturating_add(1);
        }
    }

    // The matches are offsets into the original source. Copying untouched spans
    // forward keeps those offsets stable while visiting every source byte once.
    let capacity = source
        .len()
        .saturating_sub(removed_bytes)
        .saturating_add(replacement_bytes);
    let mut result = String::with_capacity(capacity);
    let mut copied_through = 0usize;
    for matched in matches {
        result.push_str(&source[copied_through..matched.byte_start]);
        push_fuzzy_replacement(&mut result, source, matched, replacement);
        copied_through = matched.byte_start + matched.byte_len;
    }
    result.push_str(&source[copied_through..]);
    Ok(result)
}

/// Handle a single-file edit_match (original behavior).
fn handle_single_file_edit_match(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    match_str: &str,
    replacement: &str,
    op_id: &str,
) -> Response {
    let raw_occurrence = req.params.get("occurrence");

    let replace_all = req
        .params
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if replace_all && raw_occurrence.is_some() {
        return Response::error(
            &req.id,
            "invalid_request",
            "edit_match: 'replaceAll' and 'occurrence' are mutually exclusive",
        );
    }

    let occurrence = match raw_occurrence {
        None => None,
        Some(value) => match value.as_u64() {
            Some(0) | None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "edit_match: 'occurrence' must be a positive integer (1-based)",
                );
            }
            // Width-independent bound: compare in u64 before converting, so the
            // full contract domain stays valid regardless of target usize width.
            Some(value) if value - 1 <= usize::MAX as u64 => Some((value - 1) as usize),
            Some(_) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "edit_match: 'occurrence' exceeds the supported range",
                );
            }
        },
    };

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
        }
    };

    // Find all positions using progressive fuzzy matching:
    // Pass 1: exact, Pass 2: rstrip, Pass 3: trim, Pass 4: normalized Unicode
    let fuzzy_matches = crate::fuzzy_match::find_all_fuzzy(&source, match_str);

    if fuzzy_matches.is_empty() {
        return Response::error(
            &req.id,
            "match_not_found",
            format!(
                "edit_match: '{}' not found in {}{}",
                match_str,
                file,
                crate::fuzzy_match::render_nearest_miss_detail(&source, match_str),
            ),
        );
    }

    // Log if fuzzy match was needed (not exact)
    if fuzzy_matches[0].pass > 1 {
        log::debug!(
            "edit_match: fuzzy match (pass {}) for '{}' in {}",
            fuzzy_matches[0].pass,
            match_str,
            file
        );
    }

    let positions: Vec<usize> = fuzzy_matches.iter().map(|m| m.byte_start).collect();

    // If occurrence specified but out of range (only relevant when not replace_all).
    if !replace_all {
        if let Some(occ) = occurrence {
            if occ >= positions.len() {
                let hint = format!(
                    " 'occurrence' is 1-based (valid range: 1-{}).",
                    positions.len()
                );
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "edit_match: occurrence {} out of range, file has {} occurrence(s).{}",
                        occ + 1,
                        positions.len(),
                        hint
                    ),
                );
            }
        }
    }

    // Multiple matches without occurrence selector → disambiguation (unless replace_all)
    if positions.len() > 1 && occurrence.is_none() && !replace_all {
        let occurrences: Vec<serde_json::Value> = positions
            .iter()
            .enumerate()
            .map(|(idx, &byte_pos)| {
                let line = source[..byte_pos].matches('\n').count();
                let context = build_context(&source, line, 2);
                serde_json::json!({
                    "occurrence": idx + 1,
                    "line": line + 1,
                    "context": context,
                })
            })
            .collect();

        return Response::error_with_data(
            &req.id,
            "ambiguous_match",
            format!(
                "Found {} matches. Use 'occurrence' (1-based) to select one, or 'replaceAll: true' to replace all.{}",
                occurrences.len(),
                crate::fuzzy_match::render_occurrence_listing(&source, &positions),
            ),
            serde_json::json!({
                "occurrences": occurrences,
            }),
        );
    }

    // Apply edit(s) — use fuzzy match byte lengths (may differ from match_str.len()).
    let (new_source, count) = if replace_all {
        // Guard against overlapping matches before applying. The fuzzy line
        // passes (2-4) step line-by-line, so a multi-line needle can match
        // overlapping regions (e.g. needle "a\na" over "a\na\na" when whitespace
        // variants defeat the exact pass). Applying overlapping ranges would
        // silently corrupt the file. Fail cleanly instead — mirrors the `batch`
        // command's overlap guard. (Matches are ascending by byte_start.)
        for pair in fuzzy_matches.windows(2) {
            let cur_end = pair[0].byte_start + pair[0].byte_len;
            if cur_end > pair[1].byte_start {
                return Response::error(
                    &req.id,
                    "overlapping_edits",
                    format!(
                        "edit: replace_all matches overlap — match at bytes [{}..{}) overlaps with match at bytes [{}..{}). Use a more specific 'match' or edit occurrences individually.",
                        pair[0].byte_start, cur_end, pair[1].byte_start, pair[1].byte_start + pair[1].byte_len
                    ),
                );
            }
        }
        let count = fuzzy_matches.len();
        let result = match apply_sorted_non_overlapping_fuzzy_matches(
            &source,
            &fuzzy_matches,
            replacement,
        ) {
            Ok(updated) => updated,
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        };
        (result, count)
    } else {
        let target_idx = occurrence.unwrap_or(0);
        let matched = &fuzzy_matches[target_idx];
        let mut effective_replacement = String::with_capacity(replacement.len().saturating_add(1));
        push_fuzzy_replacement(&mut effective_replacement, &source, matched, replacement);
        (
            match edit::replace_byte_range(
                &source,
                matched.byte_start,
                matched.byte_start + matched.byte_len,
                &effective_replacement,
            ) {
                Ok(updated) => updated,
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            },
            1,
        )
    };

    if edit::wants_preview(&req.params) {
        let mut result = serde_json::json!({
            "file": file,
            "replacements": count,
            "formatted": false,
        });
        if source == new_source {
            result["no_op"] = serde_json::json!(true);
        }
        edit::attach_preview_diff(&mut result, &req.params, file, &source, &new_source);
        return Response::success(&req.id, result);
    }

    // Auto-backup before mutation
    let label = if replace_all {
        format!(
            "edit_match: {} (replace_all x{})",
            match_str,
            positions.len()
        )
    } else {
        format!("edit_match: {}", match_str)
    };
    let backup_id = match edit::auto_backup(ctx, req.session(), path.as_path(), &label, Some(op_id))
    {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Write, format, and validate via shared pipeline
    let mut write_result = match edit::write_format_validate(
        path.as_path(),
        &new_source,
        &ctx.config(),
        &req.params,
    ) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if write_result.rolled_back {
        ctx.backup()
            .lock()
            .discard_operation_entries(req.session(), op_id);
    }

    if let Ok(final_content) = std::fs::read_to_string(path.as_path()) {
        write_result.lsp_outcome = ctx.lsp_post_write(path.as_path(), &final_content, &req.params);
    }

    log::debug!("edit_match: {} in {}", match_str, file);

    let mut result = serde_json::json!({
        "file": file,
        "replacements": count,
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
    edit::attach_backup_skipped_reason(
        &mut result,
        ctx,
        req.session(),
        op_id,
        Some(path.as_path()),
    );

    write_result.append_lsp_diagnostics_to(&mut result);
    write_result.append_reformatted_excerpt_to(&mut result);

    // Compute final on-disk content once for both `no_op` detection and the
    // optional diff metadata. We always emit `no_op: true` when the file
    // content is byte-identical to the source — this happens when:
    //   - agent passed `oldString === newString` (identity edit)
    //   - a formatter normalized the agent's change back to the original
    //   - the replacement matched what the file already contained
    // The match was satisfied (replacements > 0) but no net change landed.
    // Pi/OpenCode UIs use this to render "matched but no change" instead of
    // a bare `+0/-0` that looks like a tool failure (see GitHub #45).
    let final_content = std::fs::read_to_string(&path).unwrap_or_else(|_| new_source);
    if source == final_content {
        result["no_op"] = serde_json::json!(true);
    }

    if edit::wants_diff(&req.params) {
        result["diff"] = edit::compute_diff_for_response(&req.params, &source, &final_content);
    }

    Response::success(&req.id, result)
}

/// Build a context string showing the target line ± `margin` lines.
fn build_context(source: &str, target_line: usize, margin: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = target_line.saturating_sub(margin);
    let end = (target_line + margin + 1).min(lines.len());
    lines[start..end].join("\n")
}

#[cfg(test)]
mod replace_all_tests {
    use super::*;

    fn frozen_reverse_apply(
        source: &str,
        matches: &[crate::fuzzy_match::FuzzyMatch],
        replacement: &str,
    ) -> String {
        let mut result = source.to_string();
        for matched in matches.iter().rev() {
            let byte_end = matched.byte_start + matched.byte_len;
            let restores_newline = matched.pass >= 2
                && byte_end > 0
                && byte_end <= source.len()
                && source.as_bytes()[byte_end - 1] == b'\n'
                && !replacement.is_empty()
                && !replacement.ends_with('\n');
            let effective = if restores_newline {
                format!("{replacement}\n")
            } else {
                replacement.to_string()
            };
            result = edit::replace_byte_range(
                &result,
                matched.byte_start,
                matched.byte_start + matched.byte_len,
                &effective,
            )
            .expect("frozen reverse replacement");
        }
        result
    }

    #[test]
    fn one_pass_replace_all_matches_frozen_reverse_reference() {
        let cases = [
            ("old α old\nold", "old", "replacement"),
            ("old α old\nold", "old", ""),
            ("old α old\nold", "old", "old"),
            (
                "alpha   \nbeta   \nmid\nalpha \nbeta \n",
                "alpha\nbeta",
                "gamma\ndelta",
            ),
            (
                "let x = “hi”…\nmid\nlet x = “hi”…\n",
                "let x = \"hi\"...",
                "let x = 'bye'",
            ),
            ("old\r\nold\r\n", "old", "新"),
        ];

        for (source, needle, replacement) in cases {
            let matches = crate::fuzzy_match::find_all_fuzzy(source, needle);
            assert!(
                matches.len() >= 2,
                "fixture must exercise replace_all: {needle:?}"
            );
            let expected = frozen_reverse_apply(source, &matches, replacement);
            let actual = apply_sorted_non_overlapping_fuzzy_matches(source, &matches, replacement)
                .expect("one-pass replacement");
            assert_eq!(actual.as_bytes(), expected.as_bytes(), "needle={needle:?}");
        }
    }

    #[test]
    fn replace_all_output_construction_allocates_once() {
        let source = "old value\n".repeat(1_024);
        let matches = crate::fuzzy_match::find_all_fuzzy(&source, "old");
        assert_eq!(matches.len(), 1_024);

        let (result, allocations) = crate::test_allocations::count(|| {
            apply_sorted_non_overlapping_fuzzy_matches(&source, &matches, "new")
                .expect("one-pass replacement")
        });

        assert_eq!(result, "new value\n".repeat(1_024));
        assert_eq!(
            allocations, 1,
            "output construction must not allocate once per match"
        );
    }
}
