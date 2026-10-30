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
        .contains(&format!("failed to kill task scope aft-task-{id}.scope")));
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
