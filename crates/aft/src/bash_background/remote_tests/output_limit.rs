use super::*;

#[tokio::test]
async fn exec_remote_bash_continuous_output_cannot_starve_cancel() {
    let daemon = daemon(Script::Continuous, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    // Cancel while output is active without waiting for its saved cursor.
    wait_for(
        "continuous output must be consumed before cancelling",
        || (fs::metadata(&task.paths.stdout).unwrap().len() >= 11).then_some(()),
    )
    .await;
    assert!(registry.record_remote_cancel(&task).unwrap());
    daemon.wait_for_requests("exec.cancel", 1).await;
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::Killed
    );
}

#[tokio::test]
async fn output_limit_caps_remote_capture_and_requests_one_cancel() {
    use crate::bash_background::buffer::{limit_for_test, OUTPUT_LIMIT_REASON};

    let daemon = daemon(Script::Continuous, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let _limit = limit_for_test(session_tasks_dir(dir.path(), "session"), 3);
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    daemon.wait_for_requests("exec.cancel", 1).await;
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(OUTPUT_LIMIT_REASON)
    );
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"xxx");
    assert_eq!(fs::metadata(&task.paths.stderr).unwrap().len(), 0);
    let saved = read_task(&task.paths.json).unwrap().remote.unwrap();
    assert!(saved.output_limit_dropped_bytes > 0);
    assert_eq!(
        daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, record)| record["method"] == "exec.cancel")
            .count(),
        1
    );
}

#[test]
fn output_limit_remote_replay_does_not_write_or_recount_dropped_records() {
    use crate::bash_background::buffer::limit_for_test;

    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let _limit = limit_for_test(task.paths.session_dir.clone(), 4);
    {
        let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
        sink.output(1, OutputStream::Stdout, b"a").unwrap();
        sink.output(2, OutputStream::Stderr, b"oversized").unwrap();
    }
    // Build a fresh sink exclusively from the durable cursor and byte counts.
    task.lock_state().unwrap().metadata = read_task(&task.paths.json).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.output(2, OutputStream::Stderr, b"oversized").unwrap();
    sink.output(3, OutputStream::Stderr, b"b").unwrap();
    sink.commit(|_| {}).unwrap();
    let remote = read_task(&task.paths.json).unwrap().remote.unwrap();
    assert_eq!(remote.last_seq, Some(3));
    assert_eq!(remote.output_limit_dropped_bytes, 10);
    assert!(remote.cancel_requested);
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"a");
    assert_eq!(fs::metadata(&task.paths.stderr).unwrap().len(), 0);
}
