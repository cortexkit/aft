//! PTY tasks whose output capture or exit marker misbehaves: a capture file
//! that cannot be written, and an exit marker that cannot be saved. Gates hold
//! each program until the test has set up the condition it checks; every wait
//! has a bound, so a regression fails instead of hanging.

use super::*;
use crate::bash_background::pty_process::{inject_pty_fault_for_test, PtyFaultForTest};

const WAIT: Duration = Duration::from_secs(30);

/// Spawn a PTY task that waits for the file `ready` in `dir` before it runs
/// `after_gate`. While it waits, the program reads none of its input.
fn spawn_gated_pty(registry: &BgTaskRegistry, dir: &Path, after_gate: &str) -> String {
    registry
        .spawn_pty(
            SpawnPlan::Unsandboxed,
            &format!(
                "/bin/sh -c '{}; {after_gate}'",
                crate::test_gate::gate("ready", "sleep 0.01")
            ),
            "session".to_string(),
            dir.to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(120)),
            None,
            dir.to_path_buf(),
            10,
            true,
            false,
            Some(dir.to_path_buf()),
            24,
            80,
        )
        .unwrap()
}

fn open_gate(dir: &Path) {
    fs::write(dir.join("ready"), "").unwrap();
}

fn wait_for_completion(registry: &BgTaskRegistry, task_id: &str) -> BgCompletion {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(completion) = registry
            .pending_completions_for_session("session")
            .into_iter()
            .find(|completion| completion.task_id == task_id)
        {
            return completion;
        }
        assert!(
            Instant::now() < deadline,
            "no completion was queued for {task_id}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_pty_exit_whose_marker_cannot_be_saved_still_completes_with_its_code() {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_gated_pty(&registry, dir.path(), "exit 23");
    inject_pty_fault_for_test(&task_id, PtyFaultForTest::ExitMarkerWrite);
    open_gate(dir.path());

    let completion = wait_for_completion(&registry, &task_id);

    assert_eq!(completion.exit_code, Some(23));
    assert_eq!(completion.status, BgTaskStatus::Failed);
    let task = registry.task_for_test(&task_id).unwrap();
    assert!(
        fs::read(&task.paths.exit).unwrap().is_empty(),
        "the injected failure must leave the exit marker unwritten"
    );
}

#[test]
fn a_pty_whose_capture_fails_reports_incomplete_output_and_keeps_its_exit_code() {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_gated_pty(&registry, dir.path(), "printf hello");
    inject_pty_fault_for_test(&task_id, PtyFaultForTest::CaptureWrite);
    open_gate(dir.path());

    let completion = wait_for_completion(&registry, &task_id);

    assert_eq!(completion.exit_code, Some(0));
    assert_eq!(completion.status, BgTaskStatus::Completed);
    let task = registry.task_for_test(&task_id).unwrap();
    let state = task.lock_state().unwrap();
    assert!(
        state.metadata.output_incomplete,
        "a failed capture must be reported, not shown as the whole output"
    );
    assert!(
        state
            .metadata
            .status_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("output capture failed before EOF")),
        "{:?}",
        state.metadata.status_reason
    );
}
