use crate::bash_background::registry::BgTaskRegistry;
use crate::sandbox_spawn::SpawnPlan;
use aft_engine::test_bridge::bash_remote::watch_pty_write_start_for_test;
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

const WAIT: Duration = Duration::from_secs(30);

/// Spawn a PTY task that waits for the file `ready` in `dir` before it runs
/// `after_gate`. While it waits, the program reads none of its input.
fn spawn_gated_pty(registry: &BgTaskRegistry, dir: &Path, after_gate: &str) -> String {
    registry
        .spawn_pty(
            SpawnPlan::Unsandboxed,
            &format!(
                "/bin/sh -c '{}; {after_gate}'",
                crate::test_gate::gate("ready", "sleep 0.01")
            ),
            "session".to_string(),
            dir.to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(120)),
            None,
            dir.to_path_buf(),
            10,
            true,
            false,
            Some(dir.to_path_buf()),
            24,
            80,
        )
        .unwrap()
}

fn context(dir: &Path) -> Arc<crate::context::AppContext> {
    Arc::new(crate::context::AppContext::from_app(
        crate::test_app(),
        crate::config::Config {
            project_root: Some(dir.into()),
            storage_dir: Some(dir.into()),
            ..Default::default()
        },
    ))
}

/// Run `bash_write` with `input` on its own thread; the receiver gets the
/// response when the call returns.
fn bash_write_in_background(
    ctx: &Arc<crate::context::AppContext>,
    task_id: &str,
    input: String,
) -> crossbeam_channel::Receiver<crate::protocol::Response> {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write",
        "command": "bash_write",
        "session_id": "session",
        "params": {"task_id": task_id, "input": input},
    }))
    .unwrap();
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let ctx = Arc::clone(ctx);
    std::thread::spawn(move || {
        let _ = done_tx.send(crate::commands::bash_write::handle(&request, &ctx));
    });
    done_rx
}

/// 1 MiB of 1 KiB lines: far more than a terminal's input queue holds.
fn one_mebibyte_of_lines() -> String {
    format!("{}\n", "x".repeat(1023)).repeat(1024)
}

#[test]
fn bash_write_to_a_program_not_reading_its_input_returns_by_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    // The gate is never opened: the program never reads its input.
    let task_id = spawn_gated_pty(ctx.bash_background(), dir.path(), "true");
    let input = one_mebibyte_of_lines();
    let total = input.len() as u64;

    let response = bash_write_in_background(&ctx, &task_id, input)
        .recv_timeout(WAIT)
        .expect("bash_write never returned while the program ignored its input");

    assert!(!response.success, "{:?}", response.data);
    assert_eq!(
        response.data["code"], "input_timeout",
        "{:?}",
        response.data
    );
    let written = response.data["bytes_written"].as_u64().unwrap();
    let pending = response.data["bytes_pending"].as_u64().unwrap();
    assert_eq!(written + pending, total);
    assert!(pending > 0, "{:?}", response.data);
    let _ = ctx.bash_background().kill(&task_id, "session");
}

#[test]
fn killing_a_pty_task_releases_a_write_its_program_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let task_id = spawn_gated_pty(ctx.bash_background(), dir.path(), "true");
    let started = watch_pty_write_start_for_test(&task_id);
    let done = bash_write_in_background(&ctx, &task_id, one_mebibyte_of_lines());
    started
        .recv_timeout(WAIT)
        .expect("bash_write never started writing");

    ctx.bash_background().kill(&task_id, "session").unwrap();

    let response = done
        .recv_timeout(WAIT)
        .expect("bash_write was still blocked after its task was killed");
    // Only the kill ends this write early; its deadline would answer
    // `input_timeout` instead.
    assert_eq!(response.data["code"], "task_exited", "{:?}", response.data);
}
