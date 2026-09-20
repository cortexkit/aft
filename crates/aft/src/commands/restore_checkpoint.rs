use crate::checkpoint::{
    checkpoint_durability, CHECKPOINT_HYDRATED_NOTICE, CHECKPOINT_RESTART_NOTICE,
};
use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};
use std::path::PathBuf;

/// Handle the `restore_checkpoint` command: restore files from a named checkpoint.
///
/// Params: `name` (string, required), plus optional `file`/`files` restore scope.
/// Returns: `{ name, file_count, paths, created_at, storage_path, durability }` on success.
/// If the requested checkpoint is missing, mention possible loss during a restart
/// only when loading persisted checkpoints found none for this session.
pub fn handle_restore_checkpoint(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_restore_checkpoint_impl(req, ctx) {
        Ok(resp) | Err(resp) => resp,
    }
}

fn handle_restore_checkpoint_impl(
    req: &RawRequest,
    ctx: &AppContext,
) -> Result<Response, Response> {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Ok(Response::error(
                &req.id,
                "invalid_request",
                "restore_checkpoint: missing required param 'name'",
            ));
        }
    };

    let requested_paths = requested_restore_paths(req, ctx)?;
    let mut checkpoint_store = ctx.checkpoint().lock();
    let checkpoint_paths = checkpoint_store
        .file_paths(req.session(), name)
        .map_err(|error| {
            checkpoint_not_found_response(
                &req.id,
                error,
                checkpoint_store.session_is_empty(req.session()),
            )
        })?;
    let restore_paths = if let Some(requested_paths) = requested_paths {
        for path in &requested_paths {
            if !checkpoint_paths.contains(path) {
                return Ok(Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "restore_checkpoint: requested path '{}' is not in checkpoint '{name}'",
                        path.display()
                    ),
                ));
            }
        }
        requested_paths
    } else {
        validate_restore_paths(&req.id, ctx, &checkpoint_paths)?
    };
    let restored_path_strings = restore_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();

    match checkpoint_store.restore_validated(req.session(), name, &restore_paths) {
        Ok(info) => {
            let storage_path = info
                .storage_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            Ok(Response::success(
                &req.id,
                serde_json::json!({
                    "name": info.name,
                    "file_count": restore_paths.len(),
                    "paths": restored_path_strings,
                    "created_at": info.created_at,
                    "storage_path": storage_path,
                    "durability": checkpoint_durability(std::path::Path::new(&storage_path)),
                }),
            ))
        }
        Err(e) => Ok(Response::error(&req.id, e.code(), e.to_string())),
    }
}

fn requested_restore_paths(
    req: &RawRequest,
    ctx: &AppContext,
) -> Result<Option<Vec<PathBuf>>, Response> {
    let mut inputs = Vec::new();
    if let Some(file) = req.params.get("file") {
        let file = file
            .as_str()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                Response::error(
                    &req.id,
                    "invalid_request",
                    "restore_checkpoint: 'file' must be a non-empty string",
                )
            })?;
        inputs.push(PathBuf::from(file));
    }
    if let Some(files) = req.params.get("files") {
        let files = files.as_array().ok_or_else(|| {
            Response::error(
                &req.id,
                "invalid_request",
                "restore_checkpoint: 'files' must be an array of non-empty strings",
            )
        })?;
        for file in files {
            let file = file
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    Response::error(
                        &req.id,
                        "invalid_request",
                        "restore_checkpoint: 'files' must be an array of non-empty strings",
                    )
                })?;
            inputs.push(PathBuf::from(file));
        }
    }

    if inputs.is_empty() {
        return Ok(None);
    }

    let mut validated = Vec::with_capacity(inputs.len());
    for path in inputs {
        let input = ctx.resolve_relative_path(&path);
        let path = ctx.validate_write_location(&req.id, &input)?;
        if !validated.contains(&path) {
            validated.push(path);
        }
    }
    Ok(Some(validated))
}

fn checkpoint_not_found_response(
    request_id: &str,
    error: AftError,
    session_is_empty: bool,
) -> Response {
    match error {
        AftError::CheckpointNotFound { name } => {
            let notice = if session_is_empty {
                CHECKPOINT_RESTART_NOTICE
            } else {
                CHECKPOINT_HYDRATED_NOTICE
            };
            Response::error(
                request_id,
                "checkpoint_not_found",
                format!("checkpoint not found: {name}; {notice}"),
            )
        }
        other => Response::error(request_id, other.code(), other.to_string()),
    }
}

fn validate_restore_paths(
    req_id: &str,
    ctx: &AppContext,
    file_paths: &[std::path::PathBuf],
) -> Result<Vec<std::path::PathBuf>, Response> {
    for path in file_paths {
        ctx.validate_write_location(req_id, path)?;
    }

    // Authorization must not replace the checkpoint key with a symlink target.
    // The restore writer removes a final-component symlink before materializing
    // the snapshot, so the stored location is both the lookup and write target.
    Ok(file_paths.to_vec())
}
