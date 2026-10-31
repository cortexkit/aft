use super::*;

#[test]
fn task_scope_report_only_deadline_has_safe_kill_reason() {
    for source in [HardKillSource::Scope, HardKillSource::ScopedDefault] {
        assert_eq!(
            hard_kill_reason(HardKillDeadline {
                limit_ms: 1000,
                source
            }),
            "killed by AFT's task time limit of 1s (exit 124)",
        );
    }
}

#[test]
fn task_scope_failed_spawn_logs_stop_error_once_and_keeps_kill_result() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("cgroup.kill"), "0").unwrap();
    let id = random_slug();
    let registry = BgTaskRegistry::default();
    registry
        .inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            root.path().into(),
        ));
    let launch = super::super::task_scope::launch_for_test(&id);
    let (_, lines) = crate::logging::capture_log_lines(|| {
        super::super::task_scope::with_stop_error_for_test(|| {
            for _ in 0..2 {
                let mut child = Child::from(
                    std::process::Command::new("/bin/sh")
                        .args(["-c", "exit 0"])
                        .spawn()
                        .unwrap(),
                );
                let pid = child.id() as i32;
                registry
                    .cleanup_failed_scope_spawn(&id, &mut child, Some(&launch))
                    .unwrap();
                let mut status = 0;
                assert_eq!(
                    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                    -1
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
            }
        });
    });
    let message = format!("task scope aft-task-{id}.scope could not be stopped");
    assert_eq!(
        lines.iter().filter(|line| line.contains(&message)).count(),
        1,
        "failed spawn swallowed the scope-stop error"
    );
    assert!(!lines
        .iter()
        .any(|line| line.contains("injected stop error text")));
}

#[test]
fn task_scope_failed_spawn_stops_and_reaps_after_kill_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("cgroup.kill")).unwrap();
    let id = random_slug();
    let registry = BgTaskRegistry::default();
    registry
        .inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            root.path().into(),
        ));
    let launch = super::super::task_scope::launch_for_test(&id);
    let mut child = Child::from(
        std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap(),
    );
    let pid = child.id() as i32;
    let stopped = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = stopped.clone();
    let result = super::super::task_scope::with_stop_observer_for_test(
        move |unit| observed.borrow_mut().push(unit.to_owned()),
        || registry.cleanup_failed_scope_spawn(&id, &mut child, Some(&launch)),
    );
    assert!(result.is_err());
    assert_eq!(stopped.borrow().len(), 1, "kill error skipped scope stop");
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

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
#[ignore = "requires a systemd user manager"]
fn task_scope_survives_module_cgroup_kill() {
    require_manager();
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
        until(|| {
            registry
                .pending_completions_for_session("task-scope")
                .iter()
                .any(|completion| completion.task_id == id)
        });
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
#[ignore = "requires a systemd user manager"]
fn task_scope_deadline_survives_durable_starting_checkpoint() {
    require_manager();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.configure_task_scope_runtime(true, Duration::from_secs(60));
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *registry.inner.before_bootstrap_release.lock().unwrap() = Some((arrived_tx, release_rx));
    let release = Release(release_tx);
    let spawning = registry.clone();
    let project = root.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        spawning.spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "sleep 60",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "task-scope".into(),
            project.clone(),
            HashMap::new(),
            super::super::HardKill::Default,
            None,
            project.clone(),
            super::super::TaskSlot::Background { max: 4 },
            true,
            false,
            Some(project),
            true,
        )
    });
    let id = arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let _stop = StopOnDrop(id.clone());
    let layout = super::super::persistence::resolve_task(root.path(), "task-scope", &id).unwrap();
    let saved = read_task_at(&layout).unwrap();
    assert_eq!(saved.status, super::super::BgTaskStatus::Starting);
    let recovered = BgTaskRegistry::default();
    recovered.inner.shutdown.store(true, Ordering::SeqCst);
    recovered
        .insert_rehydrated_task(saved.clone(), layout.paths.clone(), true)
        .unwrap();
    assert_eq!(
        recovered
            .hard_kill_deadline(&id, "task-scope")
            .unwrap()
            .limit_ms,
        60000,
        "durable checkpoint lost the earlier scope deadline"
    );
    let mut old_record = saved;
    old_record.task_scope_runtime_ms = None;
    assert_eq!(
        HardKillDeadline::reported(&old_record, true)
            .unwrap()
            .limit_ms,
        DEFAULT_BG_TIMEOUT.as_millis() as u64,
        "old records invented a scope deadline"
    );
    drop(release);
    assert_eq!(worker.join().unwrap().unwrap(), id);
    registry.kill(&id, "task-scope").unwrap();
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_fixed_backstop_works_without_registry() {
    require_manager();
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
    assert_eq!(
        registry
            .hard_kill_deadline(&id, "task-scope")
            .unwrap()
            .limit_ms,
        1000,
        "reply ignored the earlier scope deadline"
    );
    let note =
        crate::commands::bash_orchestrate::kill_deadline_note(&registry, &id, "task-scope", true);
    assert!(
        !note.contains("so it is not killed while you keep waiting"),
        "{note}"
    );
    registry.detach();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !scope_stopped(&scope.cgroup().join("cgroup.events")) {
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

fn require_manager() {
    super::super::task_scope::probe_user_manager(Duration::from_secs(1))
        .expect("requires a systemd user manager");
}

fn scope_stopped(events: &Path) -> bool {
    match std::fs::read_to_string(events) {
        Ok(text) => text.lines().any(|line| line == "populated 0"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => panic!("cannot read cgroup.events: {error}"),
    }
}

#[test]
fn task_scope_stopped_requires_empty_or_missing_events() {
    let root = tempfile::tempdir().unwrap();
    let events = root.path().join("cgroup.events");
    assert!(scope_stopped(&events));
    std::fs::write(&events, "populated 1\n").unwrap();
    assert!(!scope_stopped(&events));
    std::fs::write(&events, "populated 0\n").unwrap();
    assert!(scope_stopped(&events));
    assert!(std::panic::catch_unwind(|| scope_stopped(root.path())).is_err());
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "task scope condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_spawn_has_named_membership() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_memory_uses_the_task_scope() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_memory_denied_setup_keeps_payload_gate_closed() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_kill_reaches_setsid_descendant() {
    require_manager();
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
    until(|| scope_stopped(&scope.cgroup().join("cgroup.events")));
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
    let root = tempfile::tempdir().unwrap();
    let id = random_slug();
    let control = root.path().join("cgroup.kill");
    std::fs::create_dir(&control).unwrap();
    let r = BgTaskRegistry::default();
    r.inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            root.path().into(),
        ));
    assert!(r.kill_task_scope(&id).unwrap_err().contains(&format!(
        "task scope aft-task-{id}.scope could not be killed"
    )));
    std::fs::remove_dir(&control).unwrap();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_replay_adopts_and_keeps_output() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_replay_identity_refusal_does_not_block_session() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_kill_without_handle_uses_exact_identity() {
    require_manager();
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
    until(|| scope_stopped(&scope.cgroup().join("cgroup.events")));
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_kill_without_handle_does_not_need_pid_identity() {
    require_manager();
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
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    second.kill_task_scope(&id).unwrap();
    assert!(second.inner.task_scopes.get(&id).is_none());
    until(|| scope_stopped(&scope.cgroup().join("cgroup.events")));
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_kill_distinguishes_absent_present_and_failed() {
    require_manager();
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
    other.kill_task_scope(&id).unwrap();
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
    assert_eq!(
        error,
        format!("task scope aft-task-{id}.scope could not be killed")
    );
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_uncached_kill_does_not_need_user_bus() {
    const ID: &str = "AFT_TEST_LOCAL_SCOPE_KILL_ID";
    if let Ok(id) = std::env::var(ID) {
        BgTaskRegistry::default()
            .kill_task_scope(&id)
            .expect("manager outage prevented local cgroup kill");
        return;
    }
    require_manager();
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
    let scope = super::super::task_scope::lookup(&id).unwrap().unwrap();
    for target in [random_slug(), id] {
        let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "bash_background::registry::task_scope_tests::task_scope_uncached_kill_does_not_need_user_bus", "--nocapture", "--include-ignored"]).env(ID, target).env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/aft-probe-bus").env("XDG_RUNTIME_DIR", "/nonexistent/aft-probe-runtime").output().unwrap();
        assert!(
            output.status.success(),
            "uncached kill required manager: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    until(|| scope_stopped(&scope.cgroup().join("cgroup.events")));
}

#[test]
fn task_scope_kill_file_write_failure_is_static() {
    let fake = tempfile::tempdir().unwrap();
    std::fs::create_dir(fake.path().join("cgroup.kill")).unwrap();
    let registry = BgTaskRegistry::default();
    let id = random_slug();
    registry
        .inner
        .task_scopes
        .register(super::super::task_scope::handle_for_test(
            &id,
            fake.path().into(),
        ));
    assert_eq!(
        registry.kill_task_scope(&id).unwrap_err(),
        format!("task scope aft-task-{id}.scope could not be killed")
    );
}

#[test]
#[ignore = "requires a systemd user manager"]
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
        let task = registry.task(&id).unwrap();
        assert!(
            registry.memory_limit_exceeded(&task).is_none(),
            "manager outage became a capped-task memory breach"
        );
        registry.poll_task(&task).unwrap();
        assert_eq!(
            registry
                .status(&id, "task-scope", None, None, 1024)
                .unwrap()
                .info
                .status,
            expected,
            "manager outage changed live task after poll"
        );
        if let Some(policy) = task.lock_state().unwrap().metadata.memory_policy.clone() {
            assert!(
                super::super::memory_limit::Monitor::recover(policy, &random_slug())
                    .unwrap()
                    .is_none(),
                "missing local scope did not return no monitor"
            );
            if expected == super::super::BgTaskStatus::Running {
                assert!(
                    registry
                        .inner
                        .memory_monitors
                        .lock()
                        .unwrap()
                        .get(&id)
                        .is_some_and(|monitor| monitor.is_task_scope()),
                    "capped task lost its local scope monitor"
                );
            }
        }
        registry.detach();
        return;
    }
    require_manager();
    for cap in [None, Some(64)] {
        let root = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let id = scope_spawn(
            &registry,
            root.path(),
            "sleep 60",
            super::super::HardKill::Default,
            cap,
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
            child.args(["--exact", "bash_background::registry::task_scope_tests::task_scope_replay_keeps_live_identity_without_user_manager", "--nocapture", "--include-ignored"]).env(ROOT, root.path());
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
}

#[test]
fn task_scope_monitor_recovery_uses_local_files_without_manager() {
    let root = tempfile::tempdir().unwrap();
    let manager_root = root
        .path()
        .join(format!("user@{}.service", unsafe { libc::geteuid() }));
    let current = manager_root.join("app.slice/module.service");
    let id = random_slug();
    let cgroup = manager_root
        .join("aft.slice")
        .join(format!("aft-task-{id}.scope"));
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(cgroup.join("memory.events"), "oom 0\noom_kill 0\n").unwrap();
    std::fs::write(cgroup.join("memory.oom.group"), "0").unwrap();
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: super::super::memory_limit::MemoryPolicy::TASK_SCOPE_MEMORY_KIND.into(),
        cgroup: None,
    };
    super::super::task_scope::with_local_recovery_fixture_for_test(current, || {
        let mut monitor = super::super::memory_limit::Monitor::recover(policy.clone(), &id)
            .unwrap()
            .expect("local scope was not recovered");
        assert!(monitor.is_task_scope());
        assert_eq!(monitor.policy.cgroup.as_ref(), Some(&cgroup));
        assert!(!monitor.exceeded(0));
        assert!(
            super::super::memory_limit::Monitor::recover(policy, &random_slug())
                .unwrap()
                .is_none()
        );
    });
    assert_eq!(
        std::fs::read_to_string(cgroup.join("memory.oom.group")).unwrap(),
        "0"
    );
}

#[test]
fn task_scope_monitor_recovery_error_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let source = BgTaskRegistry::default();
    let (_, task) = tests::insert_terminal_piped_task(&source, &root, "true", "", "", false);
    let mut metadata = task.lock_state().unwrap().metadata.clone();
    metadata.task_id = "invalid".into();
    metadata.status = super::super::BgTaskStatus::Running;
    metadata.pgid = Some(i32::MAX);
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: super::super::memory_limit::MemoryPolicy::TASK_SCOPE_MEMORY_KIND.into(),
        cgroup: None,
    };
    assert!(
        super::super::memory_limit::Monitor::recover(policy.clone(), &metadata.task_id).is_err()
    );
    metadata.memory_policy = Some(policy);
    let registry = BgTaskRegistry::default();
    registry
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let task = registry
        .inner
        .tasks
        .lock()
        .unwrap()
        .get("invalid")
        .unwrap()
        .clone();
    assert_eq!(
        registry.memory_limit_exceeded(&task).as_deref(),
        Some("Command stopped: memory monitor recovery failed."),
        "monitor recovery error did not fail closed"
    );
    assert_eq!(
        task.lock_state().unwrap().pending_terminal_override,
        Some(super::super::BgTaskStatus::Failed)
    );
}

#[test]
fn task_scope_monitor_recovery_does_not_write_controls() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("memory.oom.group"), "0").unwrap();
    let scope =
        super::super::task_scope::handle_for_test("bash-0123456789abcdef", root.path().into());
    let monitor = super::super::memory_limit::Monitor::recover_task_scope(64, &scope);
    assert!(monitor.is_task_scope());
    assert_eq!(
        std::fs::read_to_string(root.path().join("memory.oom.group")).unwrap(),
        "0",
        "monitor recovery rewrote cgroup controls"
    );
    drop(monitor);
    assert!(root.path().exists());
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_scoped_task_without_marker_is_fate_unknown() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_cleanup_and_hook_require_readable_owned_records() {
    require_manager();
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
#[ignore = "requires a systemd user manager"]
fn task_scope_cleanup_requires_this_storage_description() {
    require_manager();
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let id = scope_spawn(
        &registry,
        root.path(),
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let original = super::super::persistence::resolve_task(root.path(), "task-scope", &id).unwrap();
    let mut metadata = read_task_at(&original).unwrap();
    metadata.mark_terminal(super::super::BgTaskStatus::Completed, Some(0), None);
    let forged =
        super::super::persistence::create_task_layout(other.path(), "task-scope", &id).unwrap();
    write_task_at(&forged, &metadata).unwrap();
    assert_eq!(
        registry.cleanup_terminal_task_scopes(other.path()).unwrap(),
        0,
        "cleanup stopped another root's scope"
    );
    assert_eq!(
        registry.end_all_task_scopes(other.path()).unwrap(),
        0,
        "end-all stopped another root's scope"
    );
    assert!(super::super::task_scope::lookup(&id).unwrap().is_some());
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_cleanup_keeps_harness_store_coverage() {
    require_manager();
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("pi");
    std::fs::create_dir(&store).unwrap();
    let registry = BgTaskRegistry::default();
    let id = scope_spawn(
        &registry,
        &store,
        "sleep 60",
        super::super::HardKill::Default,
        None,
    );
    let _stop = StopOnDrop(id.clone());
    let task = super::super::persistence::resolve_task(&store, "task-scope", &id).unwrap();
    let mut metadata = read_task_at(&task).unwrap();
    metadata.mark_terminal(super::super::BgTaskStatus::Completed, Some(0), None);
    write_task_at(&task, &metadata).unwrap();
    assert_eq!(
        registry.cleanup_terminal_task_scopes(root.path()).unwrap(),
        1,
        "parent scan skipped owned harness-store scope"
    );
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
fn task_scope_cleanup_retries_unconfirmed_owned_unit() {
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (id, _) = tests::insert_terminal_piped_task(&registry, &root, "true", "", "", false);
    let (other, _) = tests::insert_terminal_piped_task(&registry, &root, "true", "", "", false);
    let mut ids = [id, other];
    ids.sort();
    let [id, other] = ids;
    let unit = format!("aft-task-{id}.scope");
    let other_unit = format!("aft-task-{other}.scope");
    let description = super::super::task_scope::scope_description(&id, root.path());
    let other_description = super::super::task_scope::scope_description(&other, root.path());
    let foreign = "aft-task-bash-0000000000000000.scope";
    let manager = root.path().join("systemctl");
    let script = format!(
        r#"#!/bin/sh
for unit in "$@"; do :; done
case "$2" in
list-units)
  echo '{foreign}'
  for unit in '{unit}' '{other_unit}'; do
    if [ ! -e '{root}/'"$unit"'.stopped' ]; then echo "$unit"; fi
  done ;;
show)
  case "$unit" in
    '{foreign}') touch '{root}/foreign-queried'; exit 97 ;;
    '{unit}')
      if [ ! -e '{root}/looked-up' ]; then touch '{root}/looked-up'; exit 1; fi
      description='{description}' ;;
    '{other_unit}') description='{other_description}' ;;
  esac
  printf 'LoadState=loaded\nControlGroup=/aft.slice/%s\nDescription=%s\n' "$unit" "$description" ;;
stop) echo "$unit" >> '{root}/stopped'; touch '{root}/'"$unit"'.stopped' ;;
esac
"#,
        root = root.path().display()
    );
    crate::executable_cache::write_executable(&manager, script.as_bytes()).unwrap();
    registry.inner.task_scopes.set_manager_for_test(manager);
    registry.configure_task_scope_runtime(true, Duration::from_secs(86400));
    let key = canonicalized_path(root.path());
    registry.start_task_scope_cleanup(root.path());
    until(|| {
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|slot| !slot.running)
    });
    {
        let slots = task_scope_cleanup_slots().lock().unwrap();
        let slot = slots.get(&key).unwrap();
        assert!(!slot.done, "unconfirmed lookup marked root cleanup done");
        assert!(slot.retry_at.is_some());
    }
    assert!(
        !root.path().join("foreign-queried").exists(),
        "foreign unit blocked owned cleanup"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("stopped")).unwrap(),
        format!("{other_unit}\n"),
        "owned lookup failure blocked another owned unit"
    );
    assert!(
        !root.path().join(format!("{unit}.stopped")).exists(),
        "unconfirmed unit was stopped"
    );
    registry.start_task_scope_cleanup(root.path());
    assert_eq!(
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .runs,
        1
    );
    until(|| {
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .retry_at
            .is_some_and(|when| Instant::now() >= when)
    });
    registry.start_task_scope_cleanup(root.path());
    until(|| {
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .done
    });
    assert_eq!(
        std::fs::read_to_string(root.path().join("stopped")).unwrap(),
        format!("{other_unit}\n{unit}\n")
    );
    assert_eq!(
        task_scope_cleanup_slots()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .runs,
        2
    );
}

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_actor_created_after_daemon_attach_has_named_membership() {
    require_manager();
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
