use super::test_support::conpty::*;
use super::*;
#[test]
fn conpty_wrapper_marker_does_not_close_before_child_exit() {
    let fixture = Fixture::new(false);
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task().state.lock().unwrap().runtime {
        pty.exit_observed.store(false, Ordering::SeqCst);
    }
    fixture.registry().poll_task(&fixture.task()).unwrap();
    fixture.assert_pending();
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task().state.lock().unwrap().runtime {
        assert!(pty.master.is_some());
        assert!(pty.output_drain.as_ref().unwrap().deadline.is_none());
        pty.exit_observed.store(true, Ordering::SeqCst);
    }
    fixture.registry().poll_task(&fixture.task()).unwrap();
    fixture.assert_pending();
    fixture.finish_reader();
    fixture.registry().poll_task(&fixture.task()).unwrap();
    assert_eq!(
        fixture.task().snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(&fixture.task().paths.pty).unwrap(),
        OUTPUT
    );
}

#[test]
fn conpty_completion_waits_for_output_capture() {
    let fixture = Fixture::new(false);
    fixture.registry().poll_task(&fixture.task()).unwrap();
    fixture.assert_pending();
    if let TaskRuntime::Pty(Some(pty)) = &fixture.task().state.lock().unwrap().runtime {
        assert!(
            pty.master.is_none(),
            "close the pseudoconsole before waiting for EOF"
        );
    }
    fixture.finish_reader();
    fixture.registry().poll_task(&fixture.task()).unwrap();
    assert_eq!(
        fixture.task().snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(&fixture.task().paths.pty).unwrap(),
        OUTPUT
    );
    assert_eq!(
        fixture
            .registry()
            .pending_completions_for_session("session")
            .len(),
        1
    );
}

#[test]
fn conpty_kill_after_exit_waits_for_output_capture() {
    let fixture = Fixture::new(false);
    fixture
        .registry()
        .kill(&fixture.task().task_id, "session")
        .unwrap();
    fixture.assert_pending();
    fixture.finish_reader();
    fixture.registry().poll_task(&fixture.task()).unwrap();
    assert_eq!(
        fixture.task().snapshot(0).info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(&fixture.task().paths.pty).unwrap(),
        OUTPUT
    );
}
