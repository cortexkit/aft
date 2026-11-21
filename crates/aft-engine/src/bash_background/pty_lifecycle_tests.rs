//! PTY tasks whose terminal input, output capture or exit marker misbehaves:
//! a program that is not reading its input, a cursor-position query while
//! input is still pending, a capture file that cannot be written, and an exit
//! marker that cannot be saved. Gates hold each program until the test has set
//! up the condition it checks; every wait has a bound, so a regression fails
//! instead of hanging.

use super::*;
use crate::bash_background::pty_process::{
    inject_pty_fault_for_test, watch_pty_write_start_for_test, PtyFaultForTest,
};

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

fn context(dir: &Path) -> Arc<crate::context::AppContext> {
    Arc::new(crate::context::AppContext::from_app(
        crate::context::App::default_shared(),
        crate::config::Config {
            project_root: Some(dir.into()),
            storage_dir: Some(dir.into()),
            ..Default::default()
        },
    ))
}

/// Run `bash_write` with `input` on its own thread; the receiver gets the
/// response when the call returns.
fn bash_write_in_background(
    ctx: &Arc<crate::context::AppContext>,
    task_id: &str,
    input: String,
) -> crossbeam_channel::Receiver<crate::protocol::Response> {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write",
        "command": "bash_write",
        "session_id": "session",
        "params": {"task_id": task_id, "input": input},
    }))
    .unwrap();
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let ctx = Arc::clone(ctx);
    std::thread::spawn(move || {
        let _ = done_tx.send(crate::commands::bash_write::handle(&request, &ctx));
    });
    done_rx
}

/// 1 MiB of 1 KiB lines: far more than a terminal's input queue holds.
fn one_mebibyte_of_lines() -> String {
    format!("{}\n", "x".repeat(1023)).repeat(1024)
}

#[test]
fn bash_write_to_a_program_not_reading_its_input_returns_by_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    // The gate is never opened: the program never reads its input.
    let task_id = spawn_gated_pty(ctx.bash_background(), dir.path(), "true");
    let input = one_mebibyte_of_lines();
    let total = input.len() as u64;

    let response = bash_write_in_background(&ctx, &task_id, input)
        .recv_timeout(WAIT)
        .expect("bash_write never returned while the program ignored its input");

    assert!(!response.success, "{:?}", response.data);
    assert_eq!(
        response.data["code"], "input_timeout",
        "{:?}",
        response.data
    );
    let written = response.data["bytes_written"].as_u64().unwrap();
    let pending = response.data["bytes_pending"].as_u64().unwrap();
    assert_eq!(written + pending, total);
    assert!(pending > 0, "{:?}", response.data);
    let _ = ctx.bash_background().kill(&task_id, "session");
}

#[test]
fn killing_a_pty_task_releases_a_write_its_program_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let task_id = spawn_gated_pty(ctx.bash_background(), dir.path(), "true");
    let started = watch_pty_write_start_for_test(&task_id);
    let done = bash_write_in_background(&ctx, &task_id, one_mebibyte_of_lines());
    started
        .recv_timeout(WAIT)
        .expect("bash_write never started writing");

    ctx.bash_background().kill(&task_id, "session").unwrap();

    let response = done
        .recv_timeout(WAIT)
        .expect("bash_write was still blocked after its task was killed");
    // Only the kill ends this write early; its deadline would answer
    // `input_timeout` instead.
    assert_eq!(response.data["code"], "task_exited", "{:?}", response.data);
}

#[test]
fn a_cursor_query_while_input_is_pending_does_not_stall_the_terminal() {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let dir = tempfile::tempdir().unwrap();
    // After the gate the program asks for the cursor position, prints far
    // more than the terminal buffers, and only then reads its input.
    let task_id = spawn_gated_pty(
        &registry,
        dir.path(),
        r#"printf "\033[6n"; head -c 2000000 /dev/zero | tr "\0" x; cat >/dev/null"#,
    );
    let task = registry.task_for_test(&task_id).unwrap();
    let started = watch_pty_write_start_for_test(&task_id);
    let input = format!("{}\n", "a".repeat(127)).repeat(2048);
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    {
        let registry = registry.clone();
        let task_id = task_id.clone();
        let input = input.clone();
        std::thread::spawn(move || {
            let _ = done_tx.send(registry.write_pty(&task_id, "session", input.as_bytes()));
        });
    }
    started
        .recv_timeout(WAIT)
        .expect("bash_write never started writing");
    // The write is under way and the program is not reading, so the query
    // arrives while input is still pending.
    open_gate(dir.path());

    let written = done_rx
        .recv_timeout(WAIT)
        .expect("the write never finished: the cursor query stalled the terminal");
    assert_eq!(written, Ok(input.len()));
    let deadline = Instant::now() + WAIT;
    while fs::metadata(&task.paths.pty).map_or(0, |meta| meta.len()) < 2_000_000 {
        assert!(
            Instant::now() < deadline,
            "the capture stopped growing after the cursor query"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = registry.kill(&task_id, "session");
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
