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

pub(crate) use super::tests::insert_terminal_piped_task;

    pub(crate) fn register_unpersisted_watch(registry: &BgTaskRegistry, id: String, pattern: WatchPattern, once: bool) {
        registry.inner.watch_registry.lock().unwrap().register(id, pattern, once).unwrap();
    }

pub(crate) use super::completion_preview::capture_reads::tests::{filler, terminal_task, with_secret};
    pub(crate) use super::completion_preview::capture_reads::tests::DISPLAY_LINE_FINISH_BYTES;

    pub(crate) fn display_bodies(registry: &BgTaskRegistry, task: &Arc<BgTask>, metadata: &PersistedTask, buffer: &BgBuffer) -> crate::bash_background::frames::BashBodies {
        registry.display_bodies(task, metadata, buffer)
    }

#[cfg(unix)]
    pub(crate) mod ordering {
        pub(crate) use super::super::completion_replay_ordering_tests::{capture_live_ordering_frames, completion_frames, live_secret_task, pause_ordering_publication, registry, MASKED, SESSION};
    }

pub(crate) use super::a02_tests::terminal_fixture;

    pub(crate) fn set_completion_secrets(task: &BgTask, values: crate::log_redact::SecretValues) {
        *task.completion_secrets.lock().unwrap() = Some(values);
    }

    pub(crate) fn capture_display_terminal(registry: &BgTaskRegistry, task: &Arc<BgTask>, stale: &PersistedTask, buffer: &BgBuffer) {
        registry.capture_display_terminal(task, stale, buffer);
    }
