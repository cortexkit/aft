use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

const PREVIEW_BYTES: usize = 5 * 1024;

#[derive(Debug, Deserialize)]
struct BashStatusParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashStatusParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_status: invalid params: {e}"),
            );
        }
    };

    let Some(task_id) = params.task_id else {
        return Response::error(&req.id, "invalid_request", "bash_status: missing task_id");
    };

    let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    match ctx.bash_background().status(
        &task_id,
        req.session(),
        ctx.config().project_root.as_deref(),
        Some(&storage_dir),
        PREVIEW_BYTES,
    ) {
        Some(snapshot) => Response::success(&req.id, json!(snapshot)),
        None => Response::error(
            &req.id,
            "task_not_found",
            format!("background task not found: {task_id}"),
        ),
    }
}
