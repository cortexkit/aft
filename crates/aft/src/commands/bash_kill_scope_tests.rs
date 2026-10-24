use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bash_background::registry::with_scope_spawn;
use crate::bash_background::{BgTaskStatus, HardKill};
use crate::sandbox_spawn::SpawnPlan;
use serde_json::json;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

fn scope(name: &str) -> ScopeStamp {
    ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        name,
        1,
        ScopeKind::Worker,
    )
}

struct Tasks {
    registry: BgTaskRegistry,
    dir: tempfile::TempDir,
}

impl Tasks {
    fn new() -> Self {
        Self {
            registry: BgTaskRegistry::new(Arc::new(Mutex::new(None))),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn start(&self, scope: &ScopeStamp, session: &str, command: &str, background: bool) -> String {
        with_scope_spawn(Some(scope.clone()), || {
            self.registry.spawn_with_shell_in_slot(
                SpawnPlan::Unsandboxed,
                command,
                crate::bash_background::BashShell::Bash,
                crate::bash_background::registry::resolve_posix_shell(),
                session.into(),
                self.dir.path().into(),
                HashMap::new(),
                HardKill::After(Duration::from_secs(30)),
                None,
                self.dir.path().into(),
                if background {
                    crate::bash_background::TaskSlot::Background { max: 10 }
                } else {
                    crate::bash_background::TaskSlot::Foreground
                },
                background,
                false,
                Some(self.dir.path().into()),
            )
        })
        .unwrap()
    }

    fn cancel(&self, scope: &ScopeStamp) -> Response {
        handle_bash_kill_scope(
            json!({"scope": scope}),
            Some(&scope.owner),
            &[self.registry.clone()],
        )
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.registry.shutdown();
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "task readiness timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn bash_kill_scope_kills_promoted_background_and_grandchildren() {
    let tasks = Tasks::new();
    let stamp = scope("worker");
    let pid_file = tasks.dir.path().join("grandchild");
    let background = tasks.start(
        &stamp,
        "background",
        &format!("sh -c 'sleep 30 & echo $! > {}; wait'", pid_file.display()),
        true,
    );
    let foreground = tasks.start(&stamp, "foreground", "sleep 30", false);
    assert!(tasks.registry.promote(&foreground, "foreground").unwrap());
    wait_for(|| pid_file.exists());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(crate::bash_background::process::is_process_alive(
        pid as u32
    ));
    let reply = tasks.cancel(&stamp);
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert_eq!(killed.len(), 2);
    for (id, session) in [(&background, "background"), (&foreground, "foreground")] {
        assert!(killed.iter().any(|entry| entry["task_id"] == *id
            && entry["session_id"] == session
            && entry["remote"].is_null()));
        let snapshot = tasks.registry.status(id, session, None, None, 0).unwrap();
        assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
        let rendered = crate::subc_format::format_bash_task_status_header(
            id,
            "killed",
            snapshot.exit_code,
            snapshot.info.status_reason.as_deref(),
        );
        assert!(
            rendered.contains("killed (cancelled by owner)"),
            "{rendered}"
        );
        let notices = tasks.registry.take_cancelled_call_notices(session);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("killed (cancelled by owner)"));
        assert!(tasks
            .registry
            .take_cancelled_call_notices(session)
            .is_empty());
    }
    wait_for(|| !crate::bash_background::process::is_process_alive(pid as u32));
}

#[test]
fn bash_kill_scope_leaves_other_scope_owner_and_epoch_alone() {
    let tasks = Tasks::new();
    let stamp = scope("worker");
    tasks.start(&stamp, "target", "sleep 30", true);
    let mut epoch = stamp.clone();
    epoch.scope_epoch += 1;
    let mut owner = stamp.clone();
    owner.owner = Principal::Direct;
    let mut others = Vec::new();
    for (index, other) in [scope("other"), epoch, owner].into_iter().enumerate() {
        let session = format!("other-{index}");
        let task = tasks.start(&other, &session, "sleep 30", true);
        others.push((task, session));
    }
    let reply = tasks.cancel(&stamp);
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["killed"].as_array().unwrap().len(), 1);
    for (task, session) in others {
        assert_eq!(
            tasks
                .registry
                .status(&task, &session, None, None, 0)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Running
        );
    }
}

#[test]
fn bash_kill_scope_non_owner_is_typed_and_kills_nothing() {
    let tasks = Tasks::new();
    let stamp = scope("worker");
    let task = tasks.start(&stamp, "session", "sleep 30", true);
    for principal in [
        None,
        Some(Principal::Direct),
        Some(Principal::Reserved {
            module_id: "broca".into(),
        }),
    ] {
        let reply = handle_bash_kill_scope(
            json!({"scope":stamp}),
            principal.as_ref(),
            &[tasks.registry.clone()],
        );
        assert!(!reply.success);
        assert_eq!(reply.data["code"], "kill_scope_not_owner");
        assert_eq!(
            tasks
                .registry
                .status(&task, "session", None, None, 0)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Running
        );
    }
}

#[test]
fn bash_kill_scope_second_call_is_noop() {
    let tasks = Tasks::new();
    let stamp = scope("worker");
    tasks.start(&stamp, "session", "sleep 30", true);
    let first = tasks.cancel(&stamp);
    assert!(first.success, "{first:?}");
    assert_eq!(first.data["killed"].as_array().unwrap().len(), 1);
    let second = tasks.cancel(&stamp);
    assert!(second.success, "{second:?}");
    assert_eq!(second.data["killed"], json!([]));
    assert_eq!(second.data["not_found"], 0);
    assert_eq!(second.data["already_finished"], 1);
}

#[test]
fn bash_kill_scope_allows_same_worker_and_codemode_scope_to_resume() {
    let tasks = Tasks::new();
    for mut stamp in [scope("worker"), scope("run")] {
        if stamp.scope_ref == "run" {
            stamp.attributes.run_id = Some("run-id".into());
        }
        tasks.start(&stamp, &stamp.scope_ref, "sleep 30", true);
        let reply = tasks.cancel(&stamp);
        assert!(reply.success, "{reply:?}");
        let resumed = tasks.start(&stamp, &stamp.scope_ref, "sleep 30", true);
        tasks.registry.kill_scope_if_revoked(&stamp);
        assert_eq!(
            tasks
                .registry
                .status(&resumed, &stamp.scope_ref, None, None, 0)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Running
        );
        assert!(tasks.cancel(&stamp).success);
    }
}

#[test]
fn bash_kill_scope_success_wire_fixture_matches_handler() {
    let tasks = Tasks::new();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/bash-kill-scope/wire.json"
    ))
    .unwrap();
    let case = &fixture["success"];
    let stamp: ScopeStamp =
        serde_json::from_value(case["request"]["params"]["scope"].clone()).unwrap();
    let task = tasks.start(&stamp, "pause-session", "sleep 30", true);
    let reply = tasks.cancel(&stamp);
    assert!(reply.success, "{reply:?}");
    let mut expected = case["reply"].clone();
    expected["data"]["killed"][0]["task_id"] = json!(task);
    assert_eq!(
        json!({"op": BASH_KILL_SCOPE_OPERATION, "status":"ok", "data":reply.data}),
        expected
    );
}

#[test]
fn bash_kill_scope_malformed_scope_is_typed() {
    let tasks = Tasks::new();
    for params in [
        Value::Null,
        json!({}),
        json!({"scope":{"owner":{"kind":"direct"}}}),
    ] {
        let reply =
            handle_bash_kill_scope(params, Some(&Principal::Direct), &[tasks.registry.clone()]);
        assert!(!reply.success);
        assert_eq!(reply.data["code"], "invalid_request");
    }
}

#[test]
fn bash_kill_scope_completion_is_an_expected_owner_stop() {
    let tasks = Tasks::new();
    let stamp = scope("worker");
    let task = tasks.start(&stamp, "session", "sleep 30", true);
    assert!(tasks.cancel(&stamp).success);
    let completions = tasks.registry.pending_completions_for_session("session");
    let completion = completions
        .iter()
        .find(|completion| completion.task_id == task)
        .unwrap();
    assert_eq!(completion.status, BgTaskStatus::Killed);
    assert_eq!(completion.status_reason.as_deref(), Some("paused_by_owner"));
    assert!(
        completion
            .output_preview
            .contains("[killed (cancelled by owner); expected stop, not a command failure]"),
        "{completion:?}"
    );
}

#[test]
fn bash_kill_scope_next_bash_family_reply_names_cancellation_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(dir.path().into());
    let ctx = crate::context::AppContext::new(Box::new(crate::language::StubProvider), config);
    let tasks = Tasks {
        registry: ctx.bash_background().clone(),
        dir,
    };
    let stamp = scope("worker");
    let task = tasks.start(&stamp, "session", "sleep 30", true);
    assert!(tasks.cancel(&stamp).success);
    let mut response = Response::success("next", json!({}));
    let mut unrelated = String::new();
    crate::response_finalize::finalize_tool_response(
        &mut response,
        &mut unrelated,
        &ctx,
        "session",
        "read",
        false,
    );
    assert!(!unrelated.contains("cancelled by owner"));
    let mut next = String::new();
    crate::response_finalize::finalize_tool_response(
        &mut response,
        &mut next,
        &ctx,
        "session",
        "bash_status",
        false,
    );
    assert!(
        next.contains(&format!("task {task}: killed (cancelled by owner)")),
        "{next}"
    );
    let mut again = String::new();
    crate::response_finalize::finalize_tool_response(
        &mut response,
        &mut again,
        &ctx,
        "session",
        "bash",
        false,
    );
    assert!(!again.contains("cancelled by owner"));
}

fn read_pid(path: &std::path::Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A module restart leaves a project's saved tasks out of every live registry
/// until that project binds again, while their processes keep running. The
/// owner's pause before that bind must find the scope's saved task, stop its
/// process now, and say in the reply that the task came from its saved
/// record. A saved task of another scope is left running.
#[test]
fn bash_kill_scope_stops_saved_task_before_its_project_binds() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // Saved under a harness namespace, as configured routes store them.
    let harness_dir = storage.path().join("opencode");
    let previous = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let start = |stamp: &ScopeStamp, session: &str, pid_file: &std::path::Path| {
        with_scope_spawn(Some(stamp.clone()), || {
            previous.spawn_with_shell_in_slot(
                SpawnPlan::Unsandboxed,
                &format!("sh -c 'sleep 30 & echo $! > {}; wait'", pid_file.display()),
                crate::bash_background::BashShell::Bash,
                crate::bash_background::registry::resolve_posix_shell(),
                session.into(),
                project.path().into(),
                HashMap::new(),
                HardKill::After(Duration::from_secs(30)),
                None,
                harness_dir.clone(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(project.path().into()),
            )
        })
        .unwrap()
    };
    let stamp = scope("restarted-worker");
    let other = scope("other-worker");
    let paused_pid_file = project.path().join("paused");
    let other_pid_file = project.path().join("other");
    let task = start(&stamp, "session", &paused_pid_file);
    let other_task = start(&other, "other-session", &other_pid_file);
    wait_for(|| paused_pid_file.exists() && other_pid_file.exists());
    let paused_pid = read_pid(&paused_pid_file);
    let other_pid = read_pid(&other_pid_file);
    // The module exits: its registry lets go of the tasks without stopping
    // them, and no project has bound since.
    previous.detach();
    assert!(crate::bash_background::process::is_process_alive(
        paused_pid
    ));

    let reply = handle_bash_kill_scope_with_saved_tasks(
        json!({"scope": stamp}),
        Some(&stamp.owner),
        &[],
        Some(storage.path()),
    );
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        reply.data["killed"],
        json!([{
            "task_id": task,
            "session_id": "session",
            "remote": null,
            "from_saved_record": true,
        }]),
        "{reply:?}"
    );
    wait_for(|| !crate::bash_background::process::is_process_alive(paused_pid));
    assert!(crate::bash_background::process::is_process_alive(other_pid));

    // The project binds later: its replay restores the task as stopped by its
    // owner, never as a running command to resume.
    let rebound = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    rebound.replay_session(&harness_dir, "session").unwrap();
    let restored = rebound.status(&task, "session", None, None, 0).unwrap();
    assert_eq!(restored.info.status, BgTaskStatus::Killed, "{restored:?}");
    assert_eq!(
        restored.info.status_reason.as_deref(),
        Some("paused_by_owner")
    );
    rebound.detach();

    // Stop the other scope's command the same way, so the test leaves
    // nothing running.
    let other_reply = handle_bash_kill_scope_with_saved_tasks(
        json!({"scope": other}),
        Some(&other.owner),
        &[],
        Some(storage.path()),
    );
    assert_eq!(other_reply.data["killed"][0]["task_id"], other_task);
    wait_for(|| !crate::bash_background::process::is_process_alive(other_pid));
}
