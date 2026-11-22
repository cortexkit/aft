#[tokio::test]
async fn exec_remote_bash_refusal_runs_local_with_disclosure() {
    for (script, reason) in [
        (Script::Refused, "future_refusal"),
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let task = start(&registry, dir.path(), daemon.connection.clone());
        let done = terminal(&registry, &task).await;
        assert_eq!(done.exit_code, Some(0));
        assert!(
            done.output_preview.contains("local-proof"),
            "{}",
            done.output_preview
        );
        assert_eq!(
            done.output_preview.lines().next(),
            Some(
                format!(
                    "ran locally on {}: remote refused: {reason}",
                    local_os_name()
                )
                .as_str()
            ),
            "{}",
            done.output_preview
        );
        let log = daemon.log.lock().unwrap();
        let request = log.iter().find(|(_, b)| b["method"] == "exec.run").unwrap();
        assert_eq!(
            request.1["params"]["env"]["BUILD_REMOTE_ENV_TEST"],
            "environment-proof"
        );
        assert_eq!(request.1["params"]["cwd"], dir.path().display().to_string());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bwrap_composition_remote_refusal_keeps_local_sandbox() {
    use crate::bash_background::{HardKill, TaskSlot};
    use crate::sandbox_spawn::bwrap_composition_fixture::Fixture;
    let dir = tempfile::tempdir().unwrap();
    let Some(fixture) = Fixture::new(dir.path()) else {
        return;
    };
    ensure_git_fixture(&fixture.root);
    let daemon = daemon(Script::KnownRefused, "exec-remote/v1").await;
    let registry = registry();
    let task = registry
        .spawn_remote(
            launch(daemon.connection.clone()),
            fixture.plan(),
            &format!("{} printf local-proof", fixture.oracle()),
            resolve_posix_shell(),
            "session".into(),
            fixture.root.clone(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(30)),
            None,
            fixture.root.clone(),
            TaskSlot::Background { max: 10 },
            true,
            false,
            Some(fixture.root.clone()),
        )
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0), "{}", done.output_preview);
    assert!(
        done.output_preview.contains("ran locally on"),
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview.contains("remote refused: unreachable"),
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview.contains("local-proof"),
        "{}",
        done.output_preview
    );
    Fixture::assert_report(&done.output_preview);
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, req)| req["method"] == "exec.run"));
}
