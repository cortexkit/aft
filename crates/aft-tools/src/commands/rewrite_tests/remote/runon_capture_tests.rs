use super::*;
use aft_engine::test_bridge::bash_remote::{
    remote_wire::Daemon, set_test_cap, BgTaskRegistryTestExt as _,
};

const EDIT: &str = "until test -e .release-editor; do sleep 0.02; done; printf 'edited before capture\n' > tracked.txt";
const HOLD: &str = "until test -e .release-editor; do sleep 0.02; done";

fn local_command(
    ctx: &crate::context::AppContext,
    workdir: &Path,
    session: &str,
    slot: crate::bash_background::TaskSlot,
    command: &str,
) -> String {
    ctx.bash_background()
        .spawn_with_shell_in_slot(
            SpawnPlan::Unsandboxed,
            command,
            crate::bash_background::BashShell::Bash,
            aft_engine::test_bridge::bash_remote::resolve_posix_shell(),
            session.into(),
            workdir.into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(15)),
            None,
            crate::bash_background::task_storage_dir(ctx),
            slot,
            false,
            false,
            ctx.config().project_root.clone(),
        )
        .unwrap()
}

fn start_runon(ctx: &crate::context::AppContext, daemon: &Daemon) -> String {
    let response = handle_with_policy(
        ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command": "printf capture", "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    response.data["task_id"].as_str().unwrap().into()
}

fn start_remote_editor(
    ctx: &crate::context::AppContext,
    daemon: &Daemon,
    workdir: &Path,
) -> String {
    start_foreground_remote(
        ctx,
        launch(daemon.connection().clone()),
        workdir,
        &format!("touch .editor-started; {EDIT}"),
    )
}

fn start_foreground_remote(
    ctx: &crate::context::AppContext,
    launch: crate::bash_background::RemoteLaunch,
    workdir: &Path,
    command: &str,
) -> String {
    ctx.bash_background()
        .spawn_remote(
            launch,
            SpawnPlan::Unsandboxed,
            command,
            resolve_posix_shell(),
            "session".into(),
            workdir.into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(15)),
            None,
            crate::bash_background::task_storage_dir(ctx),
            crate::bash_background::TaskSlot::Foreground,
            false,
            false,
            ctx.config().project_root.clone(),
        )
        .unwrap()
}

fn start_log_capture(root: &Path) -> Arc<Mutex<Vec<String>>> {
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(root.canonicalize().unwrap(), capture.clone());
    capture
}

fn assert_warning(snapshot: &BgTaskSnapshot, expected: Option<&str>) {
    assert_eq!(
        snapshot.info.status,
        BgTaskStatus::Completed,
        "{snapshot:?}"
    );
    assert_eq!(
        snapshot
            .output_preview
            .lines()
            .filter(|line| line.starts_with("Copied while another command in this checkout"))
            .collect::<Vec<_>>(),
        expected.into_iter().collect::<Vec<_>>(),
        "{}",
        snapshot.output_preview,
    );
}

fn assert_saved_overlaps(registry: &BgTaskRegistry, job: &str, overlaps: &[(&str, &str)]) {
    let path = registry.task_json_path(job, "session").unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let remote = &saved["remote"];
    // The old reader knows only this boolean and ignores additional fields.
    #[derive(serde::Deserialize)]
    struct OldRemote {
        copy_warning: bool,
    }
    assert!(
        serde_json::from_value::<OldRemote>(remote.clone())
            .unwrap()
            .copy_warning
    );
    assert_eq!(
        remote["copy_warning_commands"],
        serde_json::json!(overlaps.iter().copied().collect::<BTreeMap<_, _>>()),
    );
}

#[tokio::test]
async fn runon_capture_waits_for_foreground_edit_in_a_checkout_subdirectory() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let subdir = dir.path().join("child");
    fs::create_dir(&subdir).unwrap();
    let local = local_command(
        &ctx,
        &subdir,
        "session",
        Foreground,
        &format!("cd ..; {EDIT}"),
    );
    let logs = start_log_capture(dir.path());
    let wait = set_test_cap(dir.path(), Duration::from_secs(10));
    let job = start_runon(&ctx, &daemon);
    wait.wait_started().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        exec_runs(&daemon).is_empty(),
        "runner received a stale checkout"
    );
    fs::write(dir.path().join(".release-editor"), "").unwrap();
    terminal(ctx.bash_background(), &local).await;
    let done = terminal(ctx.bash_background(), &job).await;
    assert!(
        done.output_preview.contains("edited before capture"),
        "{done:?}"
    );
    assert_warning(&done, None);
    let logs = logs.lock().unwrap();
    let wait = logs
        .iter()
        .find(|line| line.contains("runon: foreground capture wait "))
        .unwrap();
    assert!(
        wait.contains(&job) && wait.contains(&local) && wait.contains("cap_hit=false"),
        "{wait}"
    );
    let ms: u64 = wait
        .split_once("waited_ms=")
        .unwrap()
        .1
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..3000).contains(&ms), "{wait}");
}

#[tokio::test]
async fn runon_capture_waits_for_foreground_local_fallback() {
    let refused = daemon(Script::KnownRefused, "exec-remote/v1").await;
    let capture = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let local = start_remote_editor(&ctx, &refused, dir.path());
    refused.wait_for_requests("exec.run", 1).await;
    wait_for("the fallback editor to start", || {
        dir.path().join(".editor-started").exists().then_some(())
    })
    .await;

    let logs = start_log_capture(dir.path());
    let wait = set_test_cap(dir.path(), Duration::from_secs(10));
    let job = start_runon(&ctx, &capture);
    tokio::select! {
        _ = wait.wait_started() => {},
        _ = capture.wait_for_requests("exec.run", 1) => panic!("runner skipped the foreground fallback"),
    }
    fs::write(dir.path().join(".release-editor"), "").unwrap();
    let finished = terminal(ctx.bash_background(), &local).await;
    assert_eq!(finished.exit_code, Some(0), "{finished:?}");
    let done = terminal(ctx.bash_background(), &job).await;
    assert!(
        done.output_preview.contains("edited before capture"),
        "{done:?}"
    );
    assert_warning(&done, None);
    assert!(logs.lock().unwrap().iter().any(|line| {
        line.contains("runon: foreground capture wait ")
            && line.contains(&job)
            && line.contains(&local)
            && line.contains("cap_hit=false")
    }));
}

#[tokio::test]
async fn runon_capture_does_not_wait_or_warn_after_fallback_spawn_failure() {
    let refused = daemon(
        Script::GatedPreStartRefusal {
            reason: "unreachable",
            detail: None,
        },
        "exec-remote/v1",
    )
    .await;
    let capture = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ctx = remote_test_context(&root);
    let workdir = root.join("editor");
    fs::create_dir(&workdir).unwrap();
    let local = start_remote_editor(&ctx, &refused, &workdir);
    refused.wait_for_requests("exec.run", 1).await;
    // Leave the checkout intact, but make the local spawn fail in its workdir.
    fs::remove_dir(workdir).unwrap();
    refused.acceptance_gate().notify_one();
    let failed = terminal(ctx.bash_background(), &local).await;
    assert_eq!(failed.child_pid, None, "{failed:?}");
    assert!(
        failed
            .output_preview
            .contains("failed to spawn background bash command"),
        "{failed:?}"
    );

    let logs = start_log_capture(&root);
    let _cap = set_test_cap(&root, Duration::from_millis(100));
    let job = start_runon(&ctx, &capture);
    assert_warning(&terminal(ctx.bash_background(), &job).await, None);
    assert!(!logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("foreground capture wait")));
}

#[tokio::test]
async fn runon_capture_submits_after_cap_and_warns_once() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let root = dir.path().canonicalize().unwrap();
    let _cap = set_test_cap(&root, Duration::from_millis(100));
    let local = local_command(&ctx, &root, "session", Foreground, HOLD);
    let logs = start_log_capture(&root);
    let job = start_runon(&ctx, &daemon);
    let done = terminal(ctx.bash_background(), &job).await;
    assert!(
        done.output_preview.lines().any(|line| line == "fixture"),
        "{done:?}"
    );
    assert_warning(&done, Some("Copied while another command in this checkout was still running (`until test -e .release-editor; do sleep 0.02; done`); its changes may be missing from the job."));
    assert_saved_overlaps(ctx.bash_background(), &job, &[(&local, HOLD)]);
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains(&local) && line.contains("cap_hit=true")));
    fs::write(root.join(".release-editor"), "").unwrap();
    terminal(ctx.bash_background(), &local).await;
}

#[tokio::test]
async fn runon_capture_ignores_background_promoted_other_checkout_and_other_session() {
    use crate::bash_background::TaskSlot::{Background, Foreground};
    let daemon = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("copy");
    let other = dir.path().join("copy-other");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&other).unwrap();
    let ctx = remote_test_context(&root);
    let root = root.canonicalize().unwrap();
    let _cap = set_test_cap(&root, Duration::from_millis(100));
    let background = local_command(&ctx, &root, "session", Background { max: 8 }, HOLD);
    let promoted = local_command(&ctx, &root, "session", Foreground, HOLD);
    ctx.bash_background().promote(&promoted, "session").unwrap();
    let outside = local_command(&ctx, &other, "session", Foreground, HOLD);
    let other_session = local_command(&ctx, &root, "other-session", Foreground, HOLD);
    let logs = start_log_capture(&root);
    let job = start_runon(&ctx, &daemon);
    assert_warning(&terminal(ctx.bash_background(), &job).await, None);
    assert!(!logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("foreground capture wait")));
    for cwd in [&root, &other] {
        fs::write(cwd.join(".release-editor"), "").unwrap();
    }
    for id in [background, promoted, outside] {
        terminal(ctx.bash_background(), &id).await;
    }
    wait_for("other session editor to finish", || {
        ctx.bash_background()
            .observed_status(&other_session, "other-session", 0)
            .filter(|snapshot| snapshot.info.status.is_terminal())
    })
    .await;
}

#[tokio::test]
async fn runon_capture_warns_if_foreground_command_starts_and_finishes_during_copy() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let job = start_runon(&ctx, &daemon);
    daemon.wait_for_requests("exec.run", 1).await;
    let local = local_command(
        &ctx,
        dir.path(),
        "session",
        Foreground,
        "printf 'edited during copy\n' > tracked.txt",
    );
    terminal(ctx.bash_background(), &local).await;
    daemon.acceptance_gate().notify_one();
    let done = terminal(ctx.bash_background(), &job).await;
    assert_warning(&done, Some("Copied while another command in this checkout was still running (`printf 'edited during copy`); its changes may be missing from the job."));
    assert_saved_overlaps(
        ctx.bash_background(),
        &job,
        &[(&local, "printf 'edited during copy")],
    );
}

#[tokio::test]
async fn runon_capture_warns_with_one_name_and_counts_other_overlapping_tasks() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let root = dir.path().canonicalize().unwrap();
    let _cap = set_test_cap(&root, Duration::from_millis(100));
    let first = local_command(&ctx, &root, "session", Foreground, HOLD);
    let job = start_runon(&ctx, &daemon);
    daemon.wait_for_requests("exec.run", 1).await;
    let second = local_command(&ctx, &root, "session", Foreground, "printf second");
    terminal(ctx.bash_background(), &second).await;
    daemon.acceptance_gate().notify_one();

    let shown = if first < second {
        HOLD
    } else {
        "printf second"
    };
    let done = terminal(ctx.bash_background(), &job).await;
    assert_warning(&done, Some(&format!("Copied while another command in this checkout was still running (`{shown}` and 1 more); its changes may be missing from the job.")));
    assert_saved_overlaps(
        ctx.bash_background(),
        &job,
        &[(&first, HOLD), (&second, "printf second")],
    );
    fs::write(root.join(".release-editor"), "").unwrap();
    terminal(ctx.bash_background(), &first).await;
}

#[tokio::test]
async fn runon_capture_shortens_a_long_command_on_a_character_boundary() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let job = start_runon(&ctx, &daemon);
    daemon.wait_for_requests("exec.run", 1).await;
    let command = format!("printf '%s' '{}'\nprintf second-line", "é".repeat(100));
    let local = local_command(&ctx, dir.path(), "session", Foreground, &command);
    terminal(ctx.bash_background(), &local).await;
    daemon.acceptance_gate().notify_one();

    let done = terminal(ctx.bash_background(), &job).await;
    let shown = format!("printf '%s' '{}…", "é".repeat(67));
    assert_warning(&done, Some(&format!("Copied while another command in this checkout was still running (`{shown}`); its changes may be missing from the job.")));
    assert_saved_overlaps(ctx.bash_background(), &job, &[(&local, &shown)]);
}

#[test]
fn runon_capture_renders_the_old_warning_from_a_record_without_command_names() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _task, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused-connection"),
        "printf capture",
    );
    let mut old: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.json).unwrap()).unwrap();
    let remote = old["remote"].as_object_mut().unwrap();
    assert!(!remote.contains_key("copy_warning_commands"));
    assert_eq!(remote.insert("copy_warning".into(), true.into()), None);
    let metadata: PersistedTask = serde_json::from_value(old).unwrap();
    let note = completion_preview::rendered_execution_note(&metadata).unwrap();
    assert_eq!(note.lines().last(), Some("Copied while another command in this checkout was still running; its changes may be missing from the job."));
}

#[tokio::test]
async fn runon_capture_does_not_warn_about_another_runon_in_the_same_checkout() {
    let first = daemon(Script::CaptureCheckout { gated: true }, "exec-remote/v1").await;
    let second = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let _cap = set_test_cap(dir.path(), Duration::from_millis(100));
    let remote = start_foreground_remote(
        &ctx,
        crate::bash_background::RemoteLaunch {
            explicit_runon: true,
            ..launch(first.connection().clone())
        },
        dir.path(),
        "printf first remote",
    );
    first.wait_for_requests("exec.run", 1).await;
    let job = start_runon(&ctx, &second);
    assert_warning(&terminal(ctx.bash_background(), &job).await, None);
    first.acceptance_gate().notify_one();
    assert_warning(&terminal(ctx.bash_background(), &remote).await, None);
}

#[tokio::test]
async fn runon_capture_cancels_a_wait_without_sending_exec_run() {
    use crate::bash_background::TaskSlot::Foreground;
    let daemon = daemon(Script::CaptureCheckout { gated: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let local = local_command(&ctx, dir.path(), "session", Foreground, EDIT);
    let wait = set_test_cap(dir.path(), Duration::from_secs(10));
    let job = start_runon(&ctx, &daemon);
    wait.wait_started().await;
    let registry = ctx.bash_background().clone();
    let done = tokio::task::spawn_blocking(move || registry.kill(&job, "session"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed, "{done:?}");
    assert!(exec_runs(&daemon).is_empty());
    fs::write(dir.path().join(".release-editor"), "").unwrap();
    terminal(ctx.bash_background(), &local).await;
}

#[tokio::test]
async fn runon_launch_logs_send_phases_and_acceptance_capture_time() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(dir.path().canonicalize().unwrap(), capture.clone());
    let command = "printf runon-diagnostic-command";
    let secret = "runon-diagnostic-secret-value";
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": command,
            "runon": "linux",
            "env": {"DIAGNOSTIC_TOKEN": secret},
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    wait_for(
        "exec.run sent must be logged before the executor accepts",
        || {
            capture
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("runon: exec.run sent "))
                .then_some(())
        },
    )
    .await;
    assert!(!capture
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("runon: remote task accepted ")));
    daemon.acceptance_gate().notify_one();
    wait_for("executor acceptance must be logged", || {
        capture
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("runon: remote task accepted "))
            .then_some(())
    })
    .await;
    let registry = ctx.bash_background().clone();
    let kill_id = task_id.to_string();
    tokio::task::spawn_blocking(move || registry.kill(&kill_id, "session"))
        .await
        .unwrap()
        .unwrap();
    terminal(ctx.bash_background(), task_id).await;
    remote_worker_exited(task_id).await;
    let lines = capture.lock().unwrap().clone();
    let sent: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("runon: exec.run sent "))
        .collect();
    assert_eq!(sent.len(), 1, "{lines:?}");
    let (sent_index, sent) = sent[0];
    assert!(sent.starts_with("INFO "), "{sent}");
    assert!(
        sent.contains(&format!("task_id={task_id} session=session ")),
        "{sent}"
    );
    let (_, timings) = sent.split_once("pre_send_ms=").unwrap();
    let (total, phases) = timings.split_once(" phases=").unwrap();
    let total: u128 = total.parse().unwrap();
    let phases: Vec<_> = phases
        .split(',')
        .map(|phase| {
            let (name, millis) = phase.split_once(':').unwrap();
            (name, millis.parse::<u128>().unwrap())
        })
        .collect();
    for required in [
        "secret_preflight",
        "workdir_return_paths",
        "permission_preflight",
        "sandbox_preflight",
        "changed_files_baseline",
        "workdir_repository_git",
        "cd_snapshot_git",
        "worker_dispatch",
        "connect_route",
        "supports_submission_key",
        "submission_persistence",
        "send",
    ] {
        assert!(
            phases.iter().any(|(name, _)| *name == required),
            "missing {required}: {sent}"
        );
    }
    let phase_total: u128 = phases.iter().map(|(_, millis)| millis).sum();
    assert!(
        phase_total <= total && total - phase_total < phases.len() as u128,
        "{sent}"
    );
    let (accepted_index, accepted) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains("runon: remote task accepted "))
        .unwrap();
    assert!(sent_index < accepted_index, "{lines:?}");
    assert!(
        accepted.contains(&format!(
            "task_id={task_id} session=session job_id={}",
            id()
        )),
        "{accepted}"
    );
    accepted
        .split_once("capture_ms=")
        .unwrap()
        .1
        .parse::<u128>()
        .unwrap();
    for line in [sent, accepted] {
        assert!(!line.contains(command) && !line.contains(secret), "{line}");
    }
}
