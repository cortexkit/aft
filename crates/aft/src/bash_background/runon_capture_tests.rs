use super::*;
use crate::bash_background::registry::runon_capture::set_test_cap;
use crate::exec_remote::wire_tests::Daemon;

const COPY_WARNING: &str = "Copied while another command in this checkout was still running; its changes may be missing from the job.";
const EDIT: &str = "until test -e .release-editor; do sleep 0.02; done; printf 'edited before capture\n' > tracked.txt";

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
            crate::bash_background::registry::resolve_posix_shell(),
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
        Some(launch(daemon.connection.clone())),
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
    ctx.bash_background()
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            &format!("touch .editor-started; {EDIT}"),
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

fn start_log_capture(root: &Path) -> RemoteLaunchLogCapture {
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(root.canonicalize().unwrap(), capture.clone());
    capture
}

fn assert_warning(snapshot: &BgTaskSnapshot, expected: bool) {
    assert_eq!(
        snapshot.info.status,
        BgTaskStatus::Completed,
        "{snapshot:?}"
    );
    assert_eq!(
        snapshot
            .output_preview
            .lines()
            .filter(|line| *line == COPY_WARNING)
            .count(),
        usize::from(expected),
        "{}",
        snapshot.output_preview,
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
    assert_warning(&done, false);
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
    assert_warning(&done, false);
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
    refused.acceptance_gate.notify_one();
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
    assert_warning(&terminal(ctx.bash_background(), &job).await, false);
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
    let local = local_command(&ctx, &root, "session", Foreground, EDIT);
    let logs = start_log_capture(&root);
    let job = start_runon(&ctx, &daemon);
    let done = terminal(ctx.bash_background(), &job).await;
    assert!(
        done.output_preview.lines().any(|line| line == "fixture"),
        "{done:?}"
    );
    assert_warning(&done, true);
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
    let wait = "until test -e .release-editor; do sleep 0.02; done";
    let background = local_command(&ctx, &root, "session", Background { max: 8 }, wait);
    let promoted = local_command(&ctx, &root, "session", Foreground, wait);
    ctx.bash_background().promote(&promoted, "session").unwrap();
    let outside = local_command(&ctx, &other, "session", Foreground, wait);
    let other_session = local_command(&ctx, &root, "other-session", Foreground, wait);
    let logs = start_log_capture(&root);
    let job = start_runon(&ctx, &daemon);
    assert_warning(&terminal(ctx.bash_background(), &job).await, false);
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
    daemon.acceptance_gate.notify_one();
    let done = terminal(ctx.bash_background(), &job).await;
    assert_warning(&done, true);
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
        Some(launch(daemon.connection.clone())),
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
    daemon.acceptance_gate.notify_one();
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
