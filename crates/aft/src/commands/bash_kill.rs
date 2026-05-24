use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashKillParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashKillParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_kill: invalid params: {e}"),
            );
        }
    };

    let Some(task_id) = params.task_id else {
        return Response::error(&req.id, "invalid_request", "bash_kill: missing task_id");
    };

    let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    let result = ctx
        .bash_background()
        .kill(&task_id, req.session())
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            let _ = ctx
                .bash_background()
                .replay_session(&storage_dir, req.session());
            ctx.bash_background().kill(&task_id, req.session())
        })
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            let config = ctx.config();
            let Some(project_root) = config.project_root.as_deref() else {
                return Err(message);
            };
            ctx.bash_background()
                .kill_relaxed(&task_id, project_root, &storage_dir)
        });

    match result {
        Ok(snapshot) => Response::success(&req.id, json!(snapshot)),
        Err(message) if message.contains("not found") => {
            Response::error(&req.id, "task_not_found", message)
        }
        Err(message) => Response::error(&req.id, "kill_failed", message),
    }
}
