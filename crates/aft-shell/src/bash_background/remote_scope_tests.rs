use super::*;

use super::super::fixture::persist_accepted_with_snapshot_scope;
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a systemd user manager"]
async fn task_scope_remote_fallback_deadline_survives_reload() {
    super::super::super::super::task_scope::probe_user_manager(Duration::from_secs(1))
        .expect("requires a systemd user manager");
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot_scope(
        dir.path(),
        dir.path().join("unused-connection"),
        "sleep 60",
        None,
        true,
        60,
    );
    original.configure_task_scope_runtime(true, Duration::from_secs(60));
    struct Stop(BgTaskRegistry, String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill_task_scope(&self.1);
        }
    }
    let _stop = Stop(original.clone(), task_id.clone());
    let task = original.task(&task_id).unwrap();
    let remote = {
        let mut state = task.state.lock().unwrap();
        state.metadata.default_hard_kill = true;
        state.metadata.timeout_ms = Some(DEFAULT_BG_TIMEOUT.as_millis() as u64);
        write_task(&paths.json, &state.metadata).unwrap();
        state.metadata.remote.clone().unwrap()
    };
    let (_, logs) = crate::logging::capture_log_lines(|| {
        original
            .restored_remote_fallback(&task, &remote, "capacity", None)
            .unwrap();
    });
    assert!(
        original.inner.task_scopes.get(&task_id).is_some(),
        "fallback did not enter a scope: {:?}; {logs:?}",
        task.state.lock().unwrap().metadata.execution_note
    );
    let saved = read_task(&paths.json).unwrap();
    assert!(saved.local_fallback_started);
    assert!(saved.remote.is_none());
    let recovered = registry();
    recovered.inner.shutdown.store(true, Ordering::SeqCst);
    recovered
        .insert_rehydrated_task(saved, paths, true)
        .unwrap();
    assert_eq!(
        recovered
            .hard_kill_deadline(&task_id, "session")
            .unwrap()
            .limit_ms,
        60000,
        "reloaded local fallback lost the earlier scope deadline"
    );
    original.kill(&task_id, "session").unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn task_scope_remote_failed_unscoped_retry_clears_saved_deadline() {
    for failure in ["unscoped retry", "initial spawn", "scope placement"] {
        let root = tempfile::tempdir().unwrap();
        let (registry, id, paths) = persist_accepted_with_snapshot_scope(
            root.path(),
            root.path().join("unused-connection"),
            "true",
            None,
            true,
            60,
        );
        let task = registry.task(&id).unwrap();
        let digest = task
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .fallback_digest
            .clone()
            .unwrap();
        let layout = resolve_task_layout(&paths.session_dir, &id).unwrap();
        let (plan, shell_path, env, linux_scope, memory_limit_mib, linux_scope_max_runtime_secs) =
            crate::sandbox_spawn::restore_local_launch(&layout, &digest).unwrap();
        let workdir = root.path().join("workdir");
        if failure != "initial spawn" {
            fs::create_dir(&workdir).unwrap();
        }
        {
            let mut state = task.state.lock().unwrap();
            state.metadata.workdir = workdir;
            write_task(&paths.json, &state.metadata).unwrap();
        }
        let launcher = root.path().join("systemd-run");
        let script = format!(
            r#"#!/bin/bash
if [[ $1 == --version ]]; then echo 'systemd 254'; exit 0; fi
while [[ $1 == --* ]]; do shift; done
if [[ $1 == /bin/true ]]; then exec "$@"; fi
echo launch >> "${{0%/*}}/calls"
if [[ '{failure}' == 'scope placement' ]]; then exec sleep 60; fi
rmdir "${{0%/*}}/workdir"
exit 97
"#,
        );
        crate::executable_cache::write_executable(&launcher, script.as_bytes()).unwrap();
        registry.inner.task_scopes.set_launcher_for_test(launcher);
        registry.configure_task_scope_runtime(true, Duration::from_secs(60));
        let launch = || {
            registry.remote_fallback(
                &task,
                LocalFallback {
                    plan,
                    shell_path,
                    env,
                    capture_pipeline: false,
                    memory_limit_mib,
                    linux_scope,
                    linux_scope_max_runtime_secs,
                },
                "capacity",
                None,
            )
        };
        let result = if failure == "scope placement" {
            let metadata_path = paths.json.clone();
            let observed = Arc::new(AtomicBool::new(false));
            let observed_in_probe = Arc::clone(&observed);
            let result = super::super::super::super::task_scope::with_placement_reader_for_test(
                move |_, _| {
                    assert_eq!(
                        read_task(&metadata_path).unwrap().task_scope_runtime_ms,
                        Some(90_000),
                        "scope runtime must be durable before placement"
                    );
                    observed_in_probe.store(true, Ordering::SeqCst);
                    Err("forced scope placement error".into())
                },
                launch,
            );
            assert!(observed.load(Ordering::SeqCst));
            assert!(result
                .as_ref()
                .unwrap_err()
                .contains("forced scope placement error"));
            result
        } else {
            launch()
        };
        assert!(
            result.is_err(),
            "{failure}: failed fallback unexpectedly spawned"
        );
        if failure == "unscoped retry" {
            assert_eq!(
                fs::read_to_string(root.path().join("calls")).unwrap(),
                "launch\n"
            );
        } else if failure == "initial spawn" {
            assert!(!root.path().join("calls").exists());
        }
        let saved = read_task(&paths.json).unwrap();
        assert!(saved.local_fallback_started);
        assert_eq!(
            saved.task_scope_runtime_ms, None,
            "{failure}: failed fallback retained a scope deadline"
        );
        assert_eq!(
            task.state.lock().unwrap().metadata.task_scope_runtime_ms,
            None
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn task_scope_standalone_fallback_never_starts_systemd_run() {
    const ROOT: &str = "AFT_TEST_STANDALONE_FALLBACK_SCOPE_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        for restarted in [false, true] {
            let daemon = daemon(
                if restarted {
                    Script::AttachRefused
                } else {
                    Script::Refused
                },
                "exec-remote/v1",
            )
            .await;
            let dir = root.join(if restarted { "restored" } else { "fresh" });
            fs::create_dir(&dir).unwrap();
            let counter = dir.join("local-count");
            let command = format!("printf x >> '{}'", counter.display());
            let (original, task_id, paths) = persist_accepted_with_snapshot_scope(
                &dir,
                daemon.connection.clone(),
                &command,
                None,
                true,
                7200,
            );
            original.configure_task_scope_runtime(false, Duration::from_secs(86400));
            original
                .inner
                .task_scopes
                .set_launcher_for_test(root.join("systemd-run"));
            if restarted {
                drop(original);
                let ctx = restarted_context(&dir);
                ctx.bash_background()
                    .inner
                    .task_scopes
                    .set_launcher_for_test(root.join("systemd-run"));
                assert!(!ctx.is_daemon_mode());
                ctx.bash_background()
                    .replay_session(&dir, "session")
                    .unwrap();
                let done = terminal(ctx.bash_background(), &task_id).await;
                assert_eq!(done.exit_code, Some(0));
            } else {
                let task = original.task(&task_id).unwrap();
                let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
                let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
                let (
                    plan,
                    shell_path,
                    env,
                    linux_scope,
                    memory_limit_mib,
                    linux_scope_max_runtime_secs,
                ) = crate::sandbox_spawn::restore_local_launch(
                    &layout,
                    remote.fallback_digest.as_deref().unwrap(),
                )
                .unwrap();
                assert!(linux_scope);
                assert_eq!(linux_scope_max_runtime_secs, 7200);
                let request = exec::build_request(
                    &dir,
                    &dir,
                    &dir,
                    &command,
                    BTreeMap::new(),
                    Some(30),
                    &exec::PresetParams::default(),
                );
                original
                    .start_remote_worker(
                        task,
                        Some((
                            request.map_err(RemoteRequestError::Build),
                            LocalFallback {
                                plan,
                                shell_path,
                                env,
                                linux_scope,
                                memory_limit_mib,
                                linux_scope_max_runtime_secs,
                                capture_pipeline: false,
                            },
                        )),
                        None,
                    )
                    .unwrap();
                original.start_watchdog();
                let done = terminal(&original, &task_id).await;
                assert_eq!(done.exit_code, Some(0));
            }
            assert_eq!(fs::read_to_string(counter).unwrap(), "x");
        }
        fs::write(root.join("inner-ran"), "ran").unwrap();
        assert!(!root.join("systemd-calls").exists());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    crate::executable_cache::write_executable(
        &root.path().join("systemd-run"),
        b"#!/bin/sh\nprintf called >> \"${0%/*}/systemd-calls\"\nexit 97\n",
    )
    .unwrap();
    let path = std::env::join_paths(std::iter::once(root.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "bash_background::registry::remote::tests::scope_tests::task_scope_standalone_fallback_never_starts_systemd_run", "--nocapture"]).env(ROOT, root.path()).env("PATH", path).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("inner-ran")).unwrap(),
        "ran"
    );
    assert!(!root.path().join("systemd-calls").exists());
}
