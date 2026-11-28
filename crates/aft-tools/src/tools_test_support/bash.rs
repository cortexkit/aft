use aft_engine::test_bridge::bash_remote::{registry_support as test_support, BgTask};
use std::sync::Arc;

/// Read whether the watch has been asked to yield.
pub fn watch_yield_requested(signal: &crate::commands::bash_watch::YieldSignal) -> bool {
    signal.requested()
}

/// Render the hard kill deadline for the requested kind of session.
pub fn kill_deadline_sentence(
    deadline: Option<crate::bash_background::registry::HardKillDeadline>,
    started_at_ms: u64,
    worker_session: bool,
) -> String {
    crate::commands::bash_orchestrate::kill_deadline_sentence(
        deadline,
        started_at_ms,
        worker_session,
    )
}

/// Build a shell wait fixture with the default worker time limit.
pub fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let (dir, ctx, task) = test_support::wait_fixture(explicit_timeout);
    assert_eq!(
        crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx),
        1_800_000
    );
    (dir, ctx, task)
}

/// Launch a task and build its spawn reply after the task is known to have exited.
#[cfg(unix)]
pub fn completed_spawn_for_test(
    request: crate::protocol::RawRequest,
    ctx: &crate::context::AppContext,
) -> crate::protocol::Response {
    ctx.update_config(|config| config.experimental_bash_background = true);
    let launched = crate::commands::bash::handle(&request, ctx);
    assert!(launched.success, "{launched:?}");
    let task_id = launched.data["task_id"].as_str().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if ctx
            .bash_background()
            .observed_status(task_id, request.session(), 0)
            .is_some_and(|snapshot| snapshot.info.status.is_terminal())
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "task did not finish");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Construct the real spawn reply only after exit so the test does not
    // depend on the child winning a race against task registration.
    aft_engine::test_bridge::bash_remote::successful_spawn_response(
        &request.id,
        request.session(),
        task_id,
        launched.data["mode"] == "pty",
        ctx,
    )
}
