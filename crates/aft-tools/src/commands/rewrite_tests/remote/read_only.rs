#[allow(unused_imports)]
use super::*;

#[test]
fn runon_read_only_note_requires_lock_failure_and_in_checkout_eperm() {
    for (locked, code, error) in [
        (None, 1, "../src/x.rs: Operation not permitted\n"),
        (Some(false), 1, "../src/x.rs: Operation not permitted\n"),
        (Some(true), 0, "../src/x.rs: Operation not permitted\n"),
        (
            Some(true),
            1,
            "'$ROOT/../outside.rs': Operation not permitted\n",
        ),
        (Some(true), 1, "../../outside.rs: Operation not permitted\n"),
        (Some(true), 1, "../src/x.rs: Permission denied\n"),
        (Some(true), 1, "failed without an error path\n"),
    ] {
        let (foreground, completion, disk) =
            read_only_result_case(locked, Outcome::Exit { code }, error);
        for text in [foreground, completion, disk] {
            assert!(
                !text.contains("note: the build server keeps tracked files"),
                "{text}"
            );
        }
    }
}

/// The read-only note reads the stderr capture of a failed read-only job. That
/// scan happens once per terminal capture, not on every status poll, and stops
/// after a fixed number of bytes when no permission error turns up.
#[test]
fn runon_read_only_note_scans_stderr_once_within_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.test_task(&task_id).unwrap();
    task.test_lock_state().unwrap().metadata_mut().workdir = dir.path().join("package");
    let stderr = task
        .test_lock_state()
        .unwrap()
        .buffer()
        .stderr_path()
        .unwrap()
        .to_path_buf();
    use aft_engine::test_bridge::bash_remote::take_stderr_scans;
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 1 }, 2, 0, 0)
        .with_ran(Ran::Remote)
        .with_workspace_changes(Vec::new())
        .with_source_read_only(true);
    let verdict = exec::grade(&record);
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.output(1, OutputStream::Stdout, b"command output\n")
        .unwrap();
    let noise = "error[E0425]: cannot find value in this scope\n".repeat(6 * 1024 * 1024 / 46);
    sink.output(2, OutputStream::Stderr, noise.as_bytes())
        .unwrap();
    sink.terminal(&record, &verdict).unwrap();
    registry.remote_terminal(&task, verdict, None);
    let _ = registry.drain_completions();
    take_stderr_scans(&stderr);
    for _ in 0..3 {
        let status = registry.observed_status(&task_id, "session", 8192).unwrap();
        assert!(!status.output_preview.contains("note: the build server"));
    }
    let (opens, bytes) = take_stderr_scans(&stderr);
    assert!(opens <= 1, "stderr opened for {opens} status scans");
    assert!(
        bytes <= 4 * 1024 * 1024 + 64 * 1024,
        "the scan read {bytes} bytes of stderr"
    );
}
