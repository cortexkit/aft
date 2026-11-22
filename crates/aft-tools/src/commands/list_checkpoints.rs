use crate::checkpoint::{CHECKPOINT_HYDRATED_NOTICE, CHECKPOINT_RESTART_NOTICE};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle the `list_checkpoints` command: return metadata for all checkpoints.
///
/// No params required.
/// Returns: `{ checkpoints: [{ name, file_count, created_at }, ...], durability }`.
/// The list hydrates from the durable disk tree after a bridge or daemon restart.
/// The restart notice is emitted only when that hydration finds no checkpoints.
pub fn handle_list_checkpoints(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut checkpoint_store = ctx.checkpoint().lock();
    let list = match checkpoint_store.list(req.session()) {
        Ok(list) => list,
        Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
    };

    let checkpoints: Vec<serde_json::Value> = list
        .iter()
        .map(|info| {
            serde_json::json!({
                "name": info.name,
                "file_count": info.file_count,
                "created_at": info.created_at,
            })
        })
        .collect();

    Response::success(
        &req.id,
        serde_json::json!({
            "checkpoints": checkpoints,
            "durability": if checkpoints.is_empty() {
                CHECKPOINT_RESTART_NOTICE
            } else {
                CHECKPOINT_HYDRATED_NOTICE
            },
        }),
    )
}
