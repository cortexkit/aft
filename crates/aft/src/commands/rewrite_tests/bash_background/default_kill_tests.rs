use crate::bash_background::registry::{test_support, BgTask};
use std::sync::Arc;

pub(crate) fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let (dir, ctx, task) = test_support::wait_fixture(explicit_timeout);
    assert_worker_wait_limit(&ctx);
    (dir, ctx, task)
}

pub(super) fn assert_worker_wait_limit(ctx: &crate::context::AppContext) {
    assert_eq!(
        crate::commands::bash_orchestrate::worker_wait_max_ms(ctx),
        1_800_000
    );
}

fn message_detach_limit(worker_session: bool, explicit_timeout: Option<u64>) -> u64 {
    let (_dir, ctx, task) = wait_fixture(explicit_timeout);
    let registry = ctx.bash_background();
    let task_id = &task.task_id;
    let session_id = &task.session_id;
    let request = serde_json::from_value(serde_json::json!({
        "id": "message-detach", "command": "bash", "session_id": session_id,
        "worker_session": worker_session,
        "params": { "wait": true, "timeout": explicit_timeout }
    }))
    .unwrap();
    let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
        &request,
        &ctx,
        crate::protocol::Response::success(
            "message-detach",
            serde_json::json!({"task_id": task_id, "status": "running"}),
        ),
    );
    let crate::response_finalize::DispatchOutcome::Deferred(mut pending) = outcome else {
        panic!("the fake command must remain in a blocking wait");
    };
    assert!(registry.signal_wait_mode_detach(session_id));
    let response = (pending.poll)(&ctx).expect("a user message detaches the wait");
    assert!(response.success, "detach failed: {:?}", response.data);
    assert!(
        response
            .data
            .to_string()
            .contains("Detached because a user message arrived"),
        "the message-detach path must run: {:?}",
        response.data
    );
    assert_eq!(registry.active_wait_session_count(), 0);
    registry
        .hard_kill_deadline(task_id, session_id)
        .unwrap()
        .limit_ms
}

#[test]
fn head_message_detach_preserves_default_hard_kill() {
    assert_eq!(message_detach_limit(false, None), 1_800_000);
}

#[test]
fn pending_message_detach_preserves_wait_mode_eligibility() {
    for wait in [true, false] {
        let (_dir, ctx, task) = wait_fixture(None);
        let params = if wait {
            serde_json::json!({"wait": true})
        } else {
            serde_json::json!({})
        };
        let request = serde_json::from_value(serde_json::json!({
            "id": "pending-message", "command": "bash", "session_id": task.session_id,
            "params": params
        }))
        .unwrap();
        let admission = serde_json::from_value(serde_json::json!({
            "id": "admission", "command": "bash_wait_detach", "session_id": task.session_id,
            "pending": true
        }))
        .unwrap();
        assert!(crate::commands::bash_wait_detach::handle(&admission, &ctx).success);
        let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
            &request,
            &ctx,
            crate::protocol::Response::success(
                "pending-message",
                serde_json::json!({"task_id": task.task_id, "status": "running"}),
            ),
        );
        if !wait {
            let crate::response_finalize::DispatchOutcome::Deferred(mut pending) = outcome else {
                panic!("a plain foreground command must keep waiting despite pending input");
            };
            assert!(!ctx
                .bash_background()
                .request_wait_mode_detach(&task.session_id));
            assert!(
                (pending.poll)(&ctx).is_none(),
                "a message during a plain foreground wait must not detach it"
            );
            assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
            continue;
        }
        let response = match outcome {
            crate::response_finalize::DispatchOutcome::Immediate(response) => response,
            crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                (pending.poll)(&ctx).expect("pending input must detach immediately")
            }
        };
        assert!(response.success);
        assert!(response
            .data
            .to_string()
            .contains("Detached because a user message arrived"));
        assert_eq!(response.data["task_id"], task.task_id);
        assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
    }
}

#[test]
fn pending_message_does_not_change_background_launch() {
    let (_dir, ctx, task) = wait_fixture(None);
    ctx.bash_background()
        .request_wait_mode_detach(&task.session_id);
    let request = serde_json::from_value(serde_json::json!({
        "id": "background", "command": "bash", "session_id": task.session_id,
        "params": { "background": true }
    }))
    .unwrap();
    let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
        &request,
        &ctx,
        crate::protocol::Response::success(
            "background",
            serde_json::json!({"task_id": task.task_id, "status": "running"}),
        ),
    );
    let crate::response_finalize::DispatchOutcome::Immediate(response) = outcome else {
        panic!("background launch must not enter the foreground wait path");
    };
    assert!(!response
        .data
        .to_string()
        .contains("Detached because a user message arrived"));
    assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
    ctx.bash_background()
        .begin_wait_mode_session(&task.session_id, "later-foreground");
    assert!(ctx
        .bash_background()
        .take_task_wait_mode_detach(&task.session_id, "later-foreground"));
}

#[test]
fn worker_message_detach_renews_default_hard_kill() {
    assert!(message_detach_limit(true, None) >= 1_801_900);
}

#[test]
fn worker_message_detach_preserves_explicit_hard_kill() {
    assert_eq!(message_detach_limit(true, Some(45_000)), 45_000);
}

#[test]
fn head_capped_wait_promotion_preserves_default_hard_kill() {
    let (_dir, ctx, task) = wait_fixture(None);
    let response = crate::commands::bash_orchestrate::promote_bash(
        &ctx,
        &task.task_id,
        &task.session_id,
        None,
        1_800_000,
        "cap-handoff",
        false,
        false,
        true,
    );
    assert!(response.success, "promotion failed: {:?}", response.data);
    assert_eq!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .map(|deadline| deadline.limit_ms),
        Some(1_800_000)
    );
}

#[test]
fn worker_capped_wait_promotion_renews_default_hard_kill() {
    let (_dir, ctx, task) = wait_fixture(None);
    let response = crate::commands::bash_orchestrate::promote_bash(
        &ctx,
        &task.task_id,
        &task.session_id,
        None,
        1_800_000,
        "cap-handoff",
        true,
        true,
        true,
    );
    assert!(response.success, "promotion failed: {:?}", response.data);
    assert!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .unwrap()
            .limit_ms
            >= 1_801_900
    );
}
