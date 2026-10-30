//! Handler for the `edit_match` command: content-based string matching with
//! disambiguation for multiple occurrences.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::access_policy::{AccessOperation, GrantKind};
use crate::backup::CapturedRegularFile;
use crate::context::AppContext;
use crate::edit::{self, validate_edited_file_syntax};
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
    if req.params.get("op").and_then(|v| v.as_str()) == Some("append") {
        if super::github_comments::is_github_resource_path(file) {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit: GitHub resources support only edits[] find/replace entries",
            );
        }
        return handle_append(req, ctx, &op_id);
    }

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

    if super::github_comments::is_github_resource_path(file) {
        return super::github_comments::handle_comment_edit(req, ctx, file, match_str, replacement);
    }

    // Detect glob pattern. Prefer the literal interpretation when the path
    // already exists on disk, even if its name contains glob metacharacters
    // such as `[`, `]`, `*`, `?`, or `{`. This prevents files in directories
    // with brackets (e.g. `src/[another]/file.rs`) from being misclassified as
    // globs.
    // A refused target goes to the single-file handler, whose own validation
    // returns the refusal to the caller.
    if classify_edit_target(file, ctx, &req.id) == EditTargetKind::Glob {
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

    if existed && append_content.is_empty() {
        return edit::no_change_response(&req.id);
    }

    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);
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

    // Capture the original text for the host's unified diff.
    let before_content = if existed {
        std::fs::read_to_string(path.as_path()).unwrap_or_default()
    } else {
        String::new()
    };

    let (mut file_handle, read_access) = match open_append_file(path.as_path()) {
        Ok(opened) => opened,
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

    let original_len = match file_handle.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => return Response::error(&req.id, "write_error", error.to_string()),
    };
    if let Err((bytes_written, error, restore_error)) = append_with_rollback(
        &mut file_handle,
        append_content.as_bytes(),
        |file, appended| {
            if !read_access {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "rollback could not be verified without read access",
                ));
            }
            restore_append_length(file, original_len, appended)
        },
    ) {
        let restored = restore_error.is_none();
        drop(file_handle);
        let removal_error = if !existed && restored {
            std::fs::remove_file(path.as_path()).err()
        } else {
            None
        };
        let restore_error = restore_error.or(removal_error);
        if restore_error.is_none() {
            ctx.backup()
                .lock()
                .discard_operation_entries(req.session(), op_id);
        }
        let rolled_back = restore_error.is_none();
        let mut data = serde_json::json!({
            "bytes_written": bytes_written,
            "rolled_back": rolled_back,
        });
        if !rolled_back {
            if let Some(id) = backup_id {
                data["backup_id"] = serde_json::json!(id);
            }
            edit::attach_backup_skipped_reason(
                &mut data,
                ctx,
                req.session(),
                op_id,
                Some(path.as_path()),
            );
        }
        return Response::error_with_data(
            &req.id,
            "write_error",
            format!(
                "edit_match append: failed after writing {} of {} bytes to {}: {}{}",
                bytes_written,
                append_content.len(),
                file,
                error,
                restore_error
                    .map(|error| format!(
                        "; restore failed: {error}; inspect the file or undo before retrying"
                    ))
                    .unwrap_or_default(),
            ),
            data,
        );
    }
    let bytes_written = append_content.len();
    let mut warnings = Vec::new();

    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o644))
        {
            warnings.push(format!(
                "Append wrote {} bytes to {}, but setting permissions failed: {}. Do not retry the append.",
                bytes_written, file, error,
            ));
        }
    }

    // Run the project formatter on the appended file. `auto_format` honors
    // `config.format_on_edit` internally and returns `(false, None)` when
    // disabled, so we can call it unconditionally. Bug #4 of the v0.18.3
    // format_on_edit audit: append previously hardcoded `formatted: false,
    // format_skipped_reason: None` and bypassed the formatter entirely.
    // Agents that appended messy lines kept them messy with no signal.
    let config = ctx.config();
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let (formatted, format_skipped_reason) =
        format::auto_format(tool_policy, path.as_path(), &config);
    drop(config);

    // Re-read final content AFTER formatting so the LSP sees the formatted
    // text (matches `write_format_validate` ordering: write → format → validate
    // → notify LSP) and the diff in the response reflects what's actually
    // on disk. Reading once and reusing for LSP + diff also avoids a TOCTOU
    // window where the formatter could rewrite the file between reads.
    let final_content = match std::fs::read_to_string(path.as_path()) {
        Ok(content) => Some(content),
        Err(error) => {
            warnings.push(format!(
                "Append wrote {} bytes to {}, but reading the file after formatting failed: {}. Final contents could not be verified; inspect the file before retrying.",
                bytes_written, file, error,
            ));
            None
        }
    };

    // Honor `diagnostics: true` like other write-style handlers (write,
    // edit_match, edit_symbol). When false/absent, this still notifies the
    // LSP layer that the file changed but doesn't wait for diagnostics.
    let lsp_outcome = final_content
        .as_deref()
        .and_then(|content| ctx.lsp_post_write(path.as_path(), content, &req.params));
    // Validate the observed post-format bytes, as the shared write pipeline does.
    // Path validation can cache a missing parent before append creates it; asking
    // the file parser to stat that parent again would reuse the stale failure.
    // A file this append created goes through the same created-file check as
    // `write`, so the reply can say where the new file fails to parse.
    let created_syntax = if existed {
        None
    } else {
        final_content
            .as_deref()
            .map(|content| edit::check_created_file_syntax(tool_policy, path.as_path(), content))
    };
    let syntax_valid = match created_syntax {
        Some(check) => check.valid,
        None => final_content
            .as_deref()
            .and_then(|content| edit::validate_syntax_str(content, path.as_path())),
    };
    let syntax_error_line = created_syntax.and_then(|check| check.first_error_line);

    let mut result = serde_json::json!({
        "ok": true,
        "file": file,
        "created": !existed,
        "bytes_written": bytes_written,
        "syntax_valid": syntax_valid,
        "formatted": formatted,
    });
    if !warnings.is_empty() {
        result["warnings"] = serde_json::json!(warnings);
    }
    if let Some(line) = syntax_error_line {
        result[edit::CREATED_SYNTAX_ERROR_LINE_FIELD] = serde_json::json!(line);
    }
    let syntax_skipped_reason = created_syntax.and_then(|check| check.skipped_reason);
    edit::append_syntax_skipped_reason(&mut result, syntax_skipped_reason);

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
    if existed && final_content.as_deref() == Some(before_content.as_str()) {
        result["no_op"] = serde_json::json!(true);
    }

    // Reuse the standard WriteResult formatter so append's response carries
    // the same `lsp_diagnostics`, `lsp_complete`, `lsp_pending_servers`, and
    // `lsp_exited_servers` shape as `write` and `edit_match` find/replace.
    if lsp_outcome.is_some() {
        let write_result = edit::WriteResult {
            syntax_valid,
            syntax_skipped_reason,
            syntax_error_line,
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
            final_text: None,
            write_warnings: Vec::new(),
            reformatted_excerpt: final_content.as_deref().and_then(|content| {
                edit::compute_reformatted_excerpt(
                    &format!("{before_content}{append_content}"),
                    content,
                )
            }),
        };
        write_result.append_lsp_diagnostics_to(&mut result);
        write_result.append_reformatted_excerpt_to(&mut result);
    }

    if let Some(content) = final_content.as_deref() {
        edit::attach_edit_diff(&mut result, &req.params, file, &before_content, content);
    }
    Response::success(&req.id, result)
}

fn open_append_file(path: &Path) -> std::io::Result<(std::fs::File, bool)> {
    match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
    {
        Ok(file) => Ok((file, true)),
        // Read access enables verified rollback, but is not a prerequisite for
        // appending when the caller has chosen to proceed without a backup.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map(|file| (file, false))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
thread_local! {
    static APPEND_FAILURE: std::cell::RefCell<Option<(usize, Box<dyn FnOnce()>)>> = const { std::cell::RefCell::new(None) };
}

fn append_bytes(writer: &mut impl Write, content: &[u8]) -> (usize, Option<std::io::Error>) {
    #[cfg(test)]
    let failure = APPEND_FAILURE.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
    #[cfg(test)]
    let content = failure.as_ref().map_or(content, |(limit, _)| {
        &content[..(*limit).min(content.len())]
    });
    let mut written = 0;
    while written < content.len() {
        match writer.write(&content[written..]) {
            Ok(0) => return (written, Some(std::io::ErrorKind::WriteZero.into())),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return (written, Some(error)),
        }
    }
    #[cfg(test)]
    if let Some((_, interfere)) = failure {
        interfere();
        return (
            written,
            Some(std::io::Error::other("injected append failure")),
        );
    }
    (written, None)
}

fn append_with_rollback<W: Write>(
    writer: &mut W,
    content: &[u8],
    restore: impl FnOnce(&mut W, &[u8]) -> std::io::Result<()>,
) -> Result<(), (usize, std::io::Error, Option<std::io::Error>)> {
    let (written, error) = append_bytes(writer, content);
    match error {
        Some(error) => Err((written, error, restore(writer, &content[..written]).err())),
        None => Ok(()),
    }
}

fn restore_append_length(file: &std::fs::File, len: u64, appended: &[u8]) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    #[cfg(windows)]
    let mut rollback = {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            ReOpenFile, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // Append-only handles cannot truncate on Windows. Reopen the same file
        // object for verification and rollback, never a mutable path.
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle() as _,
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { std::fs::File::from_raw_handle(handle as _) }
    };
    #[cfg(not(windows))]
    let mut rollback = file.try_clone()?;

    let expected_len = len
        .checked_add(appended.len() as u64)
        .ok_or_else(|| std::io::Error::other("append length overflow"))?;
    let verify_length = || -> std::io::Result<()> {
        if file.metadata()?.len() != expected_len {
            return Err(std::io::Error::other(
                "file length changed; refusing to truncate unverified bytes",
            ));
        }
        Ok(())
    };
    verify_length()?;
    rollback.seek(SeekFrom::Start(len))?;
    // Another writer may have appended or replaced bytes after our original
    // length was captured. Only our exact partial suffix is safe to remove.
    let mut buffer = [0; 8192];
    for expected in appended.chunks(buffer.len()) {
        let actual = &mut buffer[..expected.len()];
        rollback.read_exact(actual)?;
        if actual != expected {
            return Err(std::io::Error::other(
                "file tail changed; refusing to truncate unverified bytes",
            ));
        }
    }
    verify_length()?;
    rollback.set_len(len)
}

/// Returns true if the file path contains glob characters. This is only a
/// spelling check; [`classify_edit_target`] makes the actual decision.
pub(crate) fn is_glob_pattern(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('{') || path.contains('[')
}

/// How an edit target is interpreted: one file, or a pattern over many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditTargetKind {
    /// A concrete file path: either spelled without glob characters, or one
    /// that names an existing path even though it contains them.
    Literal,
    /// A glob pattern: spelled with glob characters and nothing exists at
    /// that literal path.
    Glob,
    /// Spelled with glob characters, but path validation refused the target
    /// (outside the roots, denied, or waiting on the person's permission), so
    /// its existence was never probed.
    Refused,
}

/// Decides whether `file` is a glob pattern or a literal path. A path that
/// exists on disk is always literal, even if its name contains glob
/// metacharacters such as `[`, `]`, `*`, `?`, or `{`. This defends against
/// directories or files with brackets in their names (Next.js routes such as
/// `app/[id]/page.tsx`) being misclassified as glob patterns (issues #132 and
/// #486).
///
/// Resolution goes through `ctx.validate_path`, the same validation the
/// single-file handlers apply. `req_id` is the id of the request being served,
/// so per-request access rules (such as a forced project-root restriction)
/// apply to this check exactly as they apply to the edit. The existence probe
/// runs only on a path validation allowed; otherwise the result is
/// [`EditTargetKind::Refused`] and each caller picks its own safe fallback.
/// A refusal here never prompts the person: a permission request is only
/// raised when the refusal is returned as a response.
///
/// Both `edit_match` and the agent-facing `edit` translator (through
/// `run_tool_call`) use this function so the two cannot disagree about which
/// targets are files.
pub(crate) fn classify_edit_target(file: &str, ctx: &AppContext, req_id: &str) -> EditTargetKind {
    if !is_glob_pattern(file) {
        return EditTargetKind::Literal;
    }
    match ctx.validate_path(req_id, Path::new(file)) {
        Ok(candidate) if candidate.exists() => EditTargetKind::Literal,
        Ok(_) => EditTargetKind::Glob,
        Err(resp) => {
            log::debug!(
                "edit target: validate_path refused '{}': {:?}",
                file,
                resp.data
            );
            EditTargetKind::Refused
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
    let mut skipped_files = Vec::new();
    let mut response = handle_glob_edit_match_inner(
        req,
        ctx,
        pattern,
        match_str,
        replacement,
        op_id,
        &mut skipped_files,
    );
    if !skipped_files.is_empty() {
        response.data["skipped_files"] = serde_json::json!(skipped_files);
        response.data["complete"] = serde_json::json!(false);
    }
    response
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum GlobEditSkipReason {
    DeniedFloor,
}

#[derive(serde::Serialize)]
struct SkippedGlobEditFile {
    file: String,
    reason: GlobEditSkipReason,
}

fn handle_glob_edit_match_inner(
    req: &RawRequest,
    ctx: &AppContext,
    pattern: &str,
    match_str: &str,
    replacement: &str,
    op_id: &str,
    skipped_files: &mut Vec<SkippedGlobEditFile>,
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

    let policy = match ctx.access_policy(&req.id) {
        Ok(policy) => policy,
        Err(response) => return response,
    };
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
        // Floors protect content reads too. Root and write authorization belong
        // after matching, so unrelated glob candidates do not require permission.
        if policy
            .capability_floor(
                AccessOperation::Write,
                GrantKind::File,
                &crate::access_policy::policy_path(path),
            )
            .is_some()
        {
            skipped_files.push(SkippedGlobEditFile {
                file: path.display().to_string(),
                reason: GlobEditSkipReason::DeniedFloor,
            });
            continue;
        }
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

        let validated_path = match validate_glob_edit_path(ctx, &req.id, path) {
            Ok(validated) => validated,
            Err(resp) if resp.data["code"] == "denied_floor" => {
                skipped_files.push(SkippedGlobEditFile {
                    file: path.display().to_string(),
                    reason: GlobEditSkipReason::DeniedFloor,
                });
                continue;
            }
            Err(resp) => return resp,
        };
        let count = positions.len();
        let new_source = source.replace(match_str, replacement);
        let file_str = path.display().to_string();

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
                "edit_match: {} not found in any files matching '{}'",
                crate::fuzzy_match::describe_searched_text(match_str),
                pattern
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
                let (patch, diff) = edit::response_diff(
                    &serde_json::json!({"include_diff_content": true}),
                    &edit.file_str,
                    &edit.original_source,
                    &edit.new_source,
                );
                additions += diff.get("additions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                deletions += diff.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                preview_diff.push_str(&patch);
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
                "metadata": { "diff": preview_diff },
            }),
        );
    }

    if pending
        .iter()
        .all(|edit| edit.original_source == edit.new_source)
    {
        return edit::no_change_response(&req.id);
    }
    pending.retain(|edit| edit.original_source != edit.new_source);
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);

    let _mutation = crate::runtime_drain::record_file_mutation(
        ctx,
        pending.iter().map(|edit| edit.path.as_path()),
    );

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
        let (formatted, format_skipped_reason) =
            format::auto_format(tool_policy, &edit.path, &config);
        if let Some(reason) = &format_skipped_reason {
            format_skipped_count += 1;
            format_skip_reasons.insert(reason.clone());
        }
        let syntax_valid = match validate_edited_file_syntax(&edit.path) {
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
    let diff = pending
        .iter()
        .map(|edit| {
            let after =
                std::fs::read_to_string(&edit.path).unwrap_or_else(|_| edit.new_source.clone());
            edit::build_unified_diff(&edit.file_str, &edit.original_source, &after)
        })
        .collect::<String>();
    result["metadata"] = serde_json::json!({ "diff": diff });
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

    /// A failed edit should name the miss, not paste the whole `match` text
    /// back: the caller already has it, and a large one doubles its cost in
    /// the transcript. The nearest-miss excerpt that follows is already
    /// bounded (ten lines of at most 240 characters).
    #[test]
    fn not_found_error_does_not_repeat_a_large_match() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small.rs");
        fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let needle: String = (0..400)
            .map(|index| format!("    let absent_{index} = 0;\n"))
            .collect();
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "miss",
            "command": "edit_match",
            "file": path.display().to_string(),
            "match": needle,
            "replacement": "x",
        }))
        .unwrap();
        let response = handle_edit_match(&request, &ctx);
        assert!(!response.success);
        assert_eq!(response.data["code"], "match_not_found");
        let message = response.data["message"].as_str().unwrap();
        assert!(
            !message.contains(&needle),
            "{} byte match repeated in a {} byte error",
            needle.len(),
            message.len()
        );
        assert!(message.len() < 4 * 1024, "{} byte error", message.len());
        assert!(
            message.starts_with(
                "edit_match: '    let absent_0 = 0;…' (400 lines, 9490 bytes) not found in "
            ),
            "{message}"
        );
    }

    /// A failed match with a 64 KiB or 256 KiB `match` against a large file
    /// used to spend seconds in the whitespace-reflow pass, whose window scan
    /// cost file lines × match lines. The pass now scans linearly: the reply
    /// is the usual not-found message, the scan stays within two steps per
    /// byte of the whitespace-normalized file, and the file is untouched. A 64 KiB reflowed match on the
    /// same file still applies.
    #[test]
    fn large_match_reflow_search_is_linear_and_still_applies() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("big.rs");
        let lines = (0..25_000)
            .map(|index| format!("    let value_{index} = compute({index}, \"text\");\n"))
            .collect::<Vec<_>>();
        let source = lines.concat();
        // The search scans the file with every whitespace run collapsed to one space.
        let normalized_bytes = source
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .len();
        fs::write(&path, &source).unwrap();
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let request = |id: &str, needle: &str, replacement: &str| -> RawRequest {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "command": "edit_match",
                "file": path.display().to_string(),
                "match": needle,
                "replacement": replacement,
            }))
            .unwrap()
        };
        for kib in [64usize, 256] {
            // Every other line changed, so no pass matches.
            let mut needle = String::new();
            for (offset, line) in lines[8_000..].iter().enumerate() {
                if needle.len() >= kib * 1024 {
                    break;
                }
                if offset % 2 == 0 {
                    needle.push_str("// changed ");
                }
                needle.push_str(line);
            }
            crate::patch::matcher::REFLOW_SCAN_STEPS.with(|steps| steps.set(0));
            let started = std::time::Instant::now();
            let response = handle_edit_match(&request("large-miss", &needle, "x"), &ctx);
            let elapsed = started.elapsed();
            let steps = crate::patch::matcher::REFLOW_SCAN_STEPS.with(std::cell::Cell::get);
            eprintln!("{kib} KiB failed match took {elapsed:?}");
            assert!(!response.success);
            assert_eq!(response.data["code"], "match_not_found");
            assert_eq!(
                response.data["message"].as_str().unwrap(),
                format!(
                    "edit_match: {} not found in {}{}",
                    crate::fuzzy_match::describe_searched_text(&needle),
                    path.display(),
                    crate::fuzzy_match::render_edit_not_found_detail(&source, &needle, "x")
                ),
            );
            assert!(
                steps <= 2 * normalized_bytes,
                "{steps} scan steps for {normalized_bytes} normalized bytes"
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), source);

        // Join each pair of lines in a 64 KiB region, as a formatter would.
        let mut region_end = 8_000;
        let mut region_bytes = 0;
        while region_bytes < 64 * 1024 {
            region_bytes += lines[region_end].len();
            region_end += 1;
        }
        region_end += region_end % 2;
        let reflowed = lines[8_000..region_end]
            .chunks(2)
            .map(|pair| {
                pair.iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let response = handle_edit_match(&request("large-reflow", &reflowed, "replaced();"), &ctx);
        assert!(response.success, "{:?}", response.data);
        let edited = fs::read_to_string(&path).unwrap();
        assert!(edited.contains("compute(7999, \"text\");\n    replaced();\n"));
        assert_eq!(edited.lines().count(), 25_000 - (region_end - 8_000) + 1);
    }

    #[test]
    fn append_bytes_preserves_partial_write_count() {
        struct ShortWriter(Vec<u8>);
        impl std::io::Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.0.is_empty() {
                    return Err(std::io::ErrorKind::StorageFull.into());
                }
                self.0.extend_from_slice(&bytes[..2]);
                Ok(2)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ShortWriter(Vec::new());
        let (written, error) = super::append_bytes(&mut writer, b"hello");
        assert_eq!(writer.0, b"he");
        assert_eq!(written, 2);
        assert_eq!(error.unwrap().kind(), std::io::ErrorKind::StorageFull);

        let (written, error) =
            super::append_bytes(&mut std::io::Cursor::new(&mut [][..]), b"hello");
        assert_eq!(written, 0);
        assert_eq!(error.unwrap().kind(), std::io::ErrorKind::WriteZero);
    }

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
                .content_bytes()
                .unwrap()
                .as_ref(),
            b"first TARGET\n"
        );
        assert_eq!(
            backup.history(request.session(), &second)[0]
                .content_bytes()
                .unwrap()
                .as_ref(),
            b"second TARGET\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn glob_edit_skips_non_matching_symlink_outside_project_root() {
        assert_glob_edit_outside_symlink(false);
    }

    #[cfg(unix)]
    #[test]
    fn glob_edit_refuses_matching_symlink_outside_project_root() {
        assert_glob_edit_outside_symlink(true);
    }

    #[cfg(unix)]
    fn assert_glob_edit_outside_symlink(outside_matches: bool) {
        let cache = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_file = project.path().join("a.txt");
        let outside_file = outside.path().join("outside.txt");
        let outside_source = if outside_matches {
            "outside TARGET\n"
        } else {
            "unrelated\n"
        };
        fs::write(&inside_file, "inside TARGET\n").unwrap();
        fs::write(&outside_file, outside_source).unwrap();
        std::os::unix::fs::symlink(&outside_file, project.path().join("z.txt")).unwrap();
        let outside_modified = fs::metadata(&outside_file).unwrap().modified().unwrap();

        let ctx = AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root: Some(project.path().to_path_buf()),
                restrict_to_project_root: true,
                ..Config::default()
            },
        );
        ctx.checkpoint()
            .lock()
            .set_lock_path_for_test(cache.path().join("checkpoint.lock"));
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "glob-outside-symlink",
            "command": "edit_match",
            "file": "*.txt",
            "match": "TARGET",
            "replacement": "DONE",
            "replace_all": true
        }))
        .unwrap();

        let response = handle_edit_match(&request, &ctx);
        if outside_matches {
            assert!(!response.success, "{response:?}");
            assert_eq!(response.data["code"], "path_outside_root", "{response:?}");
            assert_eq!(fs::read_to_string(&inside_file).unwrap(), "inside TARGET\n");
        } else {
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["total_files"], 1, "{response:?}");
            assert_eq!(fs::read_to_string(&inside_file).unwrap(), "inside DONE\n");
        }
        assert_eq!(fs::read_to_string(&outside_file).unwrap(), outside_source);
        assert_eq!(
            fs::metadata(&outside_file).unwrap().modified().unwrap(),
            outside_modified
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

/// Append the text that replaces the plan's match at `index`: the
/// replacement re-indented by the match's indentation shift (see
/// [`crate::fuzzy_match::ReindentPlan`]), with line separators adjusted so
/// the match boundary neither loses nor doubles a newline. Fails with
/// `ambiguous_indentation` when the shift cannot be determined without
/// guessing.
pub(crate) fn push_fuzzy_replacement(
    output: &mut String,
    plan: &crate::fuzzy_match::ReindentPlan<'_>,
    index: usize,
) -> Result<(), crate::error::AftError> {
    let source = plan.source();
    let matched = plan.matched(index);
    let replacement = &*plan.replacement_for(index)?;
    let end = matched.byte_start + matched.byte_len;
    // When the match leaves the source line separator outside its span, a
    // replacement ending in a newline must not introduce a second separator.
    let remaining = source[end..].trim_start_matches([' ', '\t']);
    let separator_is_outside =
        matched.pass > 1 && (remaining.starts_with('\n') || remaining.starts_with("\r\n"));
    if separator_is_outside && replacement.ends_with('\n') {
        output.push_str(
            replacement
                .strip_suffix("\r\n")
                .unwrap_or(&replacement[..replacement.len() - 1]),
        );
    } else {
        output.push_str(replacement);
    }
    if fuzzy_replacement_restores_newline(source, matched, replacement) {
        output.push('\n');
    }
    Ok(())
}

pub(crate) fn apply_sorted_non_overlapping_fuzzy_matches(
    source: &str,
    matches: &[crate::fuzzy_match::FuzzyMatch],
    needle: &str,
    replacement: &str,
    path: Option<&Path>,
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
    // One plan covers all the matches, so finding string lines parses at most once.
    let plan = crate::fuzzy_match::ReindentPlan::new(source, needle, replacement, path, matches);
    for (index, matched) in matches.iter().enumerate() {
        result.push_str(&source[copied_through..matched.byte_start]);
        push_fuzzy_replacement(&mut result, &plan, index)?;
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

    let (source, observation) = match edit::read_edit_source(&path) {
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
                "edit_match: {} not found in {}{}",
                crate::fuzzy_match::describe_searched_text(match_str),
                file,
                crate::fuzzy_match::render_edit_not_found_detail(&source, match_str, replacement,),
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
        let line_index = edit::SourceLineIndex::new(&source);
        let occurrences: Vec<serde_json::Value> = positions
            .iter()
            .enumerate()
            .map(|(idx, &byte_pos)| {
                let line = line_index.line_at(byte_pos);
                let context = line_index.context(line, 2);
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
            match_str,
            replacement,
            Some(path.as_path()),
        ) {
            Ok(updated) => updated,
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        };
        (result, count)
    } else {
        let target_idx = occurrence.unwrap_or(0);
        let matched = &fuzzy_matches[target_idx];
        let mut effective_replacement = String::with_capacity(replacement.len().saturating_add(1));
        let plan = crate::fuzzy_match::ReindentPlan::new(
            &source,
            match_str,
            replacement,
            Some(path.as_path()),
            std::slice::from_ref(matched),
        );
        if let Err(e) = push_fuzzy_replacement(&mut effective_replacement, &plan, 0) {
            return Response::error(&req.id, e.code(), e.to_string());
        }
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
        if fuzzy_matches[0].pass > 1 {
            let selected = if replace_all {
                &fuzzy_matches[..]
            } else {
                &fuzzy_matches[occurrence.unwrap_or(0)..occurrence.unwrap_or(0) + 1]
            };
            result["fuzzy_match"] =
                crate::fuzzy_match::replacement_detail(&source, match_str, selected);
        }
        edit::attach_preview_diff(&mut result, &req.params, file, &source, &new_source);
        return Response::success(&req.id, result);
    }

    if source == new_source {
        return edit::no_change_response(&req.id);
    }

    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);

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
    let backup_id = match edit::auto_backup_observed(
        ctx,
        req.session(),
        path.as_path(),
        &label,
        Some(op_id),
        &observation,
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Write, format, and validate via shared pipeline
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let mut write_result = match edit::write_format_validate_observed(
        tool_policy,
        path.as_path(),
        &new_source,
        &ctx.config(),
        &req.params,
        &source,
        &observation,
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

    if let Some(final_content) = write_result.final_content(path.as_path()) {
        write_result.lsp_outcome = ctx.lsp_post_write(path.as_path(), final_content, &req.params);
    }

    log::debug!("edit_match: {} in {}", match_str, file);

    let mut result = serde_json::json!({
        "file": file,
        "replacements": count,
        "formatted": write_result.formatted,
    });

    if fuzzy_matches[0].pass > 1 {
        let selected = if replace_all {
            &fuzzy_matches[..]
        } else {
            &fuzzy_matches[occurrence.unwrap_or(0)..occurrence.unwrap_or(0) + 1]
        };
        result["fuzzy_match"] =
            crate::fuzzy_match::replacement_detail(&source, match_str, selected);
    }
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
    let final_content = write_result.final_content(&path).unwrap_or(&new_source);
    if source == final_content {
        result["no_op"] = serde_json::json!(true);
    }

    edit::attach_edit_diff(&mut result, &req.params, file, &source, &final_content);
    Response::success(&req.id, result)
}

#[cfg(test)]
mod replace_all_tests {
    use super::*;

    /// Finding which replacement lines sit inside string literals parses the
    /// edited file once for all the matches of a replace-all, and not at
    /// all when the file, the matched lines and the replacement hold no
    /// string delimiter.
    #[test]
    fn replace_all_parses_for_string_lines_at_most_once() {
        use crate::fuzzy_match::STRING_LINE_PARSES;
        let path = Path::new("t.py");
        for (line, needle, replacement, shifted, parses) in [
            (
                "    v = 'a'\n",
                "v = 'a' ",
                "v = 'b'\nw = 'c'",
                "    v = 'b'\n    w = 'c'\n",
                1,
            ),
            (
                "    v = 1\n",
                "v = 1 ",
                "v = 2\nw = 3",
                "    v = 2\n    w = 3\n",
                0,
            ),
        ] {
            let source = format!("def f():\n{}", line.repeat(1_000));
            let matches = crate::fuzzy_match::find_all_fuzzy(&source, needle);
            assert_eq!(matches.len(), 1_000);
            assert_eq!(matches[0].pass, 3, "the loose pass must match");
            STRING_LINE_PARSES.with(|count| count.set(0));
            let result = apply_sorted_non_overlapping_fuzzy_matches(
                &source,
                &matches,
                needle,
                replacement,
                Some(path),
            )
            .unwrap();
            assert_eq!(result, format!("def f():\n{}", shifted.repeat(1_000)));
            assert_eq!(
                STRING_LINE_PARSES.with(|count| count.get()),
                parses,
                "parses for {needle:?}"
            );
        }
    }

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
            let actual = apply_sorted_non_overlapping_fuzzy_matches(
                source,
                &matches,
                needle,
                replacement,
                None,
            )
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
            apply_sorted_non_overlapping_fuzzy_matches(&source, &matches, "old", "new", None)
                .expect("one-pass replacement")
        });

        assert_eq!(result, "new value\n".repeat(1_024));
        assert_eq!(
            allocations, 1,
            "output construction must not allocate once per match"
        );
    }
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_only_file_accepts_append_when_backup_disabled() {
        assert_write_only_append(false);
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_only_append_retains_unverified_partial_bytes() {
        assert_write_only_append(true);
    }

    #[cfg(unix)]
    fn assert_write_only_append(fail: bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write-only.txt");
        std::fs::write(&path, "before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        match std::fs::File::open(&path) {
            // Root (and some CI containers) read files regardless of mode 0200,
            // so a write-only file cannot be produced and the fixture proves nothing.
            Ok(_) => {
                eprintln!(
                    "skipped: mode 0200 does not deny reads for this user (running as root?)"
                );
                return;
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        }
        let mut config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        config.backup.enabled = Some(false);
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        // Direct handler fixtures must apply configure's backup-store policy.
        ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "write-only-append", "command": "edit_match", "op": "append", "file": path, "append_content": "after"
        })).unwrap();
        if fail {
            APPEND_FAILURE.with(|slot| *slot.borrow_mut() = Some((1, Box::new(|| {}))));
        }
        let response = handle_edit_match(&request, &ctx);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        if fail {
            APPEND_FAILURE.with(|slot| {
                assert!(
                    slot.borrow().is_none(),
                    "append injection was not reached: {:?}",
                    response.data
                )
            });
            assert!(!response.success, "{:?}", response.data);
            assert_eq!(response.data["bytes_written"], 1);
            assert_eq!(response.data["rolled_back"], false);
            assert!(
                response.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("rollback could not be verified without read access"),
                "{:?}",
                response.data
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), "beforea");
        } else {
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["bytes_written"], 5);
            assert_eq!(std::fs::read_to_string(path).unwrap(), "beforeafter");
        }
    }

    #[test]
    fn failed_append_preserves_concurrent_suffix_and_backup() {
        failed_append_preserves_external_bytes(false);
    }

    #[test]
    fn failed_append_preserves_same_length_foreign_bytes_and_backup() {
        failed_append_preserves_external_bytes(true);
    }

    #[test]
    fn failed_append_rolls_back_own_suffix_and_discards_backup() {
        for existed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("append.txt");
            if existed {
                std::fs::write(&path, "before").unwrap();
            }
            let ctx = AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    format_on_edit: false,
                    ..Default::default()
                },
            );
            let request: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "own-failed-append", "command": "edit_match", "op": "append", "file": path, "append_content": "after"
            })).unwrap();
            APPEND_FAILURE.with(|slot| *slot.borrow_mut() = Some((1, Box::new(|| {}))));
            let response = handle_edit_match(&request, &ctx);
            APPEND_FAILURE.with(|slot| {
                assert!(
                    slot.borrow().is_none(),
                    "append failure injection was not reached: {:?}",
                    response.data
                )
            });
            assert!(!response.success);
            assert_eq!(response.data["bytes_written"], 1);
            assert_eq!(response.data["rolled_back"], true);
            if existed {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
            } else {
                assert!(!path.exists());
            }
            assert!(ctx
                .backup()
                .lock()
                .preview_last_operation_paths(request.session())
                .is_err());
        }
    }

    fn failed_append_preserves_external_bytes(replace_tail: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.txt");
        std::fs::write(&path, "before").unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
        );
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "failed-append", "command": "edit_match", "op": "append", "file": path, "append_content": "after"
        }))
        .unwrap();
        let external_path = path.clone();
        APPEND_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some((
                1,
                Box::new(move || {
                    // This separate handle writes after AFT has captured the original
                    // length and written its first byte, but before its append fails.
                    if replace_tail {
                        std::fs::write(external_path, "beforeZ").unwrap();
                    } else {
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(external_path)
                            .unwrap()
                            .write_all(b"external")
                            .unwrap();
                    }
                }),
            ))
        });
        let response = handle_edit_match(&request, &ctx);
        APPEND_FAILURE.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "append failure injection was not reached: {:?}",
                response.data
            )
        });
        assert!(!response.success, "{:?}", response.data);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            if replace_tail {
                "beforeZ"
            } else {
                "beforeaexternal"
            }
        );
        assert_eq!(response.data["bytes_written"], 1);
        assert_eq!(response.data["rolled_back"], false);
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("1 of 5 bytes"));
        assert!(
            response.data["backup_id"].is_string(),
            "{:?}",
            response.data
        );
        let store = ctx.backup().lock();
        let history = store.history(request.session(), &path);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content_bytes().unwrap().as_ref(), b"before");
    }

    struct PartialWriter {
        file: std::fs::File,
        wrote: bool,
    }

    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.wrote {
                return Err(std::io::Error::other("injected partial write failure"));
            }
            self.wrote = true;
            self.file.write(&bytes[..1])
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    #[test]
    fn failed_append_truncates_partial_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.txt");
        std::fs::write(&path, "before").unwrap();
        let mut writer = PartialWriter {
            file: std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(&path)
                .unwrap(),
            wrote: false,
        };
        let error = append_with_rollback(&mut writer, b"after", |writer, appended| {
            restore_append_length(&writer.file, 6, appended)
        })
        .unwrap_err();
        assert!(error.2.is_none(), "{:?}", error.2);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
    }

    #[test]
    fn failed_append_reports_restore_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.txt");
        std::fs::write(&path, "before").unwrap();
        let mut writer = PartialWriter {
            file: std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            wrote: false,
        };
        let error = append_with_rollback(&mut writer, b"after", |_, _| {
            Err(std::io::Error::other("restore"))
        })
        .unwrap_err();
        assert_eq!(error.2.unwrap().to_string(), "restore");
    }

    #[test]
    fn ambiguity_context_is_bounded_after_source_shortens() {
        let index = edit::SourceLineIndex::new("é\r\ntext\n");
        assert_eq!(index.context(0, 0), "é");
        assert_eq!(index.context(99, 2), "");
        assert_eq!(index.context(usize::MAX, usize::MAX), "é\ntext");
    }

    #[test]
    fn fuzzy_crlf_separator_is_not_duplicated() {
        let source = "a\r\nb\r\nc\r\n";
        let matches = crate::fuzzy_match::find_all_fuzzy(source, "a\nb\n");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            super::apply_sorted_non_overlapping_fuzzy_matches(
                source,
                &matches,
                "a\nb\n",
                "x\r\ny\r\n",
                None,
            )
            .unwrap(),
            "x\r\ny\r\nc\r\n"
        );
        assert_eq!(
            super::apply_sorted_non_overlapping_fuzzy_matches(
                source, &matches, "a\nb\n", "x\ny\n", None
            )
            .unwrap(),
            "x\ny\r\nc\r\n"
        );
    }
}
