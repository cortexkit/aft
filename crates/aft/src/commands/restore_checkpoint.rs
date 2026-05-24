use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle the `restore_checkpoint` command: restore files from a named checkpoint.
///
/// Params: `name` (string, required) — checkpoint name to restore.
/// Returns: `{ name, file_count, created_at }` on success, or `checkpoint_not_found` error.
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

    let checkpoint_store = ctx.checkpoint().borrow();
    let file_paths = checkpoint_store
        .file_paths(req.session(), name)
        .map_err(|e| Response::error(&req.id, e.code(), e.to_string()))?;
    let validated_paths = validate_restore_paths(&req.id, ctx, &file_paths)?;

    match checkpoint_store.restore_validated(req.session(), name, &validated_paths) {
        Ok(info) => Ok(Response::success(
            &req.id,
            serde_json::json!({
                "name": info.name,
                "file_count": info.file_count,
                "created_at": info.created_at,
            }),
        )),
        Err(e) => Ok(Response::error(&req.id, e.code(), e.to_string())),
    }
}

fn validate_restore_paths(
    req_id: &str,
    ctx: &AppContext,
    file_paths: &[std::path::PathBuf],
) -> Result<Vec<std::path::PathBuf>, Response> {
    let mut validated = Vec::with_capacity(file_paths.len());
    for path in file_paths {
        validated.push(ctx.validate_path(req_id, path)?);
    }
    Ok(validated)
}
