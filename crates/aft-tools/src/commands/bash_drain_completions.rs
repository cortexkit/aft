use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct BashAckCompletionsParams {
    #[serde(default)]
    task_ids: Vec<String>,
    #[serde(default)]
    matches: Vec<PatternMatchAck>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternMatchAck {
    task_id: String,
    watch_id: String,
    match_offset: u64,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let background = ctx.bash_background();
    Response::success(
        &req.id,
        json!({
            "bg_completions": background.drain_completions_for_session(Some(req.session())),
            "pending_matches": background.pending_pattern_matches_for_session(req.session()),
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

    let background = ctx.bash_background();
    let mut acked_matches = Vec::new();
    for pattern_match in params.matches {
        match background.ack_pattern_match_for_session(
            req.session(),
            &pattern_match.task_id,
            &pattern_match.watch_id,
            pattern_match.match_offset,
        ) {
            Ok(Some(result)) => acked_matches.push(json!({
                "task_id": pattern_match.task_id,
                "watch_id": pattern_match.watch_id,
                "match_offset": pattern_match.match_offset,
                "watch_removed": result.watch_removed,
                "task_settled": result.task_settled,
            })),
            Ok(None) => {}
            Err(error) => return Response::error(&req.id, "pattern_ack_failed", error),
        }
    }
    let acked_task_ids =
        background.ack_completions_for_session(Some(req.session()), &params.task_ids);
    let ignored_task_ids: Vec<_> = params
        .task_ids
        .iter()
        .filter(|task| !acked_task_ids.contains(task))
        .collect();
    Response::success(
        &req.id,
        json!({
            "acked_task_ids": acked_task_ids,
            "ignored_task_ids": ignored_task_ids,
            "acked_matches": acked_matches,
        }),
    )
}
