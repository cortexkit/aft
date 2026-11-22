use super::*;

#[tokio::test]
async fn optional_worker_broker_remote_local_fallback_without_service_runs() {
    remote_local_fallback_has_no_broker(crate::config::SandboxMode::Strict, false).await;
}

/// Relaxed macOS workers receive the broker on local launches; a remote launch
/// that falls back to local execution must still get none, even when a broker
/// service is available.
#[tokio::test]
async fn optional_worker_broker_relaxed_remote_local_fallback_with_service_has_no_broker() {
    let _state = crate::test_env::gh_shim_state_guard();
    remote_local_fallback_has_no_broker(crate::config::SandboxMode::Standard, true).await;
}

async fn remote_local_fallback_has_no_broker(
    mode: crate::config::SandboxMode,
    service_available: bool,
) {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon(Script::Refused, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let temp = project.path().join("000000000001");
    std::fs::create_dir(&temp).unwrap();
    std::fs::write(
        project.path().join("sandbox-launch"),
        "while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        project.path().join("sandbox-launch"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![project.path().to_owned()],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        temp.clone(),
    )
    .unwrap();
    profile.mode = mode;
    let launcher = std::fs::canonicalize("/bin/sh").unwrap();
    let plan = crate::sandbox_spawn::SpawnPlan::launcher_for_test(profile, launcher);
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.path().to_owned()),
            storage_dir: Some(storage.path().to_owned()),
            experimental_bash_background: true,
            ..Default::default()
        },
    );
    ctx.update_config(|config| config.sandbox.profile.worker = mode);
    let command="test -z \"${AFT_GH_READ_SOCKET+x}\" && test -z \"${AFT_GH_READ_CAPABILITY_FILE+x}\" && test -z \"${AFT_GH_SHIM_TICKET+x}\" && printf FALLBACK_WITHOUT_BROKER";
    let launch_once = || {
        crate::logging::capture_log_lines(|| {
            crate::sandbox_spawn::with_worker_jail(true, || {
                crate::sandbox_spawn::with_spawn_plan_for_test(plan, || {
                    crate::bash_background::spawn(
                        "optional-fallback",
                        "session",
                        command,
                        crate::bash_background::BashShell::Bash,
                        resolve_posix_shell(),
                        Some(project.path().to_owned()),
                        None,
                        crate::bash_background::HardKill::After(Duration::from_secs(30)),
                        &ctx,
                        true,
                        true,
                        false,
                        false,
                        24,
                        80,
                        vec![],
                        false,
                        None,
                        Some(launch(daemon.connection.clone())),
                    )
                })
            })
        })
    };
    let (response, logs) = if service_available {
        crate::gh_shim_worker_broker::with_available_service_for_test(project.path(), launch_once)
    } else {
        crate::gh_shim_worker_broker::without_service_for_test(launch_once)
    };
    assert!(response.success, "{response:?}");
    assert!(
        logs.iter().any(|line| line.starts_with("WARN ")
            && line.contains("gh_shim worker_read unavailable")
            && line.contains("reason=remote launch or local fallback has no host broker")),
        "{logs:?}"
    );
    let task = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task).await;
    assert_eq!(done.exit_code, Some(0), "{}", done.output_preview);
    assert!(
        done.output_preview.contains("FALLBACK_WITHOUT_BROKER"),
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview.contains("ran locally"),
        "{}",
        done.output_preview
    );
    assert!(!temp.join(".gh-read-capability").exists());
    assert!(!temp.with_file_name("000000000001.gh-read.sock").exists());
}
