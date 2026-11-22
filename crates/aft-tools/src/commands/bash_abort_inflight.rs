use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Abort only foreground bash calls that are still registered as in-flight
/// calls for the request's bound session. The request body is intentionally
/// ignored: subc reinjects the bind session into `RawRequest`, so a caller
/// cannot use this plumbing command to target another session.
pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    match ctx.bash_background().abort_inflight(req.session()) {
        Ok(killed) => Response::success(&req.id, json!({ "killed": killed })),
        Err(message) => Response::error(&req.id, "kill_failed", message),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::context::AppContext;

    #[test]
    fn abort_with_no_wait_registered_tasks_is_a_successful_noop() {
        let app = crate::test_app();
        let ctx = AppContext::from_app(Arc::clone(&app), Config::default());
        let req = RawRequest {
            id: "abort-inflight".to_string(),
            command: "bash_abort_inflight".to_string(),
            lsp_hints: None,
            session_id: Some("session-a".to_string()),
            params: json!({ "session_id": "session-b" }),
        };

        let response = serde_json::to_value(handle(&req, &ctx)).unwrap();
        assert_eq!(response["success"], true);
        assert_eq!(response["killed"], 0);
    }
}
