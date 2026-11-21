use super::*;
use crate::bash_background::persistence::{task_paths, write_task};

pub(crate) fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::from_app(
        crate::context::App::default_shared(),
        crate::config::Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    let registry = ctx.bash_background();
    let task_id = "bash-0000000000000001";
    let session_id = "head-session";
    let paths = task_paths(dir.path(), session_id, task_id).unwrap();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.into(),
        session_id.into(),
        "fake running command".into(),
        dir.path().to_path_buf(),
        Some(dir.path().to_path_buf()),
        Some(explicit_timeout.unwrap_or(1_800_000)),
        false,
        false,
    );
    metadata.default_hard_kill = explicit_timeout.is_none();
    metadata.started_at = unix_millis().saturating_sub(1_900);
    // Insert a terminal fixture to avoid restart renewal, then make it running.
    // Its backdated start exercises elapsed-time renewal without sleeping or
    // spawning a process; no watchdog runs in this fixture.
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, paths, false)
        .unwrap();
    let task = registry.task_for_session(task_id, session_id).unwrap();
    assert!(task.started.elapsed() >= Duration::from_millis(1_900));
    assert_eq!(task.hard_kill_renewable, explicit_timeout.is_none());
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.status = BgTaskStatus::Running;
        state.metadata.finished_at = None;
        state.metadata.exit_code = None;
        state.metadata.duration_ms = None;
        write_task(&task.paths.json, &state.metadata).unwrap();
    }
    (dir, ctx, task)
}
