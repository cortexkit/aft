use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let registry = ctx.bash_background();
    let detached = if req.params.get("clear_pending").and_then(|v| v.as_bool()) == Some(true) {
        registry.clear_pending_wait_detach(req.session());
        false
    } else if req.params.get("pending").and_then(|v| v.as_bool()) == Some(true) {
        registry.request_wait_mode_detach(req.session())
    } else {
        registry.signal_wait_mode_detach(req.session())
    };
    // A message can arrive when no wait is active; keep that routine miss at
    // debug so INFO remains useful for an actual detached wait.
    log_detach_outcome(
        req.session(),
        detached,
        ctx.bash_background().active_wait_session_count(),
    );
    Response::success(&req.id, json!({ "detached": detached }))
}

fn log_detach_outcome(session: &str, detached: bool, active_wait_sessions: usize) {
    if detached {
        crate::slog_info!(
            "bash_wait_detach: session={} detached={} active_wait_sessions={}",
            session,
            detached,
            active_wait_sessions
        );
    } else {
        crate::slog_debug!(
            "bash_wait_detach: session={} detached={} active_wait_sessions={}",
            session,
            detached,
            active_wait_sessions
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_op_detach_is_debug_logged_while_success_stays_info() {
        let (_, no_op) = crate::logging::capture_log_lines(|| {
            super::log_detach_outcome("session", false, 0);
        });
        assert!(
            no_op
                .iter()
                .any(|line| { line.starts_with("DEBUG ") && line.contains("detached=false") }),
            "no-op detach was not debug-logged: {no_op:?}"
        );
        assert!(
            !no_op.iter().any(|line| line.starts_with("INFO ")),
            "no-op detach was info-logged: {no_op:?}"
        );

        let (_, success) = crate::logging::capture_log_lines(|| {
            super::log_detach_outcome("session", true, 1);
        });
        assert!(
            success
                .iter()
                .any(|line| { line.starts_with("INFO ") && line.contains("detached=true") }),
            "successful detach was not info-logged: {success:?}"
        );
    }
}
