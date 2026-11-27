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

    thread_local! {
        pub(super) static BEFORE_CREATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn edit_review_gpt_5_checkpoint_does_not_rewrite_matching_file() {
        use std::{
            fs,
            time::{Duration, SystemTime},
        };

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("matching.txt");
        let ctx = test_context(root.path());
        fs::write(&path, b"matching").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(123_456)),
            )
            .unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let create: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "create", "command": "checkpoint", "name": "matching", "files": [path],
        }))
        .unwrap();
        assert!(handle_checkpoint(&create, &ctx).success);
        let restore: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "restore", "command": "restore_checkpoint", "name": "matching",
        }))
        .unwrap();
        let response =
            crate::commands::restore_checkpoint::handle_restore_checkpoint(&restore, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["changed_count"], 0);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    #[cfg(unix)]
    #[test]
    fn edit_review_opus_5_checkpoint_and_undo_keep_open_original_bytes() {
        use std::{fs, io::Read};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changed.txt");
        let ctx = test_context(root.path());
        fs::write(&path, b"checkpoint").unwrap();
        let create: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "create", "command": "checkpoint", "name": "before", "files": [path],
        }))
        .unwrap();
        assert!(handle_checkpoint(&create, &ctx).success);
        let restore: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "restore", "command": "restore_checkpoint", "name": "before",
        }))
        .unwrap();
        let undo: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "undo", "command": "undo", "file": path,
        }))
        .unwrap();

        for (prior, handler, req) in [
            (
                b"undo".as_slice(),
                crate::commands::undo::handle_undo as fn(&RawRequest, &AppContext) -> Response,
                &undo,
            ),
            (
                b"checkpoint".as_slice(),
                crate::commands::restore_checkpoint::handle_restore_checkpoint,
                &restore,
            ),
        ] {
            fs::write(&path, prior).unwrap();
            if req.id == "undo" {
                let mut backup = ctx.backup().lock();
                backup.permit_temp_path_snapshots_for_test();
                backup.snapshot(req.session(), &path, "tracked").unwrap();
            }
            fs::write(&path, b"after").unwrap();
            ctx.backup().lock().record_post_mutation_states();
            let mut original = fs::File::open(&path).unwrap();
            let response = handler(req, &ctx);
            let mut bytes = Vec::new();
            original.read_to_end(&mut bytes).unwrap();
            assert!(response.success, "{response:?}");
            assert_eq!(fs::read(&path).unwrap(), prior);
            assert_eq!(
                bytes, b"after",
                "{} truncated the open original",
                req.command
            );
        }
    }

    fn test_context(root: &std::path::Path) -> AppContext {
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..Default::default()
            },
        );
        ctx.backup().lock().set_storage_dir(root.join("backup"), 72);
        ctx.checkpoint().lock().set_storage_dir_for_harness(
            root.join("checkpoints"),
            crate::harness::Harness::Opencode,
        );
        ctx
    }

    #[test]
    fn backup_checkpoint_command_reports_discovery_errors_without_replacing_history() {
        for released in [false, true] {
            for expired in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let path = root.path().join("tracked.txt");
                let second = root.path().join("second.txt");
                std::fs::write(&path, b"restore me").unwrap();
                std::fs::write(&second, b"second").unwrap();
                let ctx = crate::test_context(
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

    #[test]
    fn edit_review_opus_12_deleted_tracked_files_make_an_empty_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("tracked.txt");
        let ctx = test_context(root.path());
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "empty", "command": "checkpoint", "name": "empty",
        }))
        .unwrap();
        std::fs::write(&path, b"original").unwrap();
        {
            let mut backup = ctx.backup().lock();
            backup.permit_temp_path_snapshots_for_test();
            backup
                .snapshot(request.session(), &path, "tracked")
                .unwrap();
            assert_eq!(
                backup.try_tracked_files(request.session()).unwrap().len(),
                1
            );
        }
        std::fs::remove_file(&path).unwrap();

        let explicit: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "explicit", "command": "checkpoint", "name": "explicit", "files": [path],
        }))
        .unwrap();
        let response = handle_checkpoint(&explicit, &ctx);
        assert_eq!(response.data["code"], "file_not_found", "{response:?}");
        let response = handle_checkpoint(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["file_count"], 0);
        assert_eq!(response.data["skipped"].as_array().unwrap().len(), 1);
        assert!(response.data.get("requested_count").is_none());
    }

    #[test]
    fn edit_review_opus_8_checkpoint_creation_does_not_hold_backup_mutex() {
        let root = tempfile::tempdir().unwrap();
        let ctx = std::sync::Arc::new(test_context(root.path()));
        let path = root.path().join("tracked.txt");
        std::fs::write(&path, b"tracked").unwrap();
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "create", "command": "checkpoint", "name": "unlocked", "files": [path],
        }))
        .unwrap();
        BEFORE_CREATE.with(|hook| {
            let ctx = ctx.clone();
            hook.replace(Some(Box::new(move || {
                assert!(ctx.backup().try_lock().is_some());
            })));
        });
        let response = handle_checkpoint(&request, &ctx);
        assert!(response.success, "{response:?}");
        BEFORE_CREATE.with(|hook| assert!(hook.borrow().is_none()));
    }
    #[cfg(test)]
    use aft_engine::test_bridge::search_misc::BackupStoreTestExt as _;
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

    let mut checkpoint_store = ctx.checkpoint().lock();

    #[cfg(test)]
    if let Some(hook) = tests::BEFORE_CREATE.with(|hook| hook.replace(None)) {
        hook();
    }
    let checkpoint = if explicit_request {
        checkpoint_store.create_for_files(req.session(), name, validated_files)
    } else {
        checkpoint_store.create_for_tracked_files(req.session(), name, validated_files)
    };
    match checkpoint {
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
