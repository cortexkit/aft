use super::*;

#[test]
fn task_scope_placement_never_retries_a_released_gate() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let launch = super::super::task_scope::launch_for_test(&random_slug());
    let marker = root.path().join("payload-count");
    let mut child = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!("read ready; printf x >> '{}'; exit 0", marker.display()),
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"run\n").unwrap();
    child.wait().unwrap();
    let mut child = Child::from(child);
    let r = BgTaskRegistry::default();
    let result = r.place_task_scope(&mut child, &launch);
    assert!(result.is_err(), "released gate allowed a payload retry");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
}

#[test]
fn task_scope_placement_refuses_a_live_unplaced_child() {
    let id = random_slug();
    let launch = super::super::task_scope::launch_for_test(&id);
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "read ready; exit 0"])
        .stdin(std::process::Stdio::piped());
    super::super::process::start_new_session(&mut command);
    let mut child = Child::from(command.spawn().unwrap());
    let stopped = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = stopped.clone();
    let result = super::super::task_scope::with_stop_observer_for_test(
        move |unit| observed.borrow_mut().push(unit.to_owned()),
        || BgTaskRegistry::default().place_task_scope(&mut child, &launch),
    );
    assert!(result
        .as_ref()
        .unwrap_err()
        .contains("task scope placement timed out"));
    assert_eq!(
        *stopped.borrow(),
        vec![super::super::task_scope::scope_unit(&id).unwrap()]
    );
    assert!(
        child.try_wait().unwrap().is_some(),
        "unplaced child was not reaped"
    );
}

struct StopOnDrop(String);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Ok(Some(scope)) = super::super::task_scope::lookup(&self.0) {
            let _ = scope.stop();
        }
    }
}

struct ModuleScopeGuard {
    unit: String,
    root: std::path::PathBuf,
    child: Option<std::process::Child>,
}

impl Drop for ModuleScopeGuard {
    fn drop(&mut self) {
        if let Ok(id) = std::fs::read_to_string(self.root.join("task-id")) {
            if let Ok(Some(scope)) = super::super::task_scope::lookup(&id) {
                let _ = scope.stop();
            }
        }
        let _ = super::super::task_scope::user_manager_command(std::path::Path::new("systemctl"))
            .args(["--user", "stop", &self.unit])
            .status();
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
    }
}

#[test]
fn task_scope_module_helper() {
    let Some(root) = std::env::var_os("AFT_TEST_TASK_SCOPE_MODULE_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let cap = std::fs::read_to_string(root.join("cap"))
        .ok()
        .map(|value| value.parse::<u64>().unwrap());
    let registry = BgTaskRegistry::default();
    let id = scope_spawn(
        &registry,
        &root,
        "echo before; while [ ! -e release ]; do sleep .02; done; echo after; exit 7",
        super::super::HardKill::After(Duration::from_secs(120)),
        cap,
    );
    std::fs::write(root.join("task-id"), id).unwrap();
    std::fs::write(root.join("module-pid"), std::process::id().to_string()).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn task_scope_survives_module_cgroup_kill() {
    if !manager_available() {
        return;
    }
    for (cap, downtime_exit) in [(None, false), (Some(64), false), (None, true)] {
        let root = tempfile::tempdir().unwrap();
        if let Some(cap) = cap {
            std::fs::write(root.path().join("cap"), cap.to_string()).unwrap();
        }
        let mut module = ModuleScopeGuard {
            unit: format!("aft-test-module-{}.scope", random_slug()),
            root: root.path().into(),
            child: None,
        };
        module.child = Some(
            std::process::Command::new("systemd-run")
                .args(["--user", "--scope", "--collect", "--quiet"])
                .arg(format!("--unit={}", module.unit))
                .args([
                    "--property=RuntimeMaxSec=180s",
                    "--property=TimeoutStopSec=2s",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bash_background::registry::task_scope_tests::task_scope_module_helper",
                    "--nocapture",
                ])
                .env("AFT_TEST_TASK_SCOPE_MODULE_ROOT", root.path())
                .spawn()
                .unwrap(),
        );
        until(|| {
            std::fs::read_to_string(root.path().join("module-pid"))
                .ok()
                .is_some_and(|text| text.parse::<u32>().is_ok())
        });
        let id = std::fs::read_to_string(root.path().join("task-id")).unwrap();
        let pid = std::fs::read_to_string(root.path().join("module-pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_ne!(pid, std::process::id());
        let module_cgroup = super::super::task_scope::current_cgroup(pid).unwrap();
        assert_eq!(
            module_cgroup.file_name().unwrap().to_str().unwrap(),
            module.unit
        );
        let task_scope = super::super::task_scope::lookup(&id)
            .unwrap()
            .expect("task scope missing before module kill");
        assert_eq!(
            task_scope.cgroup().parent().unwrap().file_name().unwrap(),
            "aft.slice"
        );
        assert!(
            !task_scope.cgroup().starts_with(&module_cgroup),
            "task is nested in the module cgroup"
        );
        let layout =
            super::super::persistence::resolve_task(root.path(), "task-scope", &id).unwrap();
        if cap.is_some() {
            let policy = read_task_at(&layout).unwrap().memory_policy.unwrap();
            assert_eq!(policy.cgroup.as_deref(), Some(task_scope.cgroup()));
            assert!(!policy.cgroup.unwrap().starts_with(&module_cgroup));
        }
        until(|| std::fs::read_to_string(&layout.paths.stdout).unwrap_or_default() == "before\n");
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .open(module_cgroup.join("cgroup.kill"))
            .unwrap()
            .write_all(b"1")
            .unwrap();
        until(|| module.child.as_mut().unwrap().try_wait().unwrap().is_some());
        if downtime_exit {
            std::fs::write(root.path().join("release"), "1").unwrap();
            until(|| {
                read_exit_marker(&layout.paths).ok().flatten()
                    == Some(super::super::persistence::ExitMarker::Code(7))
            });
        }
        let registry = BgTaskRegistry::default();
        registry.configure_task_scope_runtime(true, Duration::from_secs(86400));
        registry.replay_session(root.path(), "task-scope").unwrap();
        if !downtime_exit {
            assert_eq!(
                registry
                    .task(&id)
                    .unwrap()
                    .lock_state()
                    .unwrap()
                    .metadata
                    .status,
                super::super::BgTaskStatus::Running
            );
            std::fs::write(root.path().join("release"), "1").unwrap();
        }
        until(|| {
            let task = registry.task(&id).unwrap();
            registry.poll_task(&task).unwrap();
            task.is_terminal()
        });
        let status = registry
            .status(&id, "task-scope", None, None, 1024)
            .unwrap();
        assert_eq!(status.exit_code, Some(7));
        assert_ne!(status.info.status, super::super::BgTaskStatus::FateUnknown);
        assert_eq!(
            std::fs::read_to_string(&layout.paths.stdout).unwrap(),
            "before\nafter\n"
        );
        assert_eq!(std::fs::read_to_string(&layout.paths.stderr).unwrap(), "");
        assert_eq!(
            registry
                .pending_completions_for_session("task-scope")
                .iter()
                .filter(|completion| completion.task_id == id)
                .count(),
            1
        );
        assert_eq!(
            registry.ack_completions_for_session(Some("task-scope"), &[id.clone()]),
            vec![id.clone()]
        );
        registry.detach();
        let third = BgTaskRegistry::default();
        third.configure_task_scope_runtime(true, Duration::from_secs(86400));
        third.replay_session(root.path(), "task-scope").unwrap();
        assert!(third
            .pending_completions_for_session("task-scope")
            .iter()
            .all(|completion| completion.task_id != id));
    }
}

#[test]
fn task_scope_fixed_backstop_works_without_registry() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.configure_task_scope_runtime(true, Duration::from_secs(1));
    let id = registry
        .spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "echo alive > alive; sleep 60",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "task-scope".into(),
            root.path().into(),
            HashMap::new(),
            super::super::HardKill::Default,
            None,
            root.path().into(),
            super::super::TaskSlot::Background { max: 4 },
            true,
            false,
            Some(root.path().into()),
            true,
        )
        .unwrap();
    let _stop = StopOnDrop(id.clone());
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    until(|| root.path().join("alive").exists());
    registry.detach();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(scope.cgroup().join("cgroup.events"))
        .is_ok_and(|text| !text.lines().any(|line| line == "populated 0"))
    {
        assert!(
            Instant::now() < deadline,
            "fixed scope backstop did not stop the detached task"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn scope_spawn(
    r: &BgTaskRegistry,
    root: &Path,
    command: &str,
    limit: super::super::HardKill,
    cap: Option<u64>,
) -> String {
    r.configure_task_scope_runtime(true, Duration::from_secs(86400));
    r.spawn_with_shell_in_slot_typed(
        SpawnPlan::Unsandboxed,
        command,
        super::super::BashShell::Bash,
        PathBuf::from("/bin/bash"),
        "task-scope".into(),
        root.to_owned(),
        HashMap::from([(LINUX_SCOPE_ENV.into(), "1".into())]),
        limit,
        cap,
        root.to_owned(),
        super::super::TaskSlot::Background { max: 8 },
        true,
        false,
        Some(root.to_owned()),
        true,
    )
    .unwrap()
}

fn manager_available() -> bool {
    match super::super::task_scope::probe_user_manager(Duration::from_secs(1)) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("SKIP task scopes: user manager probe failed: {e}");
            false
        }
    }
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "task scope condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn task_scope_spawn_has_named_membership() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    let id = scope_spawn(
        &r,
        root.path(),
        "sleep 60",
        super::super::HardKill::After(Duration::from_secs(90)),
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let task = r.task(&id).unwrap();
    let pid = task.lock_state().unwrap().metadata.child_pid.unwrap();
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let result = super::super::task_scope::matches_membership(&id, &text);
    r.kill(&id, "task-scope").unwrap();
    assert!(result, "task is still in the module kill domain: {text}");
}

#[test]
fn task_scope_memory_uses_the_task_scope() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    let id = scope_spawn(
        &r,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        Some(64),
    );
    let _stop = StopOnDrop(id.clone());
    let task = r.task(&id).unwrap();
    let policy = task
        .lock_state()
        .unwrap()
        .metadata
        .memory_policy
        .clone()
        .unwrap();
    let path = policy.cgroup.clone().unwrap();
    let max = std::fs::read_to_string(path.join("memory.max")).unwrap();
    let swap = std::fs::read_to_string(path.join("memory.swap.max")).unwrap();
    let group = std::fs::read_to_string(path.join("memory.oom.group")).unwrap();
    r.kill(&id, "task-scope").unwrap();
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        format!("aft-task-{id}.scope")
    );
    assert_eq!(max.trim(), "67108864");
    assert_eq!(swap.trim(), "0");
    assert_eq!(group.trim(), "1");
}

#[test]
fn task_scope_memory_denied_setup_keeps_payload_gate_closed() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    r.configure_task_scope_runtime(true, Duration::from_secs(86400));
    let result = super::super::task_scope::with_oom_group_error_for_test(|| {
        r.spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "printf unsafe > payload-ran",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "denied-group".into(),
            root.path().into(),
            HashMap::new(),
            super::super::HardKill::Default,
            Some(64),
            root.path().into(),
            super::super::TaskSlot::Background { max: 8 },
            false,
            false,
            None,
            true,
        )
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("memory.oom.group write denied"));
    assert!(!root.path().join("payload-ran").exists());
}

#[test]
fn task_scope_kill_reaches_setsid_descendant() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    let command = "setsid /bin/sh -c 'trap \"\" TERM; echo $$ > escaped; while :; do sleep 1; done' & while [ ! -s escaped ]; do sleep .02; done; sleep 60";
    let id = scope_spawn(
        &r,
        root.path(),
        command,
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    until(|| root.path().join("escaped").exists());
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    r.kill(&id, "task-scope").unwrap();
    until(
        || match std::fs::read_to_string(scope.cgroup().join("cgroup.events")) {
            Ok(text) => text.lines().any(|line| line == "populated 0"),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        },
    );
}

#[test]
fn task_scope_kill_follows_group_signal_order() {
    let root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    r.inner.shutdown.store(true, Ordering::SeqCst);
    let id = r
        .spawn(
            SpawnPlan::Unsandboxed,
            "echo ready; sleep 60",
            "order".into(),
            root.path().into(),
            HashMap::new(),
            super::super::HardKill::Default,
            None,
            root.path().into(),
            8,
            false,
            false,
            None,
        )
        .unwrap();
    let task = r.task(&id).unwrap();
    until(|| {
        std::fs::read_to_string(&task.paths.stdout)
            .unwrap_or_default()
            .contains("ready")
    });
    let pgid = task.lock_state().unwrap().metadata.pgid.unwrap();
    let cgroup = root.path().join("fake-cgroup");
    std::fs::create_dir(&cgroup).unwrap();
    std::fs::write(cgroup.join("cgroup.kill"), "0").unwrap();
    r.inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            cgroup.clone(),
        ));
    let events = Arc::new(Mutex::new(Vec::new()));
    super::super::process::test_hooks::with_signal_events(pgid, events.clone(), || {
        super::super::task_scope::with_kill_events_for_test(events.clone(), || {
            r.kill(&id, "order").unwrap()
        })
    });
    assert_eq!(
        *events.lock().unwrap(),
        vec!["group-term", "group-kill", "scope-kill"]
    );
    assert_eq!(
        std::fs::read_to_string(cgroup.join("cgroup.kill")).unwrap(),
        "1"
    );
}

#[test]
fn task_scope_kill_reports_live_controller_permission_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let id = random_slug();
    let control = root.path().join("cgroup.kill");
    std::fs::write(&control, "0").unwrap();
    std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o400)).unwrap();
    let r = BgTaskRegistry::default();
    r.inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            root.path().into(),
        ));
    assert!(r
        .kill_task_scope(&id)
        .unwrap_err()
        .contains("failed to confirm or kill task scope"));
    std::fs::remove_file(&control).unwrap();
    assert!(
        r.kill_task_scope(&id).is_err(),
        "live cgroup without its controller was accepted"
    );
    let gone = root.path().join("collected");
    r.inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(&id, gone));
    r.kill_task_scope(&id).unwrap();
}

#[test]
fn task_scope_replay_adopts_and_keeps_output() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    let id = scope_spawn(
        &first,
        root.path(),
        "echo before; while [ ! -e release ]; do sleep .02; done; echo after; exit 7",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let paths = first.task(&id).unwrap().paths.clone();
    until(|| {
        std::fs::read_to_string(&paths.stdout)
            .unwrap_or_default()
            .contains("before")
    });
    first.detach();
    let second = BgTaskRegistry::default();
    second.configure_task_scope_runtime(true, Duration::from_secs(86400));
    second.replay_session(root.path(), "task-scope").unwrap();
    assert!(
        second.inner.task_scopes.get(&id).is_some(),
        "replay did not retain the exact scope"
    );
    assert_eq!(
        second
            .task(&id)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .status,
        super::super::BgTaskStatus::Running
    );
    std::fs::write(root.path().join("release"), "1").unwrap();
    until(|| {
        let task = second.task(&id).unwrap();
        second.poll_task(&task).unwrap();
        task.is_terminal()
    });
    assert_eq!(
        second
            .status(&id, "task-scope", None, None, 1024)
            .unwrap()
            .exit_code,
        Some(7)
    );
    assert_eq!(
        std::fs::read_to_string(paths.stdout).unwrap(),
        "before\nafter\n"
    );
}

#[test]
fn task_scope_replay_identity_refusal_does_not_block_session() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    first.inner.shutdown.store(true, Ordering::SeqCst);
    let a = scope_spawn(
        &first,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _a = StopOnDrop(a.clone());
    let b = scope_spawn(
        &first,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _b = StopOnDrop(b.clone());
    let (bad, healthy) = if a < b { (&a, &b) } else { (&b, &a) };
    let layout = super::super::persistence::resolve_task(root.path(), "task-scope", bad).unwrap();
    let mut metadata = read_task_at(&layout).unwrap();
    let healthy_metadata = first
        .task(healthy)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .clone();
    metadata.child_pid = healthy_metadata.child_pid;
    metadata.child_start_time_ms = healthy_metadata.child_start_time_ms;
    write_task_at(&layout, &metadata).unwrap();
    first.detach();
    let second = BgTaskRegistry::default();
    second.configure_task_scope_runtime(true, Duration::from_secs(86400));
    second.replay_session(root.path(), "task-scope").unwrap();
    let rejected = second.status(bad, "task-scope", None, None, 1024).unwrap();
    assert_eq!(
        rejected.info.status,
        super::super::BgTaskStatus::FateUnknown
    );
    assert_eq!(rejected.exit_code, None);
    assert_eq!(
        second
            .task(healthy)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .status,
        super::super::BgTaskStatus::Running
    );
    second.kill(healthy, "task-scope").unwrap();
}

#[test]
fn task_scope_kill_without_handle_uses_exact_identity() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    let id = scope_spawn(&first, root.path(), "setsid /bin/sh -c 'trap \"\" TERM; echo $$ > escaped; while :; do sleep 1; done' & sleep 60", super::super::HardKill::Default, None);
    let _stop = StopOnDrop(id.clone());
    until(|| root.path().join("escaped").exists());
    let task = first.task(&id).unwrap();
    let metadata = task.lock_state().unwrap().metadata.clone();
    let paths = task.paths.clone();
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    first.detach();
    let second = BgTaskRegistry::default();
    second.inner.shutdown.store(true, Ordering::SeqCst);
    second
        .insert_rehydrated_task(metadata, paths, true)
        .unwrap();
    assert!(second.inner.task_scopes.get(&id).is_none());
    second.kill(&id, "task-scope").unwrap();
    until(|| {
        std::fs::read_to_string(scope.cgroup().join("cgroup.events"))
            .map_or(true, |text| text.lines().any(|line| line == "populated 0"))
    });
}

#[test]
fn task_scope_kill_without_handle_refuses_foreign_identity() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    let id = scope_spawn(
        &first,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let task = first.task(&id).unwrap();
    let mut metadata = task.lock_state().unwrap().metadata.clone();
    metadata.child_start_time_ms = metadata.child_start_time_ms.map(|start| start + 1);
    first.detach();
    let second = BgTaskRegistry::default();
    second
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    assert!(
        second.kill_task_scope(&id).is_err(),
        "unconfirmed scope kill reported success"
    );
    assert!(second.inner.task_scopes.get(&id).is_none());
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    assert!(
        std::fs::read_to_string(scope.cgroup().join("cgroup.events"))
            .unwrap()
            .lines()
            .any(|line| line == "populated 1")
    );
}

#[test]
fn task_scope_kill_distinguishes_absent_unconfirmed_and_failed() {
    if !manager_available() {
        return;
    }
    let registry = BgTaskRegistry::default();
    registry.kill_task_scope(&random_slug()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let id = scope_spawn(
        &registry,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let other = BgTaskRegistry::default();
    assert!(
        other.kill_task_scope(&id).is_err(),
        "present scope without identity reported success"
    );
    let fake = tempfile::tempdir().unwrap();
    std::fs::create_dir(fake.path().join("cgroup.kill")).unwrap();
    other
        .inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            fake.path().into(),
        ));
    let error = other.kill_task_scope(&id).unwrap_err();
    assert_eq!(error, "failed to confirm or kill task scope");
}

#[test]
fn task_scope_replay_keeps_live_identity_without_user_manager() {
    const ROOT: &str = "AFT_TEST_SCOPE_REPLAY_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        let id = std::fs::read_to_string(root.join("task-id")).unwrap();
        let registry = BgTaskRegistry::default();
        registry.configure_task_scope_runtime(true, Duration::from_secs(86400));
        registry.replay_session(&root, "task-scope").unwrap();
        let status = registry
            .status(&id, "task-scope", None, None, 1024)
            .unwrap();
        let expected = if std::env::var_os("AFT_TEST_SCOPE_GONE").is_some() {
            super::super::BgTaskStatus::FateUnknown
        } else {
            super::super::BgTaskStatus::Running
        };
        assert_eq!(
            status.info.status, expected,
            "manager failure changed live process status"
        );
        assert_eq!(status.exit_code, None);
        registry.detach();
        return;
    }
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let id = scope_spawn(
        &registry,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    std::fs::write(root.path().join("task-id"), &id).unwrap();
    let pid = registry
        .task(&id)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .child_pid
        .unwrap();
    let mut owned_child = {
        let task = registry.task(&id).unwrap();
        let mut state = task.lock_state().unwrap();
        match &mut state.runtime {
            TaskRuntime::Piped(child) => child.take().unwrap(),
            _ => unreachable!(),
        }
    };
    registry.detach();
    for (broken, gone) in [(false, false), (true, false), (true, true)] {
        if gone {
            super::super::task_scope::lookup(&id)
                .unwrap()
                .unwrap()
                .kill()
                .unwrap();
            until(|| owned_child.try_wait().unwrap().is_some());
            until(|| crate::root_cache::process_start_time_ms(pid).is_none());
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "bash_background::registry::task_scope_tests::task_scope_replay_keeps_live_identity_without_user_manager", "--nocapture"]).env(ROOT, root.path());
        if broken {
            child
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unix:path=/nonexistent/aft-probe-bus",
                )
                .env("XDG_RUNTIME_DIR", "/nonexistent/aft-probe-runtime");
        }
        if gone {
            child.env("AFT_TEST_SCOPE_GONE", "1");
        }
        assert!(
            child.status().unwrap().success(),
            "replay subprocess failed"
        );
    }
}

#[test]
fn task_scope_scoped_task_without_marker_is_fate_unknown() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    let id = scope_spawn(
        &first,
        root.path(),
        "echo ready; sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let task = first.task(&id).unwrap();
    let paths = task.paths.clone();
    let pid = task.lock_state().unwrap().metadata.child_pid.unwrap();
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    until(|| std::fs::read_to_string(&paths.stdout).unwrap_or_default() == "ready\n");
    drop(task);
    first.detach();
    scope.kill().unwrap();
    until(|| {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        waited == pid as i32
            || (waited == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
    });
    assert!(
        read_exit_marker(&paths).unwrap().is_none(),
        "fixture unexpectedly wrote an exit marker"
    );
    let second = BgTaskRegistry::default();
    second.configure_task_scope_runtime(true, Duration::from_secs(86400));
    second.replay_session(root.path(), "task-scope").unwrap();
    let snapshot = second.status(&id, "task-scope", None, None, 1024).unwrap();
    assert_eq!(
        snapshot.info.status,
        super::super::BgTaskStatus::FateUnknown
    );
    assert_eq!(snapshot.exit_code, None, "replay invented an exit code");
}

#[test]
fn task_scope_cleanup_and_hook_require_readable_owned_records() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let foreign_root = tempfile::tempdir().unwrap();
    let r = BgTaskRegistry::default();
    r.inner.shutdown.store(true, Ordering::SeqCst);
    let terminal = scope_spawn(
        &r,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _terminal = StopOnDrop(terminal.clone());
    let running = scope_spawn(
        &r,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _running = StopOnDrop(running.clone());
    let foreign = scope_spawn(
        &r,
        foreign_root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _foreign = StopOnDrop(foreign.clone());
    let unreadable = scope_spawn(
        &r,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _unreadable = StopOnDrop(unreadable.clone());
    let task =
        super::super::persistence::resolve_task(root.path(), "task-scope", &terminal).unwrap();
    let mut metadata = read_task_at(&task).unwrap();
    metadata.mark_terminal(super::super::BgTaskStatus::Completed, Some(0), None);
    write_task_at(&task, &metadata).unwrap();
    let unreadable_task =
        super::super::persistence::resolve_task(root.path(), "task-scope", &unreadable).unwrap();
    std::fs::write(&unreadable_task.paths.json, "corrupt").unwrap();
    assert_eq!(r.cleanup_terminal_task_scopes(root.path()).unwrap(), 1);
    for id in [&running, &foreign, &unreadable] {
        assert!(
            super::super::task_scope::lookup(id).unwrap().is_some(),
            "cleanup stopped an unowned or running scope"
        );
    }
    assert_eq!(r.end_all_task_scopes(root.path()).unwrap(), 1);
    for id in [&foreign, &unreadable] {
        assert!(
            super::super::task_scope::lookup(id).unwrap().is_some(),
            "hook used the handle cache as ownership proof"
        );
    }
}

#[test]
fn task_scope_cleanup_worker_is_root_keyed_and_daemon_only() {
    let root = tempfile::tempdir().unwrap();
    let first = BgTaskRegistry::default();
    let second = BgTaskRegistry::default();
    first.configure_task_scope_runtime(true, Duration::from_secs(86400));
    second.configure_task_scope_runtime(true, Duration::from_secs(86400));
    first.start_task_scope_cleanup(root.path());
    second.start_task_scope_cleanup(&root.path().join("."));
    let key = canonicalized_path(root.path());
    until(|| {
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|slot| slot.done)
    });
    assert_eq!(
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .runs,
        1
    );
    let standalone_root = tempfile::tempdir().unwrap();
    BgTaskRegistry::default().start_task_scope_cleanup(standalone_root.path());
    assert!(!task_scope_cleanup_slots()
        .lock()
        .unwrap()
        .contains_key(&canonicalized_path(standalone_root.path())));
}

#[test]
fn task_scope_actor_created_after_daemon_attach_has_named_membership() {
    if !manager_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let app = crate::context::App::default_shared();
    let first = crate::context::AppContext::from_app(app.clone(), crate::config::Config::default());
    assert!(!first.is_daemon_mode());
    app.set_subc_connection_file(root.path().join("connection.json"));
    let second = crate::context::AppContext::from_app(
        app,
        crate::config::Config {
            project_root: Some(root.path().join("second")),
            ..Default::default()
        },
    );
    assert!(second.is_daemon_mode());
    let registry = second.bash_background();
    let id = registry
        .spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "echo ready; sleep 60",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "second-root".into(),
            root.path().to_owned(),
            HashMap::from([(LINUX_SCOPE_ENV.into(), "1".into())]),
            super::super::HardKill::After(Duration::from_secs(90)),
            None,
            root.path().to_owned(),
            super::super::TaskSlot::Background { max: 8 },
            false,
            false,
            Some(root.path().to_owned()),
            true,
        )
        .unwrap();
    let _stop = StopOnDrop(id.clone());
    let task = registry.task(&id).unwrap();
    until(|| {
        std::fs::read_to_string(&task.paths.stdout)
            .unwrap_or_default()
            .contains("ready")
    });
    let pid = task.lock_state().unwrap().metadata.child_pid.unwrap();
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    registry.kill(&id, "second-root").unwrap();
    assert!(
        super::super::task_scope::matches_membership(&id, &text),
        "new actor missed the shared daemon fact: {text}"
    );
}
