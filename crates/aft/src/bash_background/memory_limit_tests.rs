use super::*;

const ALLOCATOR: &str =
    "bash_background::registry::memory_limit_tests::memory_limit_allocator_child";

#[cfg(unix)]
pub(super) fn memory_request_context(
    project: &Path,
    storage: &Path,
    cap: Option<u64>,
) -> Arc<crate::context::AppContext> {
    let mut config = crate::config::Config::default();
    config.project_root = Some(project.into());
    config.storage_dir = Some(storage.into());
    config.bash.memory_limit_mib = cap;
    config.sandbox.profile.head = serde_json::from_str("\"off\"").unwrap();
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ))
}

#[cfg(unix)]
pub(super) fn spawn_memory_request(
    ctx: &crate::context::AppContext,
    request_id: &str,
    remote: Option<super::super::RemoteLaunch>,
) -> crate::protocol::Response {
    super::super::spawn(
        request_id,
        "session",
        "exit 0",
        super::super::BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        super::super::HardKill::After(Duration::from_secs(30)),
        ctx,
        false,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        remote,
    )
}

#[cfg(unix)]
#[test]
fn memory_limit_interleaved_requests_keep_admitted_caps_across_reload() {
    for new_cap in [None, Some(128)] {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = memory_request_context(project.path(), storage.path(), Some(64));
        let original = ctx.config();
        let a_ctx = ctx.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request_a = std::thread::spawn(move || {
            let _pin = a_ctx.pin_config_to(original);
            with_memory_prepare_gate_for_test(
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
                },
                || spawn_memory_request(&a_ctx, "memory-request-a", None),
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let mut reloaded = ctx.config().as_ref().clone();
        reloaded.bash.memory_limit_mib = new_cap;
        ctx.set_config(reloaded);
        let request_b = {
            let _pin = ctx.pin_config();
            spawn_memory_request(&ctx, "memory-request-b", None)
        };
        release_tx.send(()).unwrap();
        let request_a = request_a.join().unwrap();
        assert!(request_a.success, "{request_a:?}");
        assert!(request_b.success, "{request_b:?}");
        let cap_for = |response: &crate::protocol::Response| {
            let task = ctx
                .bash_background()
                .task_for_test(response.data["task_id"].as_str().unwrap())
                .unwrap();
            let state = task.lock_state().unwrap();
            state
                .metadata
                .memory_policy
                .as_ref()
                .map(|policy| policy.mib)
        };
        assert_eq!(
            cap_for(&request_a),
            Some(64),
            "request A lost its admitted cap"
        );
        assert_eq!(
            cap_for(&request_b),
            new_cap,
            "request B inherited another request's cap"
        );
    }
}

#[test]
fn memory_limit_allocator_child() {
    let Some(mib) = std::env::var("AFT_TEST_ALLOCATE_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    let allocation = vec![0x5a_u8; mib * 1024 * 1024];
    std::hint::black_box(&allocation);
    std::thread::park_timeout(Duration::from_secs(12));
    std::hint::black_box(&allocation);
}

fn allocate_past_cap(pty: bool, foreground: bool, aggregate: bool, require_kernel: bool) {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let executable = std::env::current_exe().unwrap();
    #[cfg(unix)]
    let command = format!("'{}' --exact {ALLOCATOR} --nocapture", executable.display());
    #[cfg(windows)]
    let command = format!(
        "& '{}' --exact {ALLOCATOR} --nocapture",
        executable.display()
    );
    #[cfg(unix)]
    let command = if aggregate {
        format!("{command} & {command} & wait")
    } else {
        command
    };
    #[cfg(windows)]
    assert!(!aggregate);
    let mut env = HashMap::from([(
        "AFT_TEST_ALLOCATE_MIB".into(),
        if aggregate { "40" } else { "192" }.into(),
    )]);
    for key in ["PATH", "SystemRoot", "WINDIR", "USERPROFILE", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    #[cfg(unix)]
    let (shell, shell_path) = (super::super::BashShell::Bash, PathBuf::from("/bin/sh"));
    #[cfg(windows)]
    let (shell, shell_path) = (
        super::super::BashShell::Powershell,
        PathBuf::from("pwsh.exe"),
    );
    let timeout = super::super::HardKill::After(Duration::from_secs(10));
    let id = if pty {
        registry
            .spawn_pty_with_shell(
                SpawnPlan::Unsandboxed,
                &command,
                shell,
                shell_path,
                "memory-test".into(),
                temp.path().into(),
                env,
                timeout,
                Some(64),
                temp.path().into(),
                4,
                true,
                false,
                None,
                24,
                80,
            )
            .unwrap()
    } else {
        registry
            .spawn_with_shell_in_slot_typed(
                SpawnPlan::Unsandboxed,
                &command,
                shell,
                shell_path,
                "memory-test".into(),
                temp.path().into(),
                env,
                timeout,
                Some(64),
                temp.path().into(),
                if foreground {
                    super::super::TaskSlot::Foreground
                } else {
                    super::super::TaskSlot::Background { max: 4 }
                },
                true,
                false,
                None,
            )
            .unwrap()
    };
    if require_kernel {
        let kernel = registry
            .task_for_test(&id)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .memory_policy
            .as_ref()
            .unwrap()
            .cgroup
            .is_some();
        if !kernel {
            let _ = registry.kill(&id, "memory-test");
            panic!("no delegated cgroup v2 memory controller or systemd user scope available");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = registry
            .status(&id, "memory-test", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.info.status, BgTaskStatus::Failed, "{snapshot:?}");
            let reason = snapshot.info.status_reason.unwrap_or_default();
            assert!(reason.contains("64 MiB memory cap"), "{reason}");
            assert!(!reason.contains("timeout"), "{reason}");
            let task = registry.task_for_test(&id).unwrap();
            let state = task.lock_state().unwrap();
            let policy = state.metadata.memory_policy.as_ref().unwrap();
            assert!(reason.contains(&policy.kind), "{reason}");
            println!("enforcement: {}; result: {reason}", policy.kind);
            drop(state);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let completions = registry.pending_completions_for_session("memory-test");
                if let Some(completion) = completions
                    .iter()
                    .find(|completion| completion.task_id == id)
                {
                    assert_eq!(completion.status, BgTaskStatus::Failed);
                    assert_eq!(completion.status_reason.as_deref(), Some(reason.as_str()));
                    if !pty {
                        assert!(
                            task.lock_state().unwrap().descendant_sampling_started,
                            "memory stop bypassed the shared survivor check"
                        );
                    }
                    break;
                }
                assert!(Instant::now() < deadline, "no memory-cap completion result");
                std::thread::sleep(Duration::from_millis(25));
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = registry.kill(&id, "memory-test");
            panic!("memory cap did not stop the command before its timeout");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn memory_limit_stops_background_command() {
    allocate_past_cap(false, false, false, false);
}

#[test]
fn memory_limit_stops_foreground_command() {
    allocate_past_cap(false, true, false, false);
}

#[test]
fn memory_limit_stops_pty_command() {
    allocate_past_cap(true, false, false, false);
}

#[cfg(unix)]
#[test]
fn memory_limit_counts_children_as_one_group() {
    allocate_past_cap(false, false, true, false);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a delegated cgroup v2 memory controller or a systemd user manager"]
fn memory_limit_linux_kernel_stops_group() {
    allocate_past_cap(false, false, true, true);
}

#[test]
fn memory_limit_report_is_not_a_timeout() {
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: "test enforcement".into(),
        cgroup: None,
    };
    assert_eq!(
        policy.exceeded_reason(),
        "Command stopped: exceeded the 64 MiB memory cap (test enforcement)."
    );
}

#[test]
fn memory_limit_quick_piped_success_keeps_exit_zero() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry
        .inner
        .watchdog_started
        .store(true, Ordering::SeqCst);
    let mut env = HashMap::new();
    for key in ["PATH", "SystemRoot", "WINDIR", "USERPROFILE", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    #[cfg(unix)]
    let (shell, shell_path) = (super::super::BashShell::Bash, PathBuf::from("/bin/sh"));
    #[cfg(windows)]
    let (shell, shell_path) = (
        super::super::BashShell::Powershell,
        PathBuf::from("pwsh.exe"),
    );
    #[cfg(unix)]
    let command = "exit 0";
    // An explicit exit in Invoke-Expression ends the wrapper before it records
    // the exit code. Use a normal zero-return command to isolate job accounting.
    #[cfg(windows)]
    let command = "[Console]::Write('ok')";
    let id = registry
        .spawn_with_shell(
            SpawnPlan::Unsandboxed,
            command,
            shell,
            shell_path,
            "quick-success".into(),
            temp.path().into(),
            env,
            super::super::HardKill::After(Duration::from_secs(5)),
            Some(512),
            temp.path().into(),
            4,
            true,
            false,
            None,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let snapshot = registry
            .status(&id, "quick-success", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Completed,
                "{snapshot:?}"
            );
            assert_eq!(snapshot.exit_code, Some(0), "{snapshot:?}");
            assert!(snapshot.info.status_reason.is_none(), "{snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful piped task did not complete"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn memory_limit_fast_successful_pty_keeps_exit_zero() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    // Observe the completed child before the first memory scan, without a timer race.
    registry
        .inner
        .watchdog_started
        .store(true, Ordering::SeqCst);
    let id = registry
        .spawn_pty_with_shell(
            SpawnPlan::Unsandboxed,
            "exit 0",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/sh"),
            "quick-success".into(),
            temp.path().into(),
            HashMap::new(),
            super::super::HardKill::After(Duration::from_secs(5)),
            Some(64),
            temp.path().into(),
            4,
            true,
            false,
            None,
            24,
            80,
        )
        .unwrap();
    let task = registry.task_for_test(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let exited = task.lock_state().is_ok_and(|state| {
            matches!(&state.runtime,
            TaskRuntime::Pty(Some(pty)) if pty.exit_observed.load(Ordering::SeqCst))
        });
        if exited && task.terminal_reader_done.load(Ordering::SeqCst) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful PTY child did not exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Model a reaped PTY whose process identity was cleared before completion is consumed.
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.child_pid = None;
        state.metadata.pgid = None;
    }
    loop {
        let snapshot = registry
            .status(&id, "quick-success", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Completed,
                "{snapshot:?}"
            );
            assert_eq!(snapshot.exit_code, Some(0), "{snapshot:?}");
            assert!(snapshot.info.status_reason.is_none(), "{snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful PTY task did not complete"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn memory_limit_terminal_accounting_failure_still_samples_and_notifies() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (id, task) = tests::insert_terminal_piped_task(&registry, &temp, "allocate", "", "", false);
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: "test enforcement".into(),
        cgroup: None,
    };
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.memory_policy = Some(policy.clone());
        state.metadata.completion_delivered = false;
    }
    let mut monitor = super::super::memory_limit::Monitor::recover(policy.clone(), &id).unwrap();
    monitor.force_exceeded_for_test();
    registry
        .inner
        .memory_monitors
        .lock()
        .unwrap()
        .insert(id, monitor);
    registry.inner.completions.lock().unwrap().clear();
    *task.terminal_notification_routed.lock().unwrap() = false;

    // An unwritable metadata destination must not suppress an observed terminal result.
    let original_metadata = std::fs::read(&task.paths.json).unwrap();
    std::fs::remove_file(&task.paths.json).unwrap();
    std::fs::create_dir(&task.paths.json).unwrap();
    let result = registry.post_terminal_transition(&task, false);
    assert!(
        result.is_ok(),
        "memory settlement blocked publication: {result:?}"
    );
    assert!(task.terminal_persist_dirty.load(Ordering::SeqCst));
    assert!(task.lock_state().unwrap().descendant_sampling_started);
    let completions = registry.pending_completions_for_session("session");
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].status, BgTaskStatus::Failed);
    assert_eq!(
        completions[0].status_reason.as_deref(),
        Some(policy.exceeded_reason().as_str())
    );

    std::fs::remove_dir(&task.paths.json).unwrap();
    std::fs::write(&task.paths.json, original_metadata).unwrap();
    registry.retry_terminal_persist(&task).unwrap();
    assert!(!task.terminal_persist_dirty.load(Ordering::SeqCst));
    let persisted = super::super::persistence::read_task(&task.paths.json).unwrap();
    assert_eq!(persisted.status, BgTaskStatus::Failed);
    assert_eq!(persisted.status_reason, Some(policy.exceeded_reason()));
}
