use super::*;

pub fn register_task(
    registry: &BgTaskRegistry,
    storage: &Path,
    root: &Path,
    task_id: &str,
    status: BgTaskStatus,
    pid: Option<u32>,
) {
    let layout =
        super::super::persistence::create_task_layout(storage, "reclaim", task_id).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.into(),
        "reclaim".into(),
        "owned reclaim fixture".into(),
        root.to_path_buf(),
        Some(root.to_path_buf()),
        None,
        true,
        false,
    );
    // Terminal group kills require a saved process identity so a reused PID
    // cannot authorize signaling an unrelated group. Record the real spawn's
    // ownership evidence before overriding its status for the fixture.
    if let Some(pid) = pid {
        metadata.mark_running(pid, i32::try_from(pid).expect("owned PGID fits i32"));
        assert!(
            metadata.child_start_time_ms.is_some(),
            "owned fixture child must have a readable process identity"
        );
    }
    metadata.status = status;
    metadata.live_descendants = pid.map(|pid| {
        vec![super::super::process::LiveDescendant {
            pid,
            comm: "sleep".into(),
            argv0: "sleep".into(),
            user: None,
            command: None,
        }]
    });
    super::super::persistence::write_task(&layout.paths.json, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, layout.paths, true)
        .unwrap();
    // Keep terminal fixtures in the watchdog index without starting a watchdog.
    let task = registry.task_for_session(task_id, "reclaim").unwrap();
    registry
        .inner
        .watchdog_tasks
        .lock()
        .unwrap()
        .insert(task_id.into(), task);
}

pub fn refuse_reclaim_kills(registry: &BgTaskRegistry) {
    registry
        .inner
        .reclaim_kill_noop
        .store(true, Ordering::SeqCst);
}

pub fn kill_attempts(registry: &BgTaskRegistry) -> usize {
    registry.inner.reclaim_kill_attempts.load(Ordering::SeqCst)
}

pub fn completion_attempts(registry: &BgTaskRegistry) -> usize {
    registry
        .inner
        .completion_processing_attempts
        .load(Ordering::SeqCst)
}

pub fn deliver_terminal_and_remove_output(registry: &BgTaskRegistry, id: &str) {
    let task = registry.task_for_session(id, "reclaim").unwrap();
    fs::write(&task.paths.stdout, "finished\n").unwrap();
    fs::write(&task.paths.stderr, "").unwrap();
    registry.post_terminal_transition(&task, true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !registry
        .pending_completions_for_session("reclaim")
        .iter()
        .any(|completion| completion.task_id == id)
    {
        assert!(Instant::now() < deadline, "completion was not published");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        registry.ack_completions_for_session(Some("reclaim"), &[id.into()]),
        vec![id.to_string()]
    );
    assert!(task.lock_state().unwrap().metadata.completion_delivered);
    fs::remove_file(&task.paths.stdout).unwrap();
    fs::remove_file(&task.paths.stderr).unwrap();
}

pub fn disown_group_identities(registry: &BgTaskRegistry, id: &str) {
    let task = registry.task_for_session(id, "reclaim").unwrap();
    let mut state = task.lock_state().unwrap();
    *state.metadata.child_start_time_ms.as_mut().unwrap() += 1;
    for identity in &mut state.metadata.group_member_identities {
        identity.started_at_ms += 1;
    }
}

pub struct OwnedGroup(std::process::Child);

impl OwnedGroup {
    pub fn child(&self) -> &std::process::Child {
        &self.0
    }

    pub fn child_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }

    pub fn spawn() -> Self {
        use std::os::unix::process::CommandExt;
        Self(
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn finish_task(registry: &BgTaskRegistry, id: &str) {
    registry
        .task_for_session(id, "reclaim")
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .status = BgTaskStatus::Killed;
}

pub fn clear_descendant_sample(registry: &BgTaskRegistry, id: &str) {
    registry
        .task_for_session(id, "reclaim")
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .live_descendants = None;
}

pub fn while_task_map_locked<T>(registry: &BgTaskRegistry, call: impl FnOnce() -> T) -> T {
    let _held = registry.inner.tasks.lock().unwrap();
    call()
}

pub fn reclaim_group_samples(
    registry: &BgTaskRegistry,
    root: &crate::path_identity::ProjectRootId,
) -> Option<Vec<String>> {
    let tasks = registry.inner.tasks.try_lock().ok()?;
    Some(tasks.values().filter(|task| task.project_root == root.as_path()).map(|task| {
        let Ok(state) = task.state.try_lock() else {
            return format!("{}: task state unavailable", task.task_id);
        };
        let metadata = &state.metadata;
        let members = metadata.live_descendants.as_ref().map(Vec::len);
        format!(
            "{}: status={:?}, pgid={:?}, sample_pending={}, members={members:?}, omitted={}, unavailable={:?}, group_unowned={}",
            task.task_id, metadata.status, metadata.pgid, state.descendant_sample_pending,
            metadata.live_descendants_omitted, metadata.live_descendants_unavailable,
            state.root_reclaim_group_unowned,
        )
    }).collect())
}
