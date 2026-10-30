use std::path::PathBuf;

use crate::checkpoint::checkpoint_durability;
use crate::commands::restore_checkpoint::requested_restore_paths;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle the `checkpoint` command: create a named workspace checkpoint.
///
/// Params:
/// - `name` (string, required) — checkpoint name.
/// - `files` (array of strings, optional) — files to include. If omitted, uses
///   all files tracked by the backup store.
///
/// Returns: `{ name, file_count, paths, created_at, storage_path, durability }`,
/// plus `requested_count` when `files` was given. `paths` lists the snapshotted
/// files. `storage_path` names the durable on-disk directory callers can inspect.
/// Explicit files are read directly, including untracked or gitignored files. When any requested
/// file cannot be read, `file_count` includes only restorable snapshots and the
/// response adds `skipped: [{ file, error }, ...]` for every omitted path. When
/// none of the requested files can be read, the command fails instead.
pub fn handle_checkpoint(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_checkpoint_impl(req, ctx) {
        Ok(resp) | Err(resp) => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_checkpoint_command_reports_discovery_errors_without_replacing_history() {
        for released in [false, true] {
            for expired in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let path = root.path().join("tracked.txt");
                let second = root.path().join("second.txt");
                std::fs::write(&path, b"restore me").unwrap();
                std::fs::write(&second, b"second").unwrap();
                let ctx = AppContext::new(
                    Box::new(crate::parser::TreeSitterProvider::new()),
                    crate::config::Config {
                        project_root: Some(root.path().to_path_buf()),
                        ..Default::default()
                    },
                );
                ctx.backup()
                    .lock()
                    .set_storage_dir(root.path().join("backup"), 72);
                ctx.checkpoint().lock().set_storage_dir_for_harness(
                    root.path().join("checkpoints"),
                    crate::harness::Harness::Opencode,
                );
                let original: RawRequest = serde_json::from_value(serde_json::json!({
                    "id": "original", "command": "checkpoint", "name": "useful", "files": [path],
                }))
                .unwrap();
                assert!(handle_checkpoint(&original, &ctx).success);
                {
                    let mut backup = ctx.backup().lock();
                    backup
                        .snapshot(original.session(), &path, "tracked")
                        .unwrap();
                    backup
                        .snapshot(original.session(), &second, "tracked")
                        .unwrap();
                    if released {
                        backup.release_memory();
                    }
                    if expired {
                        backup.expire_discovery_for_tests();
                    } else {
                        backup.set_discovery_entry_limit_for_tests(0);
                    }
                }
                std::fs::write(&path, b"changed").unwrap();
                let replacement = serde_json::from_value(serde_json::json!({
                    "id": "replace", "command": "checkpoint", "name": "useful",
                }))
                .unwrap();
                let response = handle_checkpoint(&replacement, &ctx);
                assert!(!response.success, "{response:?}");
                assert_eq!(response.data["code"], "io_error", "{response:?}");
                assert!(response.data.get("file_count").is_none(), "{response:?}");
                ctx.checkpoint()
                    .lock()
                    .restore(original.session(), "useful")
                    .unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), b"restore me");
            }
        }
    }
}

fn handle_checkpoint_impl(req: &RawRequest, ctx: &AppContext) -> Result<Response, Response> {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Ok(Response::error(
                &req.id,
                "invalid_request",
                "checkpoint: missing required param 'name'",
            ));
        }
    };

    let files: Vec<PathBuf> = req
        .params
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();

    let explicit_request = !files.is_empty();
    let file_list = if files.is_empty() {
        let backup = ctx.backup().lock();
        backup
            .try_tracked_files(req.session())
            .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?
    } else {
        files
    };

    let validated_files = validate_checkpoint_files(&req.id, ctx, file_list)?;
    // Two spellings of one file count once, so `requested_count` compares
    // like with like against the snapshot count.
    let requested_count = validated_files.len();

    let backup = ctx.backup().lock();
    let mut checkpoint_store = ctx.checkpoint().lock();

    match checkpoint_store.create(req.session(), name, validated_files, &backup) {
        Ok(info) => {
            let storage_path = info
                .storage_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            // Only surface `skipped` when we actually skipped something. Keeps
            // happy-path responses compact and backward-compatible for callers
            // that only read `name` / `file_count` / `created_at`.
            let mut payload = serde_json::json!({
                "name": info.name,
                "file_count": info.file_count,
                "created_at": info.created_at,
                "storage_path": storage_path,
                "durability": checkpoint_durability(std::path::Path::new(&storage_path)),
                "paths": info
                    .paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
            });
            if explicit_request {
                payload["requested_count"] = serde_json::json!(requested_count);
            }
            if !info.evicted.is_empty() {
                payload["evicted"] = serde_json::json!(info.evicted);
            }
            if !info.skipped.is_empty() {
                let skipped: Vec<_> = info
                    .skipped
                    .iter()
                    .map(|(p, err)| {
                        serde_json::json!({
                            "file": p.display().to_string(),
                            "error": err,
                        })
                    })
                    .collect();
                payload["skipped"] = serde_json::Value::Array(skipped);
            }
            Ok(Response::success(&req.id, payload))
        }
        Err(e) => Ok(Response::error(&req.id, e.code(), e.to_string())),
    }
}

fn validate_checkpoint_files(
    req_id: &str,
    ctx: &AppContext,
    files: Vec<PathBuf>,
) -> Result<Vec<PathBuf>, Response> {
    let mut validated = Vec::with_capacity(files.len());
    for path in files {
        // Resolve relative paths against the bound project root so the
        // checkpoint key matches the path the mutating tool recorded. A relative
        // path passed straight to `canonicalize_key` would be joined against the
        // daemon's cwd and miss the snapshot.
        let input = ctx.resolve_relative_path(&path);
        // Creation and restore must authorize and key the same final object.
        // Resolving only ancestors preserves a final symlink for the snapshot
        // reader while still rejecting symlinked parents that escape the root.
        let validated_path = ctx.validate_write_location(req_id, &input)?;
        if !validated.contains(&validated_path) {
            validated.push(validated_path);
        }
    }
    Ok(validated)
}

/// Handle the `checkpoint_paths` command: return paths a checkpoint restore would write.
///
/// Params: `name` (string, required) — checkpoint name, plus optional `file`/`files` scope.
/// Returns: `{ name, paths, file_count }` without mutating checkpoint or filesystem state.
pub fn handle_checkpoint_paths(req: &RawRequest, ctx: &AppContext) -> Response {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "checkpoint_paths: missing required param 'name'",
            );
        }
    };

    let requested_paths = match requested_restore_paths(req, ctx) {
        Ok(paths) => paths,
        Err(response) => return response,
    };

    let mut checkpoint_store = ctx.checkpoint().lock();
    match checkpoint_store.absolute_file_paths(req.session(), name) {
        Ok(paths) => {
            let paths = match requested_paths {
                Some(requested_paths) => {
                    for path in &requested_paths {
                        if !paths.contains(path) {
                            return Response::error(
                                &req.id,
                                "invalid_request",
                                format!(
                                    "checkpoint_paths: requested path '{}' is not in checkpoint '{name}'",
                                    path.display()
                                ),
                            );
                        }
                    }
                    requested_paths
                }
                None => paths,
            };
            Response::success(
                &req.id,
                serde_json::json!({
                    "name": name,
                    "paths": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                    "file_count": paths.len(),
                }),
            )
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}
