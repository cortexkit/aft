use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct BashAckCompletionsParams {
    #[serde(default)]
    task_ids: Vec<String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    Response::success(
        &req.id,
        json!({
            "bg_completions": ctx.bash_background().drain_completions_for_session(Some(req.session())),
        }),
    )
}

pub fn handle_ack(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashAckCompletionsParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_ack_completions: invalid params: {e}"),
            );
        }
    };

    Response::success(
        &req.id,
        json!({
            "acked_task_ids": ctx
                .bash_background()
                .ack_completions_for_session(Some(req.session()), &params.task_ids),
        }),
    )
}
