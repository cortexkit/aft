//! The background-task limit (`max_background_bash_tasks`) caps only tasks that
//! run in the background. A foreground command always starts, even when every
//! background slot is taken, and a foreground command promoted at the cap keeps
//! running. The limit is per project root, shared by all sessions in it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::Response;

use super::{BashShell, BgTaskStatus, HardKill};

const MAX: usize = 8;

fn context(project: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config {
        project_root: Some(project.to_path_buf()),
        storage_dir: Some(storage.to_path_buf()),
        experimental_bash_background: true,
        max_background_bash_tasks: MAX,
        ..crate::config::Config::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

/// Launches `command` the way the bash tool does: `background` marks a
/// background launch, otherwise it is an ordinary foreground command.
fn launch(ctx: &AppContext, session: &str, command: &str, background: bool) -> Response {
    super::spawn(
        "req-slot-limit",
        session,
        command,
        BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        HardKill::After(Duration::from_secs(60)),
        ctx,
        background,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    )
}

fn task_id(response: &Response) -> String {
    assert!(response.success, "launch failed: {:?}", response.data);
    response.data["task_id"].as_str().unwrap().to_string()
}

/// Fills every background slot of `ctx` with long sleeps, spread over two
/// sessions to show the slots are shared by the sessions of one root.
fn fill_background_slots(ctx: &AppContext) -> Vec<(String, String)> {
    (0..MAX)
        .map(|i| {
            let session = if i % 2 == 0 { "session-a" } else { "session-b" };
            let id = task_id(&launch(ctx, session, "sleep 30", true));
            (id, session.to_string())
        })
        .collect()
}

fn kill_all(ctx: &AppContext, tasks: &[(String, String)]) {
    for (task_id, session) in tasks {
        let _ = ctx.bash_background().kill(task_id, session);
    }
}

fn wait_terminal(
    ctx: &AppContext,
    task_id: &str,
    session: &str,
) -> super::registry::BgTaskSnapshot {
    let started = Instant::now();
    loop {
        let snapshot = ctx
            .bash_background()
            .observed_status(task_id, session, 4096)
            .expect("task is registered");
        if snapshot.info.status.is_terminal() {
            return snapshot;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "task {task_id} never finished: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn refusal_message(response: &Response) -> String {
    assert!(
        !response.success,
        "launch should be refused: {:?}",
        response.data
    );
    assert_eq!(
        response.data["code"], "background_task_limit_exceeded",
        "{:?}",
        response.data
    );
    response.data["message"].as_str().unwrap().to_string()
}

#[test]
fn foreground_command_runs_when_background_slots_are_full() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(project.path(), storage.path());
    let holders = fill_background_slots(&ctx);

    let foreground = launch(&ctx, "session-c", "echo ok", false);
    let foreground_id = task_id(&foreground);
    let done = wait_terminal(&ctx, &foreground_id, "session-c");

    kill_all(&ctx, &holders);
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(done.output_preview.trim(), "ok");
}

#[test]
fn refusal_lists_only_the_callers_own_holders_and_counts_other_sessions() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(project.path(), storage.path());
    // The caller holds 2 slots; another session holds the other 6, with
    // command text the caller must never see.
    let mut holders = Vec::new();
    for i in 0..2 {
        let command = format!("sleep 30 # mine-{i}");
        holders.push((task_id(&launch(&ctx, "caller", &command, true)), "caller"));
    }
    for i in 0..6 {
        let command = format!("sleep 30 # other-session-secret-{i}");
        holders.push((task_id(&launch(&ctx, "other", &command, true)), "other"));
    }

    let refused = launch(&ctx, "caller", "sleep 1", true);
    let message = refusal_message(&refused);
    // A PTY is a background launch too.
    let pty = super::spawn(
        "req-slot-limit-pty",
        "caller",
        "sleep 1",
        BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        HardKill::After(Duration::from_secs(60)),
        &ctx,
        true,
        false,
        false,
        true,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    let pty_message = refusal_message(&pty);
    for (task_id, session) in &holders {
        let _ = ctx.bash_background().kill(task_id, session);
    }

    assert!(
        message.starts_with("background bash task limit exceeded: 8 running (max 8)"),
        "{message}"
    );
    for (index, (task_id, session)) in holders.iter().enumerate() {
        if *session == "caller" {
            let row = message
                .lines()
                .find(|line| line.contains(task_id.as_str()))
                .unwrap_or_else(|| panic!("own holder {task_id} not listed:\n{message}"));
            assert!(
                row.contains(&format!("sleep 30 # mine-{index}")),
                "row lacks the command: {row}"
            );
            assert!(row.contains("running "), "row lacks the age: {row}");
        } else {
            assert!(
                !message.contains(task_id.as_str()),
                "another session's task id leaked:\n{message}"
            );
        }
    }
    assert!(
        !message.contains("other-session-secret"),
        "another session's command text leaked:\n{message}"
    );
    assert!(
        message.contains("\n  6 more held by other sessions in this project"),
        "{message}"
    );
    assert!(message.contains("bash_kill"), "{message}");
    assert!(message.contains("wait for a task to finish"), "{message}");
    assert!(pty_message.contains(holders[0].0.as_str()), "{pty_message}");
    assert!(
        !pty_message.contains("other-session-secret"),
        "{pty_message}"
    );
}

#[test]
fn foreground_command_promoted_at_cap_keeps_running_and_then_holds_a_slot() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(project.path(), storage.path());
    let holders = fill_background_slots(&ctx);
    let release = project.path().join("release");
    let command = format!(
        "{}; echo promoted-done",
        crate::test_gate::gate_exists(&format!("'{}'", release.display()), "sleep 0.05")
    );

    let foreground_id = task_id(&launch(&ctx, "session-c", &command, false));
    // Promotion is what a foreground call does when it outlives its wait
    // window; the slots are all taken, and it must not be refused or killed.
    ctx.bash_background()
        .promote(&foreground_id, "session-c")
        .expect("promote at the cap");
    let running = ctx
        .bash_background()
        .observed_status(&foreground_id, "session-c", 0)
        .unwrap();
    assert_eq!(running.info.status, BgTaskStatus::Running, "{running:?}");

    // From promotion on it holds a slot, so the count is above the cap.
    let refused = launch(&ctx, "session-c", "sleep 1", true);
    let message = refusal_message(&refused);
    std::fs::write(&release, "").unwrap();
    let done = wait_terminal(&ctx, &foreground_id, "session-c");
    kill_all(&ctx, &holders);

    assert!(
        message.starts_with("background bash task limit exceeded: 9 running (max 8)"),
        "{message}"
    );
    // Only the caller's own task is listed; the eight others are counted.
    let first_row = message.lines().nth(1).unwrap_or_default();
    assert!(first_row.contains(foreground_id.as_str()), "{message}");
    assert!(
        message.contains("\n  8 more held by other sessions in this project"),
        "{message}"
    );
    for (other_id, _) in &holders {
        assert!(!message.contains(other_id.as_str()), "{message}");
    }
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.output_preview.trim(), "promoted-done");
}

#[test]
fn background_slots_are_counted_per_project_root() {
    let storage = tempfile::tempdir().unwrap();
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let ctx_a = context(project_a.path(), storage.path());
    let ctx_b = context(project_b.path(), storage.path());
    let holders = fill_background_slots(&ctx_a);

    // Root A is full for every session, including one that holds no task.
    let refused_a = launch(&ctx_a, "session-c", "sleep 1", true);
    // Root B has its own slots.
    let other_root = launch(&ctx_b, "session-a", "sleep 30", true);
    let other_root_ok = other_root.success;
    if other_root_ok {
        let _ = ctx_b
            .bash_background()
            .kill(&task_id(&other_root), "session-a");
    }
    let message = refusal_message(&refused_a);
    kill_all(&ctx_a, &holders);

    assert!(
        other_root_ok,
        "another root's launch was refused: {:?}",
        other_root.data
    );
    assert!(
        message.starts_with("background bash task limit exceeded: 8 running (max 8)"),
        "{message}"
    );
    // The caller holds none of root A's slots.
    assert!(
        message.contains("\n  8 held by other sessions in this project"),
        "{message}"
    );
}

#[test]
fn refusal_lists_at_most_eight_holders_and_shortens_long_commands() {
    let long_command = format!("echo {}\nsecond line", "x".repeat(100));
    let holders: Vec<_> = (0..10)
        .map(|i| super::registry::BackgroundSlotHolder {
            task_id: format!("bgb-{i:02}"),
            command: long_command.clone(),
            age: Duration::from_secs(3_700),
        })
        .collect();
    let message = super::registry::format_background_slot_refusal(&holders, 0, 8);

    assert!(message.contains("10 running (max 8)"), "{message}");
    assert!(message.contains("bgb-07"), "{message}");
    assert!(!message.contains("bgb-08"), "{message}");
    assert!(message.contains("and 2 more of yours"), "{message}");
    assert!(!message.contains("other sessions"), "{message}");
    assert!(message.contains("running 1h01m"), "{message}");
    let row = message
        .lines()
        .find(|line| line.contains("bgb-00"))
        .unwrap();
    let shown = row.split("  ").last().unwrap();
    assert_eq!(
        shown.chars().count(),
        61,
        "60 characters and an ellipsis: {row}"
    );
    assert!(!message.contains("second line"), "{message}");
}
