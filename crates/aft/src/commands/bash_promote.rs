use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashPromoteParams {
    task_id: String,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashPromoteParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_promote: invalid params: {e}"),
            );
        }
    };

    match ctx
        .bash_background()
        .promote(&params.task_id, req.session())
    {
        Ok(promoted) => Response::success(
            &req.id,
            json!({
                "task_id": params.task_id,
                "promoted": promoted,
            }),
        ),
        Err(message) if message.contains("not found") => {
            Response::error(&req.id, "task_not_found", message)
        }
        Err(message) => Response::error(&req.id, "execution_failed", message),
    }
}
