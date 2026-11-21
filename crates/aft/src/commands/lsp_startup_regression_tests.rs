use crate::lsp::manager::{
    ensure_server_for_file_detailed_unlocked, ServerAttemptResult, START_IN_PROGRESS_REASON,
    START_IN_PROGRESS_STATUS,
};
use aft_engine::test_bridge::search_misc::lsp_startup::{
    attempt_reason_for_test, fixture, locked, pause_first_spawn, WAIT,
};
use std::sync::atomic::Ordering;

#[test]
fn waiter_sees_pending_then_the_real_result() {
    let f = fixture();
    let pause = pause_first_spawn(&f);
    let initial = locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(f.file(), f.config())
    });
    pause.entered().recv_timeout(WAIT).unwrap();
    let pending = locked(&f, |manager| {
        manager.ensure_server_for_file_detailed(f.file(), f.config())
    });
    assert!(
        matches!(&initial.attempts[0].result, ServerAttemptResult::InProgress { server_key } if server_key == f.key())
    );
    assert_eq!(START_IN_PROGRESS_STATUS, "start_in_progress");
    assert_eq!(
        START_IN_PROGRESS_REASON,
        "server startup is in progress; retry shortly"
    );
    assert_eq!(
        pending.attempts[0].result.failure_reason(),
        START_IN_PROGRESS_REASON
    );
    let response =
        crate::commands::lsp_diagnostics::startup_unavailable_response(f.file(), &pending);
    assert_eq!(response["complete"], false);
    assert_eq!(response["pending_servers"], serde_json::json!(["reserved"]));
    assert_eq!(
        response["unchecked_files"],
        serde_json::json!([f.file.display().to_string()])
    );
    assert_eq!(
        response["lsp_servers_used"][0]["status"],
        "start_in_progress"
    );
    let area = locked(&f, |manager| {
        manager.start_applicable_servers(f.snapshot(), f.config())
    });
    assert_eq!(area.in_progress, vec![f.key().clone()]);
    assert!(area.successful.is_empty());
    f.assert_no_cached_failure();
    pause.resume().send(()).unwrap();
    let ready =
        ensure_server_for_file_detailed_unlocked(|| f.manager().lock(), f.file(), f.config());
    assert_eq!(ready.successful, vec![f.key().clone()]);
    assert_eq!(attempt_reason_for_test(&ready.attempts[0].result), "ok");
    assert_eq!(
        ServerAttemptResult::BinaryNotInstalled {
            binary: "example-ls".into()
        }
        .failure_reason(),
        "example-ls is unavailable"
    );
    assert_eq!(pause.spawns().load(Ordering::SeqCst), 1);
}
