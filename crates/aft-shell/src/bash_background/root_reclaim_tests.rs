use super::root_reclaim_fixture::*;
use super::*;

#[test]
fn root_reclaim_waits_for_the_initial_descendant_sample() {
    let registry = BgTaskRegistry::default();
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_id = crate::path_identity::ProjectRootId::from_path(root.path()).unwrap();
    let mut group = OwnedGroup::spawn();
    let id = "bash-0000000000000094";
    register_task(
        &registry,
        storage.path(),
        root.path(),
        id,
        BgTaskStatus::Completed,
        Some(group.child().id()),
    );
    let task = registry.task_for_session(id, "reclaim").unwrap();
    {
        let mut state = task.lock_state().unwrap();
        state.descendant_sampling_started = true;
        state.descendant_sample_pending = true;
        state.metadata.live_descendants = None;
    }
    assert_eq!(registry.kill_running_tasks_for_root(root.path()), 0);
    assert!(group.child_mut().try_wait().unwrap().is_none());
    assert_eq!(completion_attempts(&registry), 0);
    assert_eq!(
        registry.try_has_reclaimable_tasks_for_root(&root_id),
        Some(true)
    );

    registry.sample_task_process_group(&task).unwrap();
    assert_eq!(registry.kill_running_tasks_for_root(root.path()), 1);
    assert_eq!(
        registry.try_has_reclaimable_tasks_for_root(&root_id),
        Some(false)
    );
    assert_eq!(completion_attempts(&registry), 1);
}

#[test]
fn root_reclaim_probe_ignores_foreign_tasks_and_terminal_history() {
    let registry = BgTaskRegistry::default();
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_id = crate::path_identity::ProjectRootId::from_path(root.path()).unwrap();
    let foreign = tempfile::tempdir().unwrap();
    register_task(
        &registry,
        storage.path(),
        root.path(),
        "bash-0000000000000010",
        BgTaskStatus::Completed,
        None,
    );
    register_task(
        &registry,
        storage.path(),
        foreign.path(),
        "bash-0000000000000011",
        BgTaskStatus::Running,
        None,
    );
    let foreign_task = registry
        .task_for_session("bash-0000000000000011", "reclaim")
        .unwrap();
    let _held = foreign_task.state.lock().unwrap();
    ROOT_LOCK_PROBE.with(|probe| {
        *probe.borrow_mut() = Some(Box::new(|| panic!("reclaim probe touched filesystem")))
    });
    assert_eq!(
        registry.try_has_reclaimable_tasks_for_root(&root_id),
        Some(false)
    );
    ROOT_LOCK_PROBE.with(|probe| *probe.borrow_mut() = None);
}

#[test]
fn root_reclaim_probe_defers_on_own_task_or_registry_contention() {
    let registry = BgTaskRegistry::default();
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_id = crate::path_identity::ProjectRootId::from_path(root.path()).unwrap();
    let id = "bash-0000000000000012";
    register_task(
        &registry,
        storage.path(),
        root.path(),
        id,
        BgTaskStatus::Running,
        None,
    );
    assert_eq!(
        registry.try_has_reclaimable_tasks_for_root(&root_id),
        Some(true)
    );
    let task = registry.task_for_session(id, "reclaim").unwrap();
    let held = task.state.lock().unwrap();
    assert_eq!(registry.try_has_reclaimable_tasks_for_root(&root_id), None);
    drop(held);
    let _held = registry.inner.tasks.lock().unwrap();
    assert_eq!(registry.try_has_reclaimable_tasks_for_root(&root_id), None);
}

#[test]
fn root_reclaim_identity_is_resolved_before_spawn_admission() {
    for kind in ["pipes", "pty", "remote"] {
        let registry = BgTaskRegistry::default();
        registry
            .inner
            .watchdog_started
            .store(true, Ordering::SeqCst);
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = Arc::clone(&calls);
        let probe_registry = registry.clone();
        ROOT_LOCK_PROBE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                assert!(
                    probe_registry.inner.background_admission.try_lock().is_ok(),
                    "root identity I/O held admission lock"
                );
                assert!(
                    probe_registry.inner.tasks.try_lock().is_ok(),
                    "root identity I/O held registry lock"
                );
            }))
        });
        // A zero slot limit stops before allocating or spawning a process, but
        // still takes admission. Root resolution must happen before that lock.
        let result = match kind {
            "pipes" => registry.spawn(
                SpawnPlan::Unsandboxed,
                "true",
                "reclaim".into(),
                root.path().into(),
                HashMap::new(),
                super::super::HardKill::After(Duration::from_secs(30)),
                None,
                storage.path().into(),
                0,
                false,
                false,
                Some(root.path().into()),
            ),
            "pty" => registry.spawn_pty(
                SpawnPlan::Unsandboxed,
                "true",
                "reclaim".into(),
                root.path().into(),
                HashMap::new(),
                super::super::HardKill::After(Duration::from_secs(30)),
                None,
                storage.path().into(),
                0,
                false,
                false,
                Some(root.path().into()),
                24,
                80,
            ),
            "remote" => registry
                .spawn_remote_typed(
                    super::super::RemoteLaunch {
                        requested_platform: None,
                        params: Default::default(),
                        explicit_runon: true,
                        connection_file: None,
                        harness: "reclaim".into(),
                        session: "reclaim".into(),
                        requested_vcpus: None,
                        requested_memory_mib: None,
                        requested_network: false,
                        configured_env_names: Vec::new(),
                        return_paths: Vec::new(),
                    },
                    SpawnPlan::Unsandboxed,
                    "true",
                    "/bin/sh".into(),
                    "reclaim".into(),
                    root.path().into(),
                    HashMap::new(),
                    super::super::HardKill::After(Duration::from_secs(30)),
                    None,
                    storage.path().into(),
                    super::super::TaskSlot::Background { max: 0 },
                    false,
                    false,
                    Some(root.path().into()),
                    false,
                )
                .map_err(|error| error.to_string()),
            _ => unreachable!(),
        };
        ROOT_LOCK_PROBE.with(|hook| hook.borrow_mut().take());
        assert!(
            result.is_err(),
            "{kind}: slot limit must refuse before spawning"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{kind}: root lookup was not exercised"
        );
    }
}

#[test]
fn root_reclaim_registration_reuses_identity_under_admission_and_claim_locks() {
    let registry = BgTaskRegistry::default();
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let alias = storage.path().join("root-alias");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    let canonical_root = canonicalized_path(&alias);
    let layout = super::super::persistence::create_task_layout(
        storage.path(),
        "reclaim",
        "bash-0000000000000013",
    )
    .unwrap();
    let metadata = PersistedTask::starting(
        "bash-0000000000000013".into(),
        "reclaim".into(),
        "no process".into(),
        alias.clone(),
        Some(alias.clone()),
        None,
        false,
        false,
    );
    std::fs::remove_file(alias).unwrap();
    let _admission =
        registry.background_admission_guard(super::super::TaskSlot::Background { max: 1 });
    let _claims = saved_remote_task_claims().lock().unwrap();
    ROOT_LOCK_PROBE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| {
            panic!("registration repeated root identity I/O under locks")
        }))
    });
    registry
        .insert_rehydrated_task_with_root(metadata, layout.paths, true, canonical_root.clone())
        .unwrap();
    ROOT_LOCK_PROBE.with(|hook| hook.borrow_mut().take());
    assert_eq!(
        registry
            .task_for_session("bash-0000000000000013", "reclaim")
            .unwrap()
            .project_root,
        canonical_root
    );
}
