//! Handler for the `edit_match` command: content-based string matching with
//! disambiguation for multiple occurrences.

use std::io::Write;
use std::path::{Path, PathBuf};

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
///   - `occurrence` (integer, optional, 0-indexed) — select a specific occurrence (single-file only)
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
            Ok(id) => Some(id),
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
                "edit_match: validate_path failed for '{}', treating as glob: {:?}",
                file,
                resp.data
            );
            true
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

    let mut paths: Vec<std::path::PathBuf> = match glob::glob(&full_pattern) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|p| p.is_file())
            .collect(),
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("edit_match: invalid glob pattern: {}", e),
            );
        }
    };
    #[cfg(windows)]
    if paths.is_empty() {
        paths = expand_windows_glob(&full_pattern);
    }
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
        count: usize,
    }
    let mut pending: Vec<PendingEdit> = Vec::new();

    for path in &paths {
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
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

    let checkpoint_name = {
        let name = unique_glob_checkpoint_name(&req.id);
        let files = pending
            .iter()
            .map(|edit| edit.path.clone())
            .collect::<Vec<_>>();
        let checkpoint_result = {
            let backup = ctx.backup().lock();
            ctx.checkpoint()
                .lock()
                .create(req.session(), &name, files, &backup)
        };
        if let Err(e) = checkpoint_result {
            return Response::error(&req.id, e.code(), e.to_string());
        }
        Some(name)
    };

    for edit in &pending {
        if let Err(e) = edit::auto_backup(
            ctx,
            req.session(),
            &edit.path,
            &format!("glob_edit_match: {}", match_str),
            Some(op_id),
        ) {
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

    Response::success(
        &req.id,
        serde_json::json!({
            "ok": true,
            "files": file_results,
            "total_replacements": total_replacements,
            "total_files": total_files,
            "format_skipped_count": format_skipped_count,
            "format_skip_reasons": format_skip_reasons,
        }),
    )
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

#[cfg(windows)]
fn expand_windows_glob(full_pattern: &str) -> Vec<PathBuf> {
    let normalized = full_pattern.replace('\\', "/");
    let Some(first_glob) = normalized.find(['*', '?', '[', '{']) else {
        return Vec::new();
    };
    let Some(base_end) = normalized[..first_glob].rfind('/') else {
        return Vec::new();
    };
    let base = PathBuf::from(&normalized[..base_end]);
    let rel_pattern = &normalized[base_end + 1..];
    let Ok(pattern) = glob::Pattern::new(rel_pattern) else {
        return Vec::new();
    };
    let options = glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    ignore::WalkBuilder::new(&base)
        .hidden(false)
        .parents(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".aftignore")
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.strip_prefix(&base)
                .ok()
                .map(|relative| relative.display().to_string().replace('\\', "/"))
                .is_some_and(|relative| pattern.matches_with(&relative, options))
        })
        .collect()
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

    use super::restore_glob_checkpoint;

    #[test]
    fn restore_glob_checkpoint_reports_failures() {
        // Isolate the checkpoint store's lock-file dir per test by giving each
        // run its own `AFT_CACHE_DIR`. The default storage_dir lives under
        // `$HOME/.cache/aft` (or `$AFT_CACHE_DIR`), and under parallel
        // `cargo test` with a shared HOME (CI containers, sandboxed runners),
        // two tests can race on the same `checkpoints/<project>/checkpoint.lock`
        // path and fail with `No such file or directory` during the
        // `create_dir_all` + `try_acquire` sequence.
        let cache = tempfile::tempdir().unwrap();
        // SAFETY: tests run single-threaded inside this function and the env
        // var is restored on drop; we only mutate process env briefly here.
        unsafe {
            std::env::set_var("AFT_CACHE_DIR", cache.path());
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let a = root.join("a.ts");
        let b = root.join("b.ts");
        fs::write(&a, "const a = TARGET;\n").unwrap();

        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
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

        // SAFETY: only one test in this module mutates this env var.
        unsafe {
            std::env::remove_var("AFT_CACHE_DIR");
        }

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

/// Handle a single-file edit_match (original behavior).
fn handle_single_file_edit_match(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    match_str: &str,
    replacement: &str,
    op_id: &str,
) -> Response {
    let occurrence = req
        .params
        .get("occurrence")
        .and_then(|v| v.as_i64())
        .map(|v| v as usize);

    let replace_all = req
        .params
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

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
            format!("edit_match: '{}' not found in {}", match_str, file),
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

    // If occurrence specified but out of range (only relevant when not replace_all)
    if !replace_all {
        if let Some(occ) = occurrence {
            if occ >= positions.len() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "edit_match: occurrence {} out of range, file has {} occurrence(s)",
                        occ,
                        positions.len()
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
                    "index": idx,
                    "line": line + 1,
                    "context": context,
                })
            })
            .collect();

        return Response::error_with_data(
            &req.id,
            "ambiguous_match",
            format!(
                "Found {} matches. Use 'occurrence' (0-indexed) to select one, or 'replaceAll: true' to replace all.",
                occurrences.len()
            ),
            serde_json::json!({
                "occurrences": occurrences,
            }),
        );
    }

    // Fuzzy passes (rstrip/trim/unicode) are line-based: `find_line_matches`
    // sets the byte range to include the trailing newline after the last
    // matched line, even when the user's `oldString` had no trailing newline.
    // Applying the replacement verbatim over that range drops the newline, so
    // the last replaced line merges with the following line (#83). Re-append
    // a newline when the matched range ended in one and the replacement does
    // not — mirroring batch.rs line-range mode. Gated to fuzzy passes (pass >=
    // 2); the exact pass (1) matches the needle byte-for-byte, so its trailing
    // newline behavior reflects exactly what the caller typed.
    let effective_replacement = |m: &crate::fuzzy_match::FuzzyMatch| -> String {
        let byte_end = m.byte_start + m.byte_len;
        let range_has_trailing_nl = m.pass >= 2
            && byte_end > 0
            && byte_end <= source.len()
            && source.as_bytes()[byte_end - 1] == b'\n';
        if range_has_trailing_nl && !replacement.is_empty() && !replacement.ends_with('\n') {
            format!("{replacement}\n")
        } else {
            replacement.to_string()
        }
    };

    // Apply edit(s) — use fuzzy match byte lengths (may differ from match_str.len())
    let (new_source, count) = if replace_all {
        // Guard against overlapping matches before applying. The fuzzy line
        // passes (2-4) step line-by-line, so a multi-line needle can match
        // overlapping regions (e.g. needle "a\na" over "a\na\na" when whitespace
        // variants defeat the exact pass). Applying overlapping ranges in
        // reverse would let a later replacement overwrite part of an earlier
        // one, silently corrupting the file. Fail cleanly instead — mirrors the
        // `batch` command's overlap guard. (matches are ascending by byte_start.)
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
        // Apply replacements in reverse order to preserve byte offsets
        let mut result = source.clone();
        for m in fuzzy_matches.iter().rev() {
            result = match edit::replace_byte_range(
                &result,
                m.byte_start,
                m.byte_start + m.byte_len,
                &effective_replacement(m),
            ) {
                Ok(updated) => updated,
                Err(e) => {
                    return Response::error(&req.id, e.code(), e.to_string());
                }
            };
        }
        (result, count)
    } else {
        let target_idx = occurrence.unwrap_or(0);
        let m = &fuzzy_matches[target_idx];
        (
            match edit::replace_byte_range(
                &source,
                m.byte_start,
                m.byte_start + m.byte_len,
                &effective_replacement(m),
            ) {
                Ok(updated) => updated,
                Err(e) => {
                    return Response::error(&req.id, e.code(), e.to_string());
                }
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
