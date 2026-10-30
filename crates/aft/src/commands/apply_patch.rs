//! Rust implementation of the `apply_patch` command.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;
use serde_json::{json, Value};

use crate::context::AppContext;
use crate::edit;
use crate::patch::apply::apply_update_chunks;
use crate::patch::parser::{parse_patch, Hunk};
use crate::protocol::{RawRequest, Response};

#[derive(Clone)]
struct ResolvedHunk {
    hunk: Hunk,
    source: ResolvedPath,
    move_dest: Option<ResolvedPath>,
}

#[derive(Clone)]
struct ResolvedPath {
    abs: PathBuf,
    rel: String,
}

struct AppliedHunkResult {
    index: usize,
    kind: &'static str,
    file_path: PathBuf,
    display_path: PathBuf,
    move_path: Option<PathBuf>,
    before: String,
    after: String,
    delete_lines: Option<usize>,
}

struct DiffEntry {
    file_path: PathBuf,
    display_path: PathBuf,
    move_path: Option<PathBuf>,
    last_kind: &'static str,
    before: String,
    after: String,
    delete_lines: Option<usize>,
    hunk_count: usize,
    syntax_error_line: Option<usize>,
    syntax_skipped_reason: Option<&'static str>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Agent-facing DISPLAY paths (relativePath, movePath, diff headers) are
/// forward-slash normalized so they are stable across platforms — matching the
/// rest of AFT's tools (e.g. outline's `path_to_slash`). Without this, Windows
/// emits `nested\dest.txt` and breaks the cross-platform display contract.
fn display_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn command_params(req: &RawRequest) -> &Value {
    req.params
        .get("params")
        .filter(|params| params.is_object())
        .unwrap_or(&req.params)
}

fn project_root(ctx: &AppContext) -> Option<PathBuf> {
    ctx.config().project_root.clone()
}

fn project_root_for_relative_paths(ctx: &AppContext) -> Option<PathBuf> {
    project_root(ctx)
}

fn resolve_patch_input(ctx: &AppContext, path: &str) -> PathBuf {
    let raw = Path::new(path);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else if let Some(root) = project_root(ctx) {
        root.join(raw)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(raw)
    }
}

fn normalize_resolved_path(path: PathBuf) -> PathBuf {
    crate::inspect::job::canonicalize_normalized(&path)
}

fn normalize_write_location(path: PathBuf) -> PathBuf {
    let normalized = crate::inspect::job::normalize_path(&path);
    let Some(file_name) = normalized.file_name() else {
        return normalize_resolved_path(normalized);
    };
    let parent = normalized.parent().unwrap_or_else(|| Path::new(""));
    let parent = normalize_resolved_path(parent.to_path_buf());
    crate::inspect::job::normalize_path(&parent.join(file_name))
}

fn relative_path(abs: &Path, root: Option<&Path>) -> String {
    let abs = crate::inspect::job::canonicalize_normalized(abs);
    if let Some(root) = root {
        // Bare canonical paths are verbatim (`\\?\`) on Windows, while a
        // configured root can already be non-verbatim. Normalize both sides
        // before `strip_prefix` so patch responses stay project-relative.
        let root = crate::inspect::job::canonicalize_normalized(root);
        if let Ok(rel) = abs.strip_prefix(&root) {
            return display_slash(rel);
        }
        // A deleted or moved-away file cannot be canonicalized. Resolve its
        // nearest existing ancestor so an aliased root still compares equal,
        // without changing the operation's filePath or the absolute fallback.
        for ancestor in abs.ancestors().skip(1) {
            if let Ok(canonical) = fs::canonicalize(ancestor) {
                let comparison = crate::inspect::job::normalize_path(
                    &canonical.join(abs.strip_prefix(ancestor).unwrap()),
                );
                if let Ok(rel) = comparison.strip_prefix(&root) {
                    return display_slash(rel);
                }
                break;
            }
        }
    }
    display_slash(&abs)
}

fn resolve_path(
    req: &RawRequest,
    ctx: &AppContext,
    path: &str,
    preserve_final_component: bool,
) -> Result<ResolvedPath, Response> {
    // Checked on the raw spelling: once joined onto the project root below,
    // `issue://7` no longer looks like a GitHub address.
    crate::commands::github_comments::reject_github_resource_path(&req.id, "apply_patch", path)?;
    let input = resolve_patch_input(ctx, path);
    let validated = if preserve_final_component {
        ctx.validate_write_location(&req.id, &input)?
    } else {
        ctx.validate_path(&req.id, &input)?
    };
    let abs = if preserve_final_component {
        normalize_write_location(validated)
    } else {
        normalize_resolved_path(validated)
    };
    let root = project_root_for_relative_paths(ctx);
    let rel = if preserve_final_component {
        relative_path_without_final_resolution(&abs, root.as_deref())
    } else {
        relative_path(&abs, root.as_deref())
    };
    Ok(ResolvedPath { abs, rel })
}

fn relative_path_without_final_resolution(abs: &Path, root: Option<&Path>) -> String {
    if let Some(root) = root {
        let root = crate::inspect::job::canonicalize_normalized(root);
        if let Ok(rel) = abs.strip_prefix(root) {
            return display_slash(rel);
        }
    }
    display_slash(abs)
}

fn reject_symlink_delete(
    req: &RawRequest,
    path: &str,
    resolved: &ResolvedPath,
) -> Result<(), Response> {
    let metadata = match fs::symlink_metadata(&resolved.abs) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Response::error(
                &req.id,
                "io_error",
                format!("apply_patch: failed to inspect '{path}': {error}"),
            ));
        }
    };

    if metadata.file_type().is_symlink() {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            format!(
                "apply_patch: refusing to delete symlink '{path}'; symlink undo is not supported"
            ),
        ));
    }

    Ok(())
}

fn remember_path(
    abs: &Path,
    rel: &str,
    affected_abs: &mut Vec<String>,
    affected_rel: &mut Vec<String>,
) {
    let abs_s = path_string(abs);
    if !affected_abs.iter().any(|existing| existing == &abs_s) {
        affected_abs.push(abs_s);
    }
    if !affected_rel.iter().any(|existing| existing == rel) {
        affected_rel.push(rel.to_string());
    }
}

fn resolve_hunks(
    req: &RawRequest,
    ctx: &AppContext,
    hunks: Vec<Hunk>,
) -> Result<(Vec<ResolvedHunk>, Vec<String>, Vec<String>), Response> {
    let mut resolved = Vec::with_capacity(hunks.len());
    let mut affected_abs = Vec::new();
    let mut affected_rel = Vec::new();

    for hunk in hunks {
        let (source_path, move_path, preserve_source_entry) = match &hunk {
            Hunk::Add { path, .. } => (path.as_str(), None, false),
            Hunk::Delete { path } => (path.as_str(), None, true),
            Hunk::Update {
                path, move_path, ..
            } => (path.as_str(), move_path.as_deref(), move_path.is_some()),
        };
        let source = resolve_path(req, ctx, source_path, preserve_source_entry)?;
        if matches!(&hunk, Hunk::Delete { .. }) {
            reject_symlink_delete(req, source_path, &source)?;
        }
        remember_path(
            &source.abs,
            &source.rel,
            &mut affected_abs,
            &mut affected_rel,
        );
        let move_dest = if let Some(move_path) = move_path {
            let dest = resolve_path(req, ctx, move_path, true)?;
            if dest.abs == source.abs {
                // Models can spell an in-place edit as a move back to the same file.
                // Drop the move so apply and preview both use the normal update path.
                None
            } else {
                remember_path(&dest.abs, &dest.rel, &mut affected_abs, &mut affected_rel);
                Some(dest)
            }
        } else {
            None
        };
        resolved.push(ResolvedHunk {
            hunk,
            source,
            move_dest,
        });
    }

    Ok((resolved, affected_abs, affected_rel))
}

fn line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let mut parts = content.split('\n').collect::<Vec<_>>();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    parts.len()
}

fn ensure_parent_dirs(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create directories: {error}"))?;
        }
    }
    Ok(())
}

fn discard_latest_backup(ctx: &AppContext, req: &RawRequest, op_id: &str, path: &Path) {
    ctx.backup()
        .lock()
        .discard_latest_operation_entry_for_path(req.session(), op_id, path);
}

fn snapshot_for_write_once(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    op_id: &str,
    existed: bool,
    description: &str,
    backed_paths: &mut HashSet<PathBuf>,
) -> Result<bool, String> {
    if backed_paths.contains(path) {
        return Ok(false);
    }

    if existed {
        edit::auto_backup(ctx, req.session(), path, description, Some(op_id))
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        ctx.backup()
            .lock()
            .snapshot_op_tombstone(req.session(), op_id, path, description)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }?;
    backed_paths.insert(path.to_path_buf());
    Ok(true)
}

fn restore_pre_write_state(path: &Path, existed: bool, original: Option<&str>) {
    if existed {
        if let Some(original) = original {
            let _ = fs::write(path, original);
        }
    } else if path.exists() {
        let _ = fs::remove_file(path);
    }
}

fn write_patched_file(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    content: &str,
    op_id: &str,
    description: &str,
    backed_paths: &mut HashSet<PathBuf>,
    created_in_call: bool,
    write_warnings: &mut Vec<String>,
) -> Result<PatchedFile, String> {
    let existed = path.exists();
    let original = if existed {
        Some(
            fs::read_to_string(path)
                .map_err(|error| format!("failed to read pre-write content: {error}"))?,
        )
    } else {
        None
    };

    let snapshot_taken =
        snapshot_for_write_once(req, ctx, path, op_id, existed, description, backed_paths)?;
    if let Err(error) = ensure_parent_dirs(path) {
        if snapshot_taken {
            discard_latest_backup(ctx, req, op_id, path);
            backed_paths.remove(path);
        }
        return Err(error);
    }

    let params = command_params(req);
    // A file this patch created has nothing from before the call to protect:
    // it is never rolled back, and its final content is syntax-checked once
    // after every hunk has run (see `created_file_syntax_checks`).
    let written = if created_in_call {
        edit::write_format_deferred_created(tool_policy, path, content, &ctx.config(), params)
    } else {
        edit::write_format_validate_deferred(tool_policy, path, content, &ctx.config(), params)
    };
    let mut write_result = match written {
        Ok(result) => result,
        Err(error) => {
            restore_pre_write_state(path, existed, original.as_deref());
            if snapshot_taken {
                discard_latest_backup(ctx, req, op_id, path);
                backed_paths.remove(path);
            }
            return Err(error.to_string());
        }
    };

    // Warnings from both the write and a syntax rollback reach the reply even
    // when the hunk fails, so a recovery copy or lost mode is never hidden.
    write_warnings.append(&mut write_result.write_warnings);
    if write_result.rolled_back {
        if snapshot_taken {
            discard_latest_backup(ctx, req, op_id, path);
            backed_paths.remove(path);
        }
        return Err("produced invalid syntax (rolled back)".to_string());
    }

    let final_content = fs::read_to_string(path).unwrap_or_else(|_| content.to_string());
    let change_type = if existed {
        FileChangeType::CHANGED
    } else {
        FileChangeType::CREATED
    };
    ctx.lsp_notify_watched_config_file(path, change_type);
    write_result.lsp_outcome = ctx.lsp_post_write(path, &final_content, params);
    Ok(PatchedFile {
        final_content,
        snapshot_taken,
    })
}

struct PatchedFile {
    final_content: String,
    snapshot_taken: bool,
}

fn delete_file_with_backup(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    op_id: &str,
    backed_paths: &mut HashSet<PathBuf>,
) -> Result<bool, String> {
    let snapshot_taken = if backed_paths.contains(path) {
        false
    } else {
        edit::auto_backup(
            ctx,
            req.session(),
            path,
            "apply_patch: pre-delete backup",
            Some(op_id),
        )
        .map_err(|error| error.to_string())?;
        backed_paths.insert(path.to_path_buf());
        true
    };

    if let Err(error) = fs::remove_file(path) {
        if snapshot_taken {
            discard_latest_backup(ctx, req, op_id, path);
            backed_paths.remove(path);
        }
        return Err(format!("failed to delete: {error}"));
    }
    ctx.lsp_notify_watched_config_file(path, FileChangeType::DELETED);
    Ok(snapshot_taken)
}

fn read_required(path: &Path, action: &str, patch_path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("Failed to {action} {patch_path}: {error}"))
}

fn preview_virtual_content(
    virtual_files: &HashMap<PathBuf, Option<String>>,
    path: &Path,
) -> Option<Option<String>> {
    virtual_files.get(path).cloned()
}

fn read_preview_content(
    virtual_files: &HashMap<PathBuf, Option<String>>,
    path: &Path,
    action: &str,
    patch_path: &str,
) -> Result<String, String> {
    if let Some(content) = preview_virtual_content(virtual_files, path) {
        return content.ok_or_else(|| {
            format!(
                "Failed to {action} {patch_path}: file not found: {}",
                path_string(path)
            )
        });
    }
    read_required(path, action, patch_path)
}

fn build_preview_response(
    req: &RawRequest,
    resolved: &[ResolvedHunk],
    affected_abs: Vec<String>,
    affected_rel: Vec<String>,
) -> Response {
    let mut virtual_files: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut patches = Vec::new();
    let filepath = affected_rel
        .first()
        .cloned()
        .unwrap_or_else(|| ".".to_string());

    for resolved_hunk in resolved {
        match &resolved_hunk.hunk {
            Hunk::Add { path, contents } => {
                let virtual_content =
                    preview_virtual_content(&virtual_files, &resolved_hunk.source.abs);
                let exists = virtual_content
                    .map(|content| content.is_some())
                    .unwrap_or_else(|| resolved_hunk.source.abs.exists());
                if exists {
                    return Response::error(
                        &req.id,
                        "invalid_request",
                        format!(
                            "Failed to create {path}: file already exists. Use *** Update File: to modify, or *** Delete File: first if you want to replace it entirely."
                        ),
                    );
                }
                let after = ensure_trailing_newline(contents);
                patches.push(edit::build_unified_diff(
                    &display_slash(&resolved_hunk.source.abs),
                    "",
                    &after,
                ));
                virtual_files.insert(resolved_hunk.source.abs.clone(), Some(after));
            }
            Hunk::Delete { path } => {
                let before = match read_preview_content(
                    &virtual_files,
                    &resolved_hunk.source.abs,
                    "delete",
                    path,
                ) {
                    Ok(content) => content,
                    Err(error) => return Response::error(&req.id, "invalid_request", error),
                };
                patches.push(edit::build_unified_diff(
                    &display_slash(&resolved_hunk.source.abs),
                    &before,
                    "",
                ));
                virtual_files.insert(resolved_hunk.source.abs.clone(), None);
            }
            Hunk::Update {
                path,
                chunks,
                move_path: _,
            } => {
                let before = match read_preview_content(
                    &virtual_files,
                    &resolved_hunk.source.abs,
                    "update",
                    path,
                ) {
                    Ok(content) => content,
                    Err(error) => return Response::error(&req.id, "invalid_request", error),
                };
                let after = match apply_update_chunks(
                    &before,
                    &path_string(&resolved_hunk.source.abs),
                    chunks,
                ) {
                    Ok(content) => content,
                    Err(error) => {
                        return Response::error(
                            &req.id,
                            "invalid_request",
                            format!("Failed to update {path}: {error}"),
                        );
                    }
                };
                let target = resolved_hunk
                    .move_dest
                    .as_ref()
                    .unwrap_or(&resolved_hunk.source);
                patches.push(edit::build_unified_diff(
                    &display_slash(&target.abs),
                    &before,
                    &after,
                ));
                if resolved_hunk.move_dest.is_some() {
                    virtual_files.insert(resolved_hunk.source.abs.clone(), None);
                }
                virtual_files.insert(target.abs.clone(), Some(after));
            }
        }
    }

    Response::success(
        &req.id,
        json!({
            "preview": true,
            "preview_diff": patches.join("\n"),
            "affected_paths": affected_abs,
            "affected_rel_paths": affected_rel,
            "filepath": filepath,
        }),
    )
}

fn ensure_trailing_newline(content: &str) -> String {
    if content.ends_with('\n') {
        content.to_string()
    } else {
        format!("{content}\n")
    }
}

fn add_failure(failures: &mut Vec<Value>, path: &str, error: String) {
    failures.push(json!({ "path": path, "error": error }));
}

/// Check the final content of every file this patch created and still exists,
/// including why a compiler confirmation could not establish its validity.
///
/// The check runs once per file after all hunks, because a later hunk in the
/// same patch can fix, break, move or delete a file an earlier hunk created.
fn created_file_syntax_checks(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    created: &HashMap<PathBuf, usize>,
) -> HashMap<PathBuf, edit::CreatedFileSyntax> {
    created
        .keys()
        .filter_map(|path| {
            let content = fs::read_to_string(path).ok()?;
            let check = edit::check_created_file_syntax(tool_policy, path, &content);
            Some((path.clone(), check))
        })
        .collect()
}

/// Put the created-file syntax notice directly under the reply line that
/// created each broken or unconfirmed file ("Created" or "Updated and moved").
/// A patch can create several files and the notice itself does not name the
/// file. `created` maps each created file to the index of that reply line.
fn insert_created_syntax_notices(
    output_lines: &mut Vec<String>,
    created: &HashMap<PathBuf, usize>,
    checks: &HashMap<PathBuf, edit::CreatedFileSyntax>,
) {
    let mut notices: Vec<(usize, String)> = checks
        .iter()
        .filter_map(|(path, check)| {
            let notice = if let Some(line) = check.first_error_line {
                edit::created_file_syntax_error_notice(line as u64)
            } else {
                edit::syntax_unknown_notice(check.skipped_reason?)
            };
            Some((*created.get(path)?, notice))
        })
        .collect();
    // Insert from the bottom up so earlier reply-line indices stay valid.
    notices.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (reply_index, notice) in notices {
        output_lines.insert(reply_index + 1, notice);
    }
}

fn failure_paths(failures: &[Value]) -> String {
    failures
        .iter()
        .filter_map(|failure| failure.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

fn apply_add(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    req: &RawRequest,
    ctx: &AppContext,
    resolved: &ResolvedHunk,
    _path: &str,
    contents: &str,
    op_id: &str,
    backed_paths: &mut HashSet<PathBuf>,
    write_warnings: &mut Vec<String>,
) -> Result<AppliedHunkResult, String> {
    if resolved.source.abs.exists() {
        return Err(
            "file already exists. Use *** Update File: to modify, or *** Delete File: first if you want to replace it entirely."
                .to_string(),
        );
    }

    let after = ensure_trailing_newline(contents);
    let written = write_patched_file(
        tool_policy,
        req,
        ctx,
        &resolved.source.abs,
        &after,
        op_id,
        "apply_patch: file created by add hunk",
        backed_paths,
        true,
        write_warnings,
    )?;
    Ok(AppliedHunkResult {
        index: 0,
        kind: "add",
        file_path: resolved.source.abs.clone(),
        display_path: resolved.source.abs.clone(),
        move_path: None,
        before: String::new(),
        after: written.final_content,
        delete_lines: None,
    })
}

fn apply_delete(
    req: &RawRequest,
    ctx: &AppContext,
    resolved: &ResolvedHunk,
    _path: &str,
    op_id: &str,
    backed_paths: &mut HashSet<PathBuf>,
) -> Result<AppliedHunkResult, String> {
    if !resolved.source.abs.exists() {
        return Err("file not found".to_string());
    }
    if !resolved.source.abs.is_file() {
        return Err("not a regular file".to_string());
    }

    let before = fs::read_to_string(&resolved.source.abs)
        .map_err(|error| format!("failed to read before delete: {error}"))?;
    let deletions = line_count(&before);
    delete_file_with_backup(req, ctx, &resolved.source.abs, op_id, backed_paths)?;
    Ok(AppliedHunkResult {
        index: 0,
        kind: "delete",
        file_path: resolved.source.abs.clone(),
        display_path: resolved.source.abs.clone(),
        move_path: None,
        before,
        after: String::new(),
        delete_lines: Some(deletions),
    })
}

/// `target_created_in_call` is true when the file this hunk writes (the move
/// destination, or the source when there is no move) did not exist before
/// this patch started.
fn apply_update(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    req: &RawRequest,
    ctx: &AppContext,
    resolved: &ResolvedHunk,
    chunks: &[crate::patch::parser::UpdateFileChunk],
    op_id: &str,
    backed_paths: &mut HashSet<PathBuf>,
    target_created_in_call: bool,
    write_warnings: &mut Vec<String>,
) -> Result<AppliedHunkResult, String> {
    let original = fs::read_to_string(&resolved.source.abs)
        .map_err(|error| format!("failed to read file: {error}"))?;
    let new_content = apply_update_chunks(&original, &path_string(&resolved.source.abs), chunks)?;

    if let Some(dest) = &resolved.move_dest {
        apply_move_update(
            tool_policy,
            req,
            ctx,
            resolved,
            dest,
            original,
            new_content,
            op_id,
            backed_paths,
            target_created_in_call,
            write_warnings,
        )
    } else {
        let written = write_patched_file(
            tool_policy,
            req,
            ctx,
            &resolved.source.abs,
            &new_content,
            op_id,
            "apply_patch: pre-update backup",
            backed_paths,
            target_created_in_call,
            write_warnings,
        )?;
        Ok(AppliedHunkResult {
            index: 0,
            kind: "update",
            file_path: resolved.source.abs.clone(),
            display_path: resolved.source.abs.clone(),
            move_path: None,
            before: original,
            after: written.final_content,
            delete_lines: None,
        })
    }
}

fn apply_move_update(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    req: &RawRequest,
    ctx: &AppContext,
    resolved: &ResolvedHunk,
    dest: &ResolvedPath,
    original: String,
    new_content: String,
    op_id: &str,
    backed_paths: &mut HashSet<PathBuf>,
    dest_created_in_call: bool,
    write_warnings: &mut Vec<String>,
) -> Result<AppliedHunkResult, String> {
    let dest_existed = dest.abs.exists();
    let dest_snapshot = if dest_existed {
        Some(
            fs::read_to_string(&dest.abs)
                .map_err(|error| format!("move: failed to read destination snapshot: {error}"))?,
        )
    } else {
        None
    };

    let written = match write_patched_file(
        tool_policy,
        req,
        ctx,
        &dest.abs,
        &new_content,
        op_id,
        "apply_patch: move destination backup",
        backed_paths,
        dest_created_in_call,
        write_warnings,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            if !dest_existed && dest.abs.exists() {
                let _ = fs::remove_file(&dest.abs);
            }
            return Err(error);
        }
    };
    let dest_snapshot_taken = written.snapshot_taken;

    let source_snapshot_taken = if backed_paths.contains(&resolved.source.abs) {
        false
    } else {
        if let Err(error) = edit::auto_backup(
            ctx,
            req.session(),
            &resolved.source.abs,
            "apply_patch: move source backup",
            Some(op_id),
        ) {
            let rollback = rollback_move_destination(
                req,
                ctx,
                op_id,
                &dest.abs,
                dest_existed,
                dest_snapshot.as_deref(),
                dest_snapshot_taken,
                backed_paths,
            );
            return Err(match rollback {
                Ok(()) => error.to_string(),
                Err(restore_error) => {
                    format!("{error}; destination rollback failed: {restore_error}")
                }
            });
        }
        backed_paths.insert(resolved.source.abs.clone());
        true
    };

    if let Err(error) = fs::remove_file(&resolved.source.abs) {
        if source_snapshot_taken {
            discard_latest_backup(ctx, req, op_id, &resolved.source.abs);
            backed_paths.remove(&resolved.source.abs);
        }
        let rollback = rollback_move_destination(
            req,
            ctx,
            op_id,
            &dest.abs,
            dest_existed,
            dest_snapshot.as_deref(),
            dest_snapshot_taken,
            backed_paths,
        );
        let message = format!("move: failed to remove source after writing destination: {error}");
        return Err(match rollback {
            Ok(()) => message,
            Err(restore_error) => {
                format!("{message}; destination rollback failed: {restore_error}")
            }
        });
    }
    ctx.lsp_notify_watched_config_file(&resolved.source.abs, FileChangeType::DELETED);

    Ok(AppliedHunkResult {
        index: 0,
        kind: "update",
        file_path: resolved.source.abs.clone(),
        display_path: dest.abs.clone(),
        move_path: Some(dest.abs.clone()),
        before: original,
        after: written.final_content,
        delete_lines: None,
    })
}

fn rollback_move_destination(
    req: &RawRequest,
    ctx: &AppContext,
    op_id: &str,
    dest: &Path,
    dest_existed: bool,
    dest_snapshot: Option<&str>,
    dest_snapshot_taken: bool,
    backed_paths: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    if dest_existed {
        if let Some(snapshot) = dest_snapshot {
            fs::write(dest, snapshot).map_err(|error| error.to_string())?;
        }
    } else if dest.exists() {
        fs::remove_file(dest).map_err(|error| error.to_string())?;
    }
    // Recovery data is only expendable after the destination is restored.
    if dest_snapshot_taken {
        discard_latest_backup(ctx, req, op_id, dest);
        backed_paths.remove(dest);
    }
    Ok(())
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[test]
    fn patch_move_restores_destination_after_source_backup_error() {
        for existed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.txt");
            fs::create_dir(&source).unwrap();
            let destination = dir.path().join("destination.txt");
            if existed {
                fs::write(&destination, "destination before").unwrap();
            }
            let ctx = AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    format_on_edit: false,
                    ..Default::default()
                },
            );
            let request =
                serde_json::from_value(json!({"id": "backup-error", "command": "apply_patch"}))
                    .unwrap();
            let resolved = ResolvedHunk {
                hunk: Hunk::Delete {
                    path: "source.txt".into(),
                },
                source: ResolvedPath {
                    abs: source.clone(),
                    rel: "source.txt".into(),
                },
                move_dest: None,
            };
            let dest = ResolvedPath {
                abs: destination.clone(),
                rel: "destination.txt".into(),
            };
            let mut backed = HashSet::new();
            assert!(apply_move_update(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                &request,
                &ctx,
                &resolved,
                &dest,
                "source before".into(),
                "destination after".into(),
                "failed-move",
                &mut backed,
                false,
                &mut Vec::new()
            )
            .is_err());
            if existed {
                assert_eq!(
                    fs::read_to_string(destination).unwrap(),
                    "destination before"
                );
            } else {
                assert!(!destination.exists());
            }
            assert!(source.is_dir());
            assert!(!backed.contains(&dest.abs));
        }
    }
}

fn report_key(applied: &AppliedHunkResult) -> String {
    if let Some(move_path) = &applied.move_path {
        format!(
            "{}\0{}",
            path_string(&applied.file_path),
            path_string(move_path)
        )
    } else {
        path_string(&applied.file_path)
    }
}

/// Carry each created file's final syntax verdict into its metadata entry.
fn metadata_files(
    applied: &[AppliedHunkResult],
    root: Option<&Path>,
    created_checks: &HashMap<PathBuf, edit::CreatedFileSyntax>,
) -> (String, Vec<Value>) {
    let mut entries: Vec<(String, DiffEntry)> = Vec::new();

    for applied_hunk in applied {
        let key = report_key(applied_hunk);
        if let Some((_, entry)) = entries.iter_mut().find(|(existing, _)| existing == &key) {
            entry.display_path = applied_hunk.display_path.clone();
            if applied_hunk.move_path.is_some() {
                entry.move_path = applied_hunk.move_path.clone();
            }
            entry.last_kind = applied_hunk.kind;
            entry.after = applied_hunk.after.clone();
            entry.hunk_count += 1;
        } else {
            entries.push((
                key,
                DiffEntry {
                    file_path: applied_hunk.file_path.clone(),
                    display_path: applied_hunk.display_path.clone(),
                    move_path: applied_hunk.move_path.clone(),
                    last_kind: applied_hunk.kind,
                    before: applied_hunk.before.clone(),
                    after: applied_hunk.after.clone(),
                    delete_lines: applied_hunk.delete_lines,
                    hunk_count: 1,
                    syntax_error_line: None,
                    syntax_skipped_reason: None,
                },
            ));
        }
    }

    // Carry each created file's syntax result on the last entry that
    // ends at that path, so a file reported by several entries (for example
    // moved onto, then updated) carries it once.
    let mut marked = HashSet::new();
    for (_, entry) in entries.iter_mut().rev() {
        if let Some(check) = created_checks.get(&entry.display_path) {
            if marked.insert(entry.display_path.clone()) {
                entry.syntax_error_line = check.first_error_line;
                entry.syntax_skipped_reason = check.skipped_reason;
            }
        }
    }

    let files = entries
        .into_iter()
        .map(|(_, entry)| {
            let (patch, counts) = edit::response_diff(
                &Value::Null,
                &display_slash(&entry.display_path),
                &entry.before,
                &entry.after,
            );
            let mut additions = counts["additions"].as_u64().unwrap() as usize;
            let mut deletions = counts["deletions"].as_u64().unwrap() as usize;
            // A lone delete counts LF-separated lines, counting a nonempty
            // unterminated final line once. Keep that deletion-only convention;
            // multiple hunks report the original-to-final line diff instead.
            if entry.hunk_count == 1 {
                if let Some(lines) = entry.delete_lines {
                    additions = 0;
                    deletions = lines;
                }
            }
            let entry_type = if entry.move_path.is_some() {
                "move"
            } else if entry.hunk_count == 1 {
                entry.last_kind
            } else if entry.before.is_empty() && !entry.after.is_empty() {
                "add"
            } else if !entry.before.is_empty() && entry.after.is_empty() {
                "delete"
            } else {
                "update"
            };
            let mut value = json!({
                "filePath": path_string(&entry.file_path),
                "relativePath": relative_path(&entry.display_path, root),
                "type": entry_type,
                "patch": patch,
                "additions": additions,
                "deletions": deletions,
            });
            if let Some(move_path) = entry.move_path {
                value["movePath"] = json!(display_slash(&move_path));
            }
            if let Some(line) = entry.syntax_error_line {
                value[edit::CREATED_SYNTAX_ERROR_LINE_FIELD] = json!(line);
            }
            edit::append_syntax_skipped_reason(&mut value, entry.syntax_skipped_reason);
            value
        })
        .collect::<Vec<_>>();

    let diff = files
        .iter()
        .filter_map(|file| file.get("patch").and_then(Value::as_str))
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    (diff, files)
}

fn apply_patch(req: &RawRequest, ctx: &AppContext, resolved: &[ResolvedHunk]) -> Response {
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);

    let op_id = crate::backup::new_op_id();
    let mut backed_paths = HashSet::new();
    let mut output_lines = Vec::new();
    let mut failures = Vec::new();
    let mut applied = Vec::new();
    // Warnings from every file write, including writes of hunks that failed.
    let mut write_warnings: Vec<String> = Vec::new();
    // Files this patch created that still exist, each mapped to the index of
    // the reply line that created it. A later hunk that deletes or moves the
    // file away removes it; the syntax check reads the final content after
    // the loop.
    let mut created: HashMap<PathBuf, usize> = HashMap::new();

    for (index, resolved_hunk) in resolved.iter().enumerate() {
        match &resolved_hunk.hunk {
            Hunk::Add { path, contents } => {
                match apply_add(
                    tool_policy,
                    req,
                    ctx,
                    resolved_hunk,
                    path,
                    contents,
                    &op_id,
                    &mut backed_paths,
                    &mut write_warnings,
                ) {
                    Ok(mut result) => {
                        result.index = index;
                        created.insert(resolved_hunk.source.abs.clone(), output_lines.len());
                        output_lines.push(format!("Created {path}"));
                        applied.push(result);
                    }
                    Err(error) => {
                        output_lines.push(format!("Failed to create {path}: {error}"));
                        add_failure(&mut failures, path, error);
                    }
                }
            }
            Hunk::Delete { path } => {
                match apply_delete(req, ctx, resolved_hunk, path, &op_id, &mut backed_paths) {
                    Ok(mut result) => {
                        result.index = index;
                        created.remove(&resolved_hunk.source.abs);
                        output_lines.push(format!("Deleted {path}"));
                        applied.push(result);
                    }
                    Err(error) => {
                        output_lines.push(format!("Failed to delete {path}: {error}"));
                        add_failure(&mut failures, path, error);
                    }
                }
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let target_created_in_call = match &resolved_hunk.move_dest {
                    Some(dest) => created.contains_key(&dest.abs) || !dest.abs.exists(),
                    None => created.contains_key(&resolved_hunk.source.abs),
                };
                match apply_update(
                    tool_policy,
                    req,
                    ctx,
                    resolved_hunk,
                    chunks,
                    &op_id,
                    &mut backed_paths,
                    target_created_in_call,
                    &mut write_warnings,
                ) {
                    Ok(mut result) => {
                        result.index = index;
                        if let (Some(dest), Some(move_path)) = (&resolved_hunk.move_dest, move_path)
                        {
                            created.remove(&resolved_hunk.source.abs);
                            if target_created_in_call {
                                created.insert(dest.abs.clone(), output_lines.len());
                            }
                            output_lines.push(format!("Updated and moved {path} → {move_path}"));
                        } else {
                            output_lines.push(format!("Updated {path}"));
                        }
                        applied.push(result);
                    }
                    Err(error) => {
                        output_lines.push(format!("Failed to update {path}: {error}"));
                        add_failure(&mut failures, path, error);
                    }
                }
            }
        }
    }

    let created_checks = created_file_syntax_checks(tool_policy, &created);
    insert_created_syntax_notices(&mut output_lines, &created, &created_checks);

    if !failures.is_empty() {
        let partial = failures.len() < resolved.len();
        let failed_list = failure_paths(&failures);
        let summary = if partial {
            format!(
                "Patch partially applied — {} of {} hunk(s) succeeded. Failed: {failed_list}. Successful changes are kept; use `aft_safety` to revert if you want to abort.",
                resolved.len() - failures.len(),
                resolved.len()
            )
        } else {
            format!(
                "Patch failed — none of the {} hunk(s) applied: {failed_list}.",
                resolved.len()
            )
        };
        output_lines.push(summary);
    }

    let root = project_root_for_relative_paths(ctx);
    let (diff, mut files) = metadata_files(&applied, root.as_deref(), &created_checks);
    // A partial patch keeps successful writes, but is not a completed project
    // mutation. Do not run checkers when any hunk failed or rolled back.
    if failures.is_empty() {
        let paths: Vec<PathBuf> = applied
            .iter()
            .filter(|hunk| hunk.kind != "delete" && hunk.display_path.is_file())
            .map(|hunk| hunk.display_path.clone())
            .collect();
        if let Some(validation) =
            edit::validate_written_files(tool_policy, &paths, &ctx.config(), command_params(req))
        {
            for file in &mut files {
                let path = file
                    .get("movePath")
                    .or_else(|| file.get("filePath"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                if let Some(path) = path {
                    validation.append_to(&path, file);
                }
            }
            output_lines.push(validation.summary());
        }
    }
    let output = output_lines.join("\n");

    if applied.is_empty() && !failures.is_empty() {
        let mut data = json!({
            "output": output,
            "complete": false,
            "all_failed": true,
            "partial": false,
            "failures": failures,
            "metadata": { "diff": "", "files": [] },
        });
        if !write_warnings.is_empty() {
            data["warnings"] = json!(write_warnings);
        }
        return Response::error_with_data(
            req.id.clone(),
            "apply_patch_failed",
            output.clone(),
            data,
        );
    }

    let complete = failures.is_empty();
    let title = if complete {
        format!("Applied {} hunks", resolved.len())
    } else {
        format!("Applied {} of {} hunks", applied.len(), resolved.len())
    };

    let mut result = json!({
        "output": output,
        "title": title,
        "complete": complete,
        "partial": !complete,
        "all_failed": false,
        "failures": failures,
        "metadata": { "diff": diff, "files": files },
    });
    if !write_warnings.is_empty() {
        result["warnings"] = json!(write_warnings);
    }
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), &op_id, None);
    Response::success(&req.id, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn short_path(path: &Path) -> PathBuf {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the input is NUL-terminated; a null output queries the buffer size.
        let size = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        assert_ne!(
            size,
            0,
            "GetShortPathNameW: {}",
            std::io::Error::last_os_error()
        );
        let mut buffer = vec![0; size as usize];
        // SAFETY: the output buffer has the capacity reported by the first call.
        let written = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), size) };
        assert!(written > 0 && written < size, "GetShortPathNameW failed");
        PathBuf::from(std::ffi::OsString::from_wide(&buffer[..written as usize]))
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn metadata_relative_paths_stay_relative_under_an_aliased_root() {
        let temp = tempfile::tempdir().expect("create temporary project");
        let root = temp.path().join("project with a long name");
        fs::create_dir_all(root.join("src")).unwrap();
        let root = crate::inspect::job::canonicalize_normalized(&root);
        #[cfg(unix)]
        let alias = {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            alias
        };
        #[cfg(windows)]
        let alias = short_path(&root);
        // Short names are a per-volume setting; where they are turned off,
        // GetShortPathNameW returns the long path and there is no alias to
        // test. The verbatim-prefix case below is covered by its own test.
        #[cfg(windows)]
        if alias == root {
            eprintln!("skipped: the temp volume does not create 8.3 short names");
            return;
        }
        assert_ne!(
            alias, root,
            "the fixture must exercise a distinct root spelling"
        );
        assert_eq!(crate::inspect::job::canonicalize_normalized(&alias), root);

        fs::write(root.join("src/live.ts"), "export const value = 1;\n").unwrap();
        fs::write(root.join("src/deleted.ts"), "before\n").unwrap();
        fs::remove_file(root.join("src/deleted.ts")).unwrap();
        fs::create_dir_all(root.join("removed/nested")).unwrap();
        fs::write(root.join("removed/nested/gone.ts"), "before\n").unwrap();
        fs::remove_dir_all(root.join("removed")).unwrap();

        for rel in ["src/live.ts", "src/deleted.ts", "removed/nested/gone.ts"] {
            let file_path = alias.join(rel);
            let applied = [AppliedHunkResult {
                index: 0,
                file_path: file_path.clone(),
                display_path: file_path.clone(),
                move_path: None,
                kind: "delete",
                before: "before\n".into(),
                after: String::new(),
                delete_lines: Some(1),
            }];
            let (_, files) = metadata_files(&applied, Some(&root), &HashMap::new());
            assert_eq!(files.len(), 1);
            assert_eq!(files[0]["filePath"], path_string(&file_path));
            assert_eq!(files[0]["relativePath"], rel, "metadata for {rel}");
            assert_eq!(relative_path(&root.join(rel), Some(&alias)), rel);
            #[cfg(windows)]
            assert_eq!(relative_path(&file_path, Some(&verbatim_path(&root))), rel);
        }
    }

    #[cfg(windows)]
    fn verbatim_path(path: &Path) -> PathBuf {
        PathBuf::from(format!(r"\\?\{}", path.display()))
    }

    #[test]
    fn a03_patch_final_pair_diff_bound() {
        let path = PathBuf::from("edge.ts");
        let applied: Vec<_> = ["middle\r\n", "final\n", "é\n"]
            .into_iter()
            .map(|after| AppliedHunkResult {
                index: 0,
                file_path: path.clone(),
                display_path: path.clone(),
                move_path: None,
                kind: "update",
                before: "original\n".into(),
                after: after.into(),
                delete_lines: None,
            })
            .collect();
        edit::A03_DIFF_CONSTRUCTIONS.with(|count| count.set(0));
        let (diff, files) = metadata_files(&applied, None, &HashMap::new());
        assert_eq!(
            files,
            vec![
                json!({"filePath": "edge.ts", "relativePath": "edge.ts", "type": "update", "patch": "Index: edge.ts\n===================================================================\n--- edge.ts\n+++ edge.ts\n@@ -1 +1 @@\n-original\n+é\n", "additions": 1, "deletions": 1})
            ]
        );
        assert_eq!(diff, files[0]["patch"].as_str().unwrap());
        assert_eq!(
            edit::A03_DIFF_CONSTRUCTIONS.with(|count| count.get()),
            1,
            "one final aggregated pair computation"
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalize_resolved_path_strips_windows_verbatim_prefix() {
        let temp = tempfile::tempdir().expect("create temporary project");
        let file = temp.path().join("src/main.rs");
        fs::create_dir_all(file.parent().expect("source directory"))
            .expect("create source directory");
        fs::write(&file, "fn main() {}\n").expect("write source file");

        let normalized = crate::inspect::job::canonicalize_normalized(&file);
        let verbatim = verbatim_path(&normalized);

        assert_eq!(normalize_resolved_path(verbatim), normalized);
    }

    #[cfg(windows)]
    #[test]
    fn relative_path_matches_a_verbatim_root_to_a_normalized_file() {
        let temp = tempfile::tempdir().expect("create temporary project");
        let file = temp.path().join("src/main.rs");
        fs::create_dir_all(file.parent().expect("source directory"))
            .expect("create source directory");
        fs::write(&file, "fn main() {}\n").expect("write source file");

        let normalized_root = crate::inspect::job::canonicalize_normalized(temp.path());
        let normalized_file = crate::inspect::job::canonicalize_normalized(&file);

        assert_eq!(
            relative_path(&normalized_file, Some(&verbatim_path(&normalized_root))),
            "src/main.rs"
        );
    }
}

/// Handle a raw `apply_patch` request.
pub fn handle_apply_patch(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = command_params(req);
    let patch_text = match params.get("patch_text").and_then(Value::as_str) {
        Some(patch_text) => patch_text,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "apply_patch: missing required param 'patch_text'",
            );
        }
    };

    let hunks = match parse_patch(patch_text) {
        Ok(hunks) => hunks,
        Err(error) => return Response::error(&req.id, "invalid_request", error),
    };
    if hunks.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "Empty patch: no file operations found",
        );
    }

    let (resolved, affected_abs, affected_rel) = match resolve_hunks(req, ctx, hunks) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    if edit::wants_preview(params) {
        return build_preview_response(req, &resolved, affected_abs, affected_rel);
    }

    let _mutation = crate::runtime_drain::record_file_mutation(
        ctx,
        affected_abs.iter().map(|path| std::path::Path::new(path)),
    );
    apply_patch(req, ctx, &resolved)
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn ambiguous_update_reports_refusal_and_preserves_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let original = "\u{feff}head\r\n  target\r\nother\r\n\ttarget\r\n";
        std::fs::write(&path, original.as_bytes()).unwrap();
        let patch = format!("*** Begin Patch\n*** Update File: {}\n@@\n-\u{feff}head\n+\u{feff}changed\n@@\n-target\n+replacement\n*** End Patch", path.display());
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(dir.path().join("state")),
                format_on_edit: false,
                ..Default::default()
            },
        );
        let req = RawRequest {
            id: "match-guard".into(),
            command: "apply_patch".into(),
            params: serde_json::json!({"patch_text": patch, "validate": "off"}),
            lsp_hints: None,
            session_id: None,
        };
        let response = serde_json::to_value(handle_apply_patch(&req, &ctx)).unwrap();
        assert_eq!(response["code"], "apply_patch_failed", "{response}");
        assert!(response["failures"][0]["error"]
            .as_str()
            .unwrap()
            .contains("Ambiguous non-exact match"));
        assert_eq!(std::fs::read(path).unwrap(), original.as_bytes());
    }

    #[test]
    fn malformed_later_update_is_rejected_before_any_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let original = b"\xef\xbb\xbfhead\r\nlast";
        let binary = b"\xffuntouched\r\n";
        std::fs::write(&first, original).unwrap();
        std::fs::write(&second, binary).unwrap();
        let patch = format!("*** Begin Patch\n*** Update File: {}\n@@\n-\u{feff}head\n+\u{feff}changed\n*** Update File: {}\n@@\ngarbage\n*** End Patch", first.display(), second.display());
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(dir.path().join("state")),
                format_on_edit: false,
                ..Default::default()
            },
        );
        let req = RawRequest {
            id: "parse-guard".into(),
            command: "apply_patch".into(),
            params: serde_json::json!({"patch_text": patch, "validate": "off"}),
            lsp_hints: None,
            session_id: None,
        };
        let response = serde_json::to_value(handle_apply_patch(&req, &ctx)).unwrap();
        assert_eq!(response["code"], "invalid_request", "{response}");
        assert!(response["message"].as_str().unwrap().contains("line 8"));
        assert_eq!(std::fs::read(first).unwrap(), original);
        assert_eq!(std::fs::read(second).unwrap(), binary);
    }
}

/// Write warnings must reach the reply even when a hunk fails, so a rolled
/// back write cannot hide a recovery copy or a mode it failed to restore.
#[cfg(all(test, unix))]
mod write_warning_tests {
    use super::*;

    #[test]
    fn rolled_back_hunk_reports_its_write_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ts");
        let original = "export const value = 1;\n";
        std::fs::write(&path, original).unwrap();
        // A second name keeps the write in place, where the mode is checked.
        std::fs::hard_link(&path, dir.path().join("b.ts")).unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-export const value = 1;\n+export const value = {{;\n*** End Patch",
            path.display()
        );
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(dir.path().join("state")),
                format_on_edit: false,
                ..Default::default()
            },
        );
        let req = RawRequest {
            id: "rollback-warnings".into(),
            command: "apply_patch".into(),
            params: serde_json::json!({"patch_text": patch, "validate": "off"}),
            lsp_hints: None,
            session_id: None,
        };
        edit::FORCE_MODE_RESTORE_FAILURE.with(|forced| forced.set(true));
        let response = serde_json::to_value(handle_apply_patch(&req, &ctx)).unwrap();
        edit::FORCE_MODE_RESTORE_FAILURE.with(|forced| forced.set(false));
        assert_eq!(response["code"], "apply_patch_failed", "{response}");
        assert!(
            response["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("invalid syntax"),
            "{response}"
        );
        let warnings = response["warnings"].as_array().expect("warnings array");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().starts_with("mode_not_restored: ")),
            "{response}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
