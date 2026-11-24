#![cfg(test)]

use crate::bash_background::persistence::{
    read_task, resolve_task_layout, session_tasks_dir, unix_millis, write_task, write_task_at,
    TaskPaths,
};
use crate::bash_background::{registry::remote::RemotePhase, BgTaskStatus};
use crate::exec_remote::types::*;
use crate::sandbox_spawn::SpawnPlan;
use crate::tools_test_support::{handle_with_policy, rendered_status};
use aft_engine::test_bridge::bash_remote::bash_background::registry::remote::*;
use aft_engine::test_bridge::bash_remote::remote_support::*;
use aft_engine::test_bridge::bash_remote::remote_wire::{daemon, id, Script};
use aft_engine::test_bridge::bash_remote::{
    resolve_posix_shell, BgTaskRegistryTestExt as _, BgTaskStateTestExt as _,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn every_local_spawn_entrypoint_executes_command_once() {
    use crate::bash_background::{BashShell, HardKill, TaskSlot};
    for entry in [
        "foreground",
        "background",
        "pty",
        "deferred-wait",
        "replay-running",
        "remote-fallback",
        "replay-fallback",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("executions");
        let release = dir.path().join("release");
        let _release_on_failure = ReleaseOnDrop::new(release.clone());
        let command = format!(
            "printf x >> '{}'; {}; printf entrypoint-proof",
            counter.display(),
            crate::test_gate::gate(&format!("'{}'", release.display()), "/bin/sleep 0.01")
        );
        if entry == "deferred-wait" {
            crate::subc::test_support::assert_deferred_wait_entrypoint(&command, &release).await;
            assert_eq!(
                fs::read(&counter).unwrap(),
                b"x",
                "deferred wait must run its command once"
            );
            eprintln!(
                "spawn entrypoint deferred-wait: command executed and frame-loop lock check completed"
            );
            continue;
        }
        let mut registry = registry();
        let mut fake = None;
        let task_id = match entry {
            "foreground" => registry
                .spawn_with_shell_in_slot(
                    SpawnPlan::Unsandboxed,
                    &command,
                    BashShell::Bash,
                    resolve_posix_shell(),
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    TaskSlot::Foreground,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "background" | "replay-running" => registry
                .spawn(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "pty" => registry
                .spawn_pty(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                    24,
                    80,
                )
                .unwrap(),
            "remote-fallback" => {
                ensure_git_fixture(dir.path());
                let daemon = daemon(Script::Refused, "exec-remote/v1").await;
                let id = registry
                    .spawn_remote(
                        launch(daemon.connection().clone()),
                        SpawnPlan::Unsandboxed,
                        &command,
                        resolve_posix_shell(),
                        "session".into(),
                        dir.path().into(),
                        HashMap::new(),
                        HardKill::After(Duration::from_secs(30)),
                        None,
                        dir.path().into(),
                        TaskSlot::Background { max: 10 },
                        true,
                        false,
                        Some(dir.path().into()),
                    )
                    .unwrap();
                fake = Some(daemon);
                id
            }
            "replay-fallback" => {
                let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
                let (original, id, _) = persist_accepted_with_snapshot(
                    dir.path(),
                    daemon.connection().clone(),
                    &command,
                );
                drop(original);
                registry.replay_session(dir.path(), "session").unwrap();
                fake = Some(daemon);
                id
            }
            _ => unreachable!(),
        };
        wait_for("command must cross its startup boundary", || {
            fs::read(&counter).ok().filter(|bytes| bytes == b"x")
        })
        .await;
        if entry == "replay-running" {
            registry.test_shutdown_flag().store(true, Ordering::SeqCst);
            let replacement = aft_engine::test_bridge::bash_remote::remote_support::registry();
            replacement.replay_session(dir.path(), "session").unwrap();
            registry = replacement;
        }
        fs::write(&release, b"release").unwrap();
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{entry}: {done:?}"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: duplicate execution"
        );
        let replay = aft_engine::test_bridge::bash_remote::remote_support::registry();
        replay.replay_session(dir.path(), "session").unwrap();
        assert_eq!(
            terminal(&replay, &task_id).await.info.status,
            BgTaskStatus::Completed,
            "{entry}: terminal replay"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: replay ran command again"
        );
        eprintln!("spawn entrypoint {entry}: command executed once and replay completed");
        registry.test_shutdown_flag().store(true, Ordering::SeqCst);
        replay.test_shutdown_flag().store(true, Ordering::SeqCst);
        drop(fake);
    }
}

#[tokio::test]
async fn held_call_startup_reattaches_remote_job_without_a_route_bind() {
    held_call_startup_remote_result(false, "").await;
}

#[tokio::test]
async fn held_call_startup_lost_remote_job_settles_as_unknown_outcome() {
    held_call_startup_remote_result(true, "").await;
}

#[tokio::test]
async fn held_call_startup_lost_remote_job_preserves_output() {
    held_call_startup_remote_result(true, "partial output\n").await;
}

async fn held_call_startup_remote_result(lost: bool, output: &str) {
    use crate::subc::held_task_recovery::tests as recovery;
    let daemon = daemon(
        if lost { Script::Lost } else { Script::Restart },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let root = storage.join("runner");
    fs::create_dir_all(&root).unwrap();
    let old = recovery::context(&root, &storage);
    let (registry, task_id, _) = persist_accepted_with_snapshot(
        &root,
        daemon.connection().clone(),
        "printf must-not-run-locally",
    );
    registry.detach();
    drop(registry);
    let layout = resolve_task_layout(&session_tasks_dir(&root, "session"), &task_id).unwrap();
    let mut metadata = read_task(&layout.paths.json).unwrap();
    let job_id = metadata.remote.as_ref().unwrap().job_id.unwrap();
    if !output.is_empty() {
        fs::write(&layout.paths.stdout, output).unwrap();
        metadata.compressed = false;
        metadata.completion_has_secret_placeholders = Some(false);
        metadata.completion_has_env_secrets = Some(false);
        metadata.completion_has_non_ticket_env_secrets = Some(false);
        let remote = metadata.remote.as_mut().unwrap();
        remote.last_seq = Some(1);
        remote.stdout_len = output.len() as u64;
        remote.output_received = true;
        write_task_at(&layout, &metadata).unwrap();
    }
    let key = recovery::hold_task(&old, &layout, "remote-held");
    drop(old);
    let restarted = recovery::context(&root, &storage);
    let executor = recovery::startup(&restarted).await;
    let frame = recovery::settled(&restarted, &key).await;
    if lost {
        let result: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        let preview = if output.is_empty() {
            String::new()
        } else {
            format!("Output received before that:\n{output}")
        };
        assert_eq!(
            result["content"][0]["text"],
            format!("Task {task_id}: outcome unknown (AFT lost the result of build-server job {job_id}).\n{preview}Check this task with bash_status before running the command again.")
        );
    } else {
        let result: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        assert!(result["content"].to_string().contains("ABCD"), "{result}");
        assert_eq!(result["structuredContent"]["exit_code"], 0, "{result}");
    }
    let log = daemon.log().lock().unwrap();
    assert!(log.iter().any(|(_, body)| body["method"] == "exec.attach"));
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
    drop(log);
    for actor in executor.actor_contexts() {
        actor.bash_background().detach();
    }
}

async fn restart_unaccepted_submission(
    connection: PathBuf,
    job_id: Option<Uuid>,
    keyed: bool,
) -> (
    tempfile::TempDir,
    Arc<crate::context::AppContext>,
    String,
    TaskPaths,
) {
    use crate::subc::held_task_recovery::tests as recovery;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let root = storage.join("runner");
    fs::create_dir_all(&root).unwrap();
    let old = recovery::context(&root, &storage);
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(&root, connection, "printf must-not-run-locally");
    registry.detach();
    drop(registry);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.harness = Some("runner".into());
    metadata.execution_note = None;
    let remote = metadata.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.job_id = job_id;
    remote.queue_position = None;
    remote.explicit_runon = true;
    remote.submission = keyed.then(|| preacceptance_submission_for_test(&task_id));
    write_task(&paths.json, &metadata).unwrap();
    let db = old.db().unwrap();
    crate::db::bash_tasks::upsert_bash_task(
        &db.lock().unwrap(),
        &metadata.to_bash_task_row("runner", &paths).unwrap(),
    )
    .unwrap();
    drop(db);
    drop(old);
    let restarted = recovery::context(&root, &storage);
    let executor = recovery::startup(&restarted).await;
    let context = executor
        .actor_contexts()
        .into_iter()
        .find(|actor| actor.bash_background().test_task(&task_id).is_some())
        .expect("startup must adopt the saved submission without a held call or route bind");
    (dir, context, task_id, paths)
}

#[tokio::test]
async fn remote_restart_unaccepted_running_job_attaches_and_finishes() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{
        outage_daemon_with_replay, OutageReplay,
    };
    let daemon = outage_daemon_with_replay(OutageReplay::Running).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
    let registry = ctx.bash_background();
    wait_for(
        "attached output must arrive while the job is running",
        || (fs::read(&paths.stdout).unwrap() == b"AB").then_some(()),
    )
    .await;
    assert!(!registry
        .observed_status(&task_id, "session", 0)
        .unwrap()
        .info
        .status
        .is_terminal());
    assert_eq!(
        registry.remote_progress(&task_id, "session").unwrap().phase,
        RemotePhase::Running
    );
    assert!(!rendered_status(&ctx, &task_id).contains("capturing inputs"));
    daemon.finish_job();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AB");
    assert!(done.output_preview.contains("AB"), "{done:?}");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0]["params"]["from_seq"], 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert!(!read_task(&paths.json).unwrap().local_fallback_started);
}

#[tokio::test]
async fn remote_restart_unaccepted_completed_job_replays_output() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{
        outage_daemon_with_replay, OutageReplay,
    };
    let daemon = outage_daemon_with_replay(OutageReplay::Finished).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    let registry = ctx.bash_background();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AB");
    assert!(done.output_preview.contains("AB"), "{done:?}");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0]["params"]["from_seq"], 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert!(!read_task(&paths.json).unwrap().local_fallback_started);
}

#[tokio::test]
async fn remote_restart_unaccepted_never_ran_job_settles() {
    let daemon = daemon(Script::PlacementWait { job_id: true }, "exec-remote/v1").await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    let registry = ctx.bash_background();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        done.output_preview
            .contains("command never started on ck-motor"),
        "{done:?}"
    );
    let saved = read_task(&paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().terminal.unwrap().ran, Some(Ran::None));
    assert!(!saved.local_fallback_started);
    let log = daemon.log().lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .collect();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].1["params"]["job_id"], id().to_string());
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
}

#[tokio::test]
async fn remote_restart_unaccepted_without_job_id_settles_without_resubmission() {
    for keyed in [false, true] {
        let daemon = daemon(Script::PlacementWait { job_id: false }, "exec-remote/v1").await;
        let (_dir, ctx, task_id, paths) =
            restart_unaccepted_submission(daemon.connection().clone(), None, keyed).await;
        let registry = ctx.bash_background();
        let done = terminal(&registry, &task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
        assert!(
            done.output_preview.contains("command was not resubmitted"),
            "{done:?}"
        );
        assert!(!read_task(&paths.json).unwrap().local_fallback_started);
        let log = daemon.log().lock().unwrap();
        assert!(!log
            .iter()
            .any(|(_, body)| matches!(body["method"].as_str(), Some("exec.run" | "exec.attach"))));
    }
}

#[tokio::test]
async fn remote_restart_unaccepted_queued_progress_reaches_bash_status() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{
        outage_daemon_with_replay, OutageReplay,
    };
    let daemon = outage_daemon_with_replay(OutageReplay::Queued).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
    let task = ctx.bash_background().test_task(&task_id).unwrap();
    wait_for(
        "attach queue progress must finish publishing its status",
        || {
            let mut state = task.test_state().lock().unwrap();
            if state.remote_admission_note().as_deref() != Some("queued on the runner (42 s)") {
                return None;
            }
            // Metadata is saved before the ephemeral line is published. Wait for
            // both, then remove the line so bash_status must render the phase note.
            assert_eq!(
                state.metadata().remote.as_ref().unwrap().queue_wait_ms,
                Some(42_000)
            );
            *state.remote_admission_note_mut() = None;
            assert_eq!(
                remote_phase_note(&state.metadata(), unix_millis()).as_deref(),
                Some("queued on the runner (42 s)")
            );
            Some(())
        },
    )
    .await;
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "status", "command": "bash_status", "session_id": "session",
        "params": { "task_id": task_id },
    }))
    .unwrap();
    let response = crate::commands::bash_status::handle(&request, &ctx);
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["phase"], "queued", "{response:?}");
    let status = rendered_status(&ctx, &task_id);
    assert!(status.contains("queued on the runner (42 s)"), "{status}");
    assert!(!status.contains("capturing inputs"), "{status}");
    let saved = read_task(&paths.json).unwrap();
    let remote = saved.remote.unwrap();
    assert!(!remote.accepted);
    assert_eq!(remote.last_seq, None);
    assert_eq!(remote.queue_position, None);
    assert_eq!(remote.stdout_len, 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert_eq!(daemon.methods("exec.attach").len(), 1);
    daemon.finish_job();
    assert_eq!(
        terminal(ctx.bash_background(), &task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

#[tokio::test]
async fn remote_queue_wait_wire_vectors_reach_bash_status_as_queued_progress() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{
        outage_daemon_with_replay, OutageReplay,
    };
    use sha2::{Digest, Sha256};
    // Published 0.2.22 progress vectors, including an unknown waiting reason.
    // Send their bytes unchanged: enum construction can conceal pin changes.
    for (bytes, digest) in [
        (
            include_bytes!("../../../../aft-shell/src/exec_remote/fixtures/progress/queue-wait.jcs").as_slice(),
            include_str!("../../../../aft-shell/src/exec_remote/fixtures/progress/queue-wait.sha256"),
        ),
        (
            include_bytes!("../../../../aft-shell/src/exec_remote/fixtures/progress/queue-wait-unknown-reason.jcs")
                .as_slice(),
            include_str!("../../../../aft-shell/src/exec_remote/fixtures/progress/queue-wait-unknown-reason.sha256"),
        ),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), digest.trim());
        let daemon = outage_daemon_with_replay(OutageReplay::QueuedWire(bytes)).await;
        let (_dir, ctx, task_id, paths) =
            restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
        let task = ctx.bash_background().test_task(&task_id).unwrap();
        wait_for("published queue-wait bytes must reach bash status", || {
            let mut state = task.test_state().lock().unwrap();
            if state.remote_admission_note().as_deref() != Some("queued on the runner (0 s)") {
                return None;
            }
            assert_eq!(
                state.metadata().remote.as_ref().unwrap().queue_wait_ms,
                Some(42)
            );
            *state.remote_admission_note_mut() = None;
            Some(())
        })
        .await;
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id":"status", "command":"bash_status", "session_id":"session", "params":{"task_id":task_id},
        })).unwrap();
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["phase"], "queued", "{response:?}");
        assert!(rendered_status(&ctx, &task_id).contains("queued on the runner (0 s)"));
        let remote = read_task(&paths.json).unwrap().remote.unwrap();
        assert_eq!(remote.queue_wait_ms, Some(42));
        assert!(!remote.accepted);
        assert_eq!(remote.last_seq, None);
        assert_eq!(remote.stdout_len, 0);
        assert!(daemon.methods("exec.run").is_empty());
        daemon.finish_job();
        assert_eq!(
            terminal(ctx.bash_background(), &task_id).await.info.status,
            BgTaskStatus::Completed
        );
    }
}

#[tokio::test]
async fn remote_restart_unaccepted_quiet_attach_waits_without_claiming_capture() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{
        outage_daemon_with_replay, OutageReplay,
    };
    let daemon = outage_daemon_with_replay(OutageReplay::Silent).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    wait_for("startup must subscribe to the saved job", || {
        (!daemon.methods("exec.attach").is_empty()).then_some(())
    })
    .await;
    let status = rendered_status(&ctx, &task_id);
    assert!(
        status.contains(
            "waiting on the runner (ck-motor has not reported a queue position or any output yet)"
        ),
        "{status}"
    );
    assert!(!status.contains("capturing inputs"), "{status}");
    assert!(!status.contains("queued on the runner"), "{status}");
    let saved = read_task(&paths.json).unwrap();
    assert!(!saved.remote.as_ref().unwrap().accepted);
    assert!(serde_json::to_value(saved).unwrap()["remote"]
        .get("queue_wait_ms")
        .is_none());
    assert!(daemon.methods("exec.run").is_empty());
    assert_eq!(daemon.methods("exec.attach").len(), 1);
    daemon.finish_job();
    assert_eq!(
        terminal(ctx.bash_background(), &task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

/// `message` is the exact refusal the bash response must carry.
pub(in crate::subc) fn assert_siblings_submit_refused_before_connect(
    ctx: &crate::context::AppContext,
    launch: crate::bash_background::RemoteLaunch,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
    message: &str,
) {
    let marker = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("must-not-run-locally");
    let response = handle_with_policy(
        ctx,
        Some(launch),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()), "runon":"linux",
            "background":true, "compressed":false,
        }),
    );
    assert!(!response.success, "{response:?}");
    assert_eq!(response.data["code"], "remote_run_refused", "{response:?}");
    assert!(
        ctx.bash_background().list(0).is_empty(),
        "a refused sibling must not register a remote or local task"
    );
    assert_eq!(
        response.data["message"].as_str().unwrap(),
        message,
        "{response:?}"
    );
    assert!(
        !marker.exists(),
        "a refused sibling must not trigger local fallback"
    );
    assert!(
        daemon.log().lock().unwrap().is_empty(),
        "no daemon frame may be sent"
    );
    assert_eq!(
        daemon.connections().load(Ordering::SeqCst),
        0,
        "the daemon must not be contacted"
    );
}

/// Submit a runon command under `launch`, wait until its task completes, and
/// return the parameters of the one `exec.run` the daemon received.
pub(in crate::subc) async fn submit_runon_for_test(
    ctx: &crate::context::AppContext,
    launch: crate::bash_background::RemoteLaunch,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
) -> serde_json::Value {
    let response = handle_with_policy(
        ctx,
        Some(launch),
        serde_json::json!({"command":"printf remote", "runon":"linux", "compressed":false}),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let mut runs = exec_runs(daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.remove(0)
}
