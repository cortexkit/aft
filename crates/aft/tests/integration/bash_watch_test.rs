use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess, ReleaseOnDrop};

fn configure_background(aft: &mut AftProcess) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let response = aft.send(
        &json!({
            "id": "cfg-watch-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    dir
}

fn configure_background_with_storage(
    aft: &mut AftProcess,
    project_root: &Path,
    storage_dir: &Path,
) {
    let response = aft.send(
        &json!({
            "id": "cfg-watch-bg-storage",
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root,
            "storage_dir": storage_dir,
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn notify(aft: &mut AftProcess, task_id: &str, params: Value) -> Value {
    let mut params = params.as_object().unwrap().clone();
    params.insert("task_id".into(), json!(task_id));
    aft.send(
        &json!({
            "id": "notify-watch",
            "command": "bash_notify",
            "params": params,
        })
        .to_string(),
    )
}

fn spawn(aft: &mut AftProcess, command: &str) -> String {
    let spawn = aft.send(
        &json!({
            "id": "spawn-watch-bg",
            "command": "bash",
            "params": { "command": command, "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    spawn["task_id"].as_str().unwrap().to_string()
}

#[cfg(windows)]
fn print_ready_after_complete_command() -> &'static str {
    "Write-Host -NoNewline READY-AFTER-COMPLETE"
}

#[cfg(not(windows))]
fn print_ready_after_complete_command() -> &'static str {
    "printf READY-AFTER-COMPLETE"
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(not(windows))]
fn release_gate_command(release: &Path, text: &str) -> String {
    const MAX_POLLS: usize = 6_000;
    let release = shell_quote(&release.display().to_string());
    format!(
        "polls=0; while [ ! -f {release} ] && [ \"$polls\" -lt {MAX_POLLS} ]; do sleep 0.05; polls=$((polls + 1)); done; if [ -f {release} ]; then printf '%s\\n' {}; else printf '%s\\n' 'gate-timeout'; fi",
        shell_quote(text)
    )
}

#[cfg(windows)]
fn release_gate_command(release: &Path, text: &str) -> String {
    const MAX_POLLS: usize = 6_000;
    let release = shell_quote(&release.display().to_string());
    format!(
        "$polls = 0; while ((-not (Test-Path -LiteralPath {release})) -and ($polls -lt {MAX_POLLS})) {{ Start-Sleep -Milliseconds 50; $polls++ }}; if (Test-Path -LiteralPath {release}) {{ Write-Output {} }} else {{ Write-Output 'gate-timeout' }}",
        shell_quote(text)
    )
}

fn wait_for_pattern_frame(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["type"] == "bash_pattern_match" && frame["task_id"] == task_id {
                return frame;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for pattern frame"
        );
    }
}

fn assert_no_pattern_frame(aft: &mut AftProcess, task_id: &str, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(100)) {
            assert!(
                frame["type"] != "bash_pattern_match" || frame["task_id"] != task_id,
                "watch emitted more than one terminal frame: {frame:?}"
            );
        }
    }
}

#[test]
fn release_guard_unblocks_gated_child_after_panic() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("panic-release");
    let mut child_pid = None;
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Declare after the TempDir: Rust drops locals in reverse declaration order,
        // so this guard writes the sentinel before the TempDir removes its directory.
        let _release_guard = ReleaseOnDrop::new(release.clone());
        let task_id = spawn(&mut aft, &release_gate_command(&release, "panic-child"));
        let running = status(&mut aft, &task_id);
        assert_eq!(
            running["status"], "running",
            "task exited early: {running:?}"
        );
        child_pid = Some(running["child_pid"].as_u64().expect("gated task child PID") as u32);
        panic!("intentional panic after spawning gated task");
    }));

    assert!(panic_result.is_err());
    let child_pid = child_pid.expect("panic test recorded child PID");
    // A positive wait: on a loaded Windows runner a PowerShell child can take
    // seconds to notice the sentinel and exit. Without the guard the child
    // polls for its full 300 s gate, so 30 s still proves the guard fired.
    let deadline = Instant::now() + Duration::from_secs(30);
    while aft::bash_background::process::is_process_alive(child_pid) {
        assert!(
            Instant::now() < deadline,
            "gated child {child_pid} survived ReleaseOnDrop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        release.exists(),
        "panic guard must write the release sentinel"
    );
    assert!(aft.shutdown().success());
}

fn status(aft: &mut AftProcess, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": "status-watch",
            "command": "bash_status",
            "params": { "task_id": task_id }
        })
        .to_string(),
    )
}

#[test]
fn bash_regex_match_command_uses_multiline_regex_and_byte_offsets() {
    let mut aft = AftProcess::spawn();
    let response = aft.send(
        &json!({
            "id": "regex-match",
            "command": "bash_regex_match",
            "params": { "pattern": "^foo$", "text": "α\nfoo\nbar" }
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], true,
        "regex match failed: {response:?}"
    );
    assert_eq!(response["matched"], true);
    assert_eq!(response["match_text"], "foo");
    assert_eq!(response["match_offset"], 3);
    assert_eq!(response["match_index_chars"], 2);

    let invalid = aft.send(
        &json!({
            "id": "regex-invalid",
            "command": "bash_regex_match",
            "params": { "pattern": "(", "text": "" }
        })
        .to_string(),
    );
    assert_eq!(invalid["success"], false);
    assert_eq!(invalid["code"], "invalid_regex");
    assert!(aft.shutdown().success());
}

#[test]
fn register_pattern_watch_returns_watch_id() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 1; echo READY");
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    assert!(response["watch_id"].as_str().unwrap().starts_with("watch-"));
    assert!(aft.shutdown().success());
}

#[test]
fn pattern_match_emits_push_frame() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("pattern-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = release_gate_command(&release, "READY");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    drop(_release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["match_text"], "READY");
    assert_eq!(frame["once"], true);
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn each_line_push_batches_all_lines_with_offsets() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("each-line-release");
    let payload = dir.path().join("each-line-output");
    fs::write(&payload, b"ERROR a\nERROR b\nERROR b\n").unwrap();
    let release_guard = ReleaseOnDrop::new(release.clone());
    let command = format!(
        "while [ ! -f {} ]; do sleep 0.05; done; cat {}",
        shell_quote(&release.display().to_string()),
        shell_quote(&payload.display().to_string())
    );
    let task_id = spawn(&mut aft, &command);
    let response = notify(
        &mut aft,
        &task_id,
        json!({"pattern":"ERROR", "deliver":"each_line", "once":false}),
    );
    assert_eq!(response["success"], true, "{response:?}");
    drop(release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(
        frame["batch"]["lines"].as_array().unwrap().len(),
        2,
        "{frame:?}"
    );
    assert_eq!(frame["batch"]["lines"][0]["offset"], 0);
    assert_eq!(frame["batch"]["lines"][1]["offset"], 8);
    assert_eq!(frame["batch"]["lines"][1]["count"], 2);
    assert!(frame["context"].as_str().unwrap().contains("ERROR b"));
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn pattern_match_offset_counts_original_bytes_before_invalid_utf8() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("invalid-utf8-release");
    let payload = dir.path().join("invalid-utf8-output");
    fs::write(&payload, b"\xffREADY\n").unwrap();
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = format!(
        "polls=0; while [ ! -f {} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ -f {} ]; then cat {}; else printf '%s\\n' 'gate-timeout'; fi",
        shell_quote(&release.display().to_string()),
        shell_quote(&release.display().to_string()),
        shell_quote(&payload.display().to_string()),
    );
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");

    drop(_release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);

    assert_eq!(frame["match_text"], "READY");
    assert_eq!(frame["match_offset"], 1);
    assert!(aft.shutdown().success());
}

#[test]
fn cap_8_watches_per_task_rejects_9th() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 2");
    for idx in 0..8 {
        let response = notify(&mut aft, &task_id, json!({ "pattern": format!("x{idx}") }));
        assert_eq!(
            response["success"], true,
            "notify {idx} failed: {response:?}"
        );
    }
    let ninth = notify(&mut aft, &task_id, json!({ "pattern": "x9" }));
    assert_eq!(ninth["success"], false);
    assert_eq!(ninth["code"], "too_many_watches");
    assert!(aft.shutdown().success());
}

#[test]
fn regex_pattern_matches_with_capture() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("regex-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = release_gate_command(&release, "port 3000");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "regex": "port (\\d+)" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    drop(_release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["match_text"], "port 3000");
    assert!(aft.shutdown().success());
}

#[test]
fn final_output_scan_emits_pattern_before_completion_on_exit_race() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("exit-race-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = release_gate_command(&release, "ready-now");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "ready-now" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    drop(_release_guard);

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] == task_id {
                assert_eq!(
                    frame["type"], "bash_pattern_match",
                    "watch-controlled task completed before final pattern scan: {frame:?}"
                );
                assert_eq!(frame["match_text"], "ready-now");
                assert_eq!(frame["reason"], "pattern_match");
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for first terminal watch frame"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn watch_controlled_exit_emits_exit_safety_net_not_completion() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("exit-safety-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = release_gate_command(&release, "never-matches-output");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    drop(_release_guard);

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] != task_id {
                continue;
            }
            assert_eq!(
                frame["type"], "bash_pattern_match",
                "watch-controlled task emitted a background completion: {frame:?}"
            );
            assert_eq!(frame["reason"], "task_exit");
            assert!(frame["context"]
                .as_str()
                .unwrap()
                .contains("never-matches-output"));
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for exit safety-net frame"
        );
    }

    let drained = aft.send(
        &json!({
            "id": "drain-watch-exit",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|completion| completion["task_id"] != task_id),
        "watch-controlled task also queued a normal completion: {drained:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn watch_controlled_exit_drain_redelivers_dropped_safety_net_until_ack() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("durable-exit-safety-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = release_gate_command(&release, "durable-never-matches-output");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    drop(_release_guard);

    // Consume and intentionally discard the live push to model a disconnected plugin.
    let live_frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(live_frame["reason"], "task_exit");

    let drained = aft.send(
        &json!({
            "id": "drain-durable-watch-exit",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|completion| completion["task_id"] != task_id),
        "watch-controlled task also queued a normal completion: {drained:?}"
    );
    let pending_match = drained["pending_matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pending| pending["task_id"] == task_id)
        .unwrap_or_else(|| panic!("drain lost durable task-exit safety net: {drained:?}"));
    assert_eq!(pending_match["reason"], "task_exit");
    assert_eq!(pending_match["context"], live_frame["context"]);

    let ack = aft.send(
        &json!({
            "id": "ack-durable-watch-exit",
            "command": "bash_ack_completions",
            "params": { "task_ids": [&task_id] }
        })
        .to_string(),
    );
    assert_eq!(ack["success"], true, "task-exit ack failed: {ack:?}");
    assert_eq!(ack["acked_task_ids"], json!([task_id]));

    let drained_after_ack = aft.send(
        &json!({
            "id": "drain-after-task-exit-ack",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert!(drained_after_ack["pending_matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|pending| pending["task_id"] != task_id));
    assert!(drained_after_ack["bg_completions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|completion| completion["task_id"] != task_id));

    let conn = aft::db::open(&aft.cache_dir().join("aft").join("aft.db"))
        .expect("open isolated test database");
    let completion_delivered: i64 = conn
        .query_row(
            "SELECT completion_delivered FROM bash_tasks WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
            |row| row.get(0),
        )
        .expect("acked task row remains available");
    assert_eq!(completion_delivered, 1);
    let remaining_watches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM bash_pattern_watches WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_watches, 0, "ack must remove durable exit row");

    assert!(aft.shutdown().success());
}

fn durable_task_and_watch_rows(conn: &rusqlite::Connection, task_id: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM bash_tasks WHERE harness = 'opencode' AND task_id = ?1),
            (SELECT COUNT(*) FROM bash_pattern_watches WHERE harness = 'opencode' AND task_id = ?1)",
        [task_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// A row deleted under a running, watched task is written again from the
/// live task, together with its watch row. The watch stays armed and fires on
/// the task's real output; a running task is never reported as erased.
#[test]
fn erased_row_under_running_watched_task_is_restored_and_watch_still_fires() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("erased-watch-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let task_id = spawn(
        &mut aft,
        &release_gate_command(&release, "READY-AFTER-ERASE"),
    );
    let response = notify(
        &mut aft,
        &task_id,
        json!({ "pattern": "READY-AFTER-ERASE" }),
    );
    assert_eq!(response["success"], true, "notify failed: {response:?}");

    let db_path = aft.cache_dir().join("aft").join("aft.db");
    let conn = aft::db::open(&db_path).expect("open isolated test database");
    let deleted = conn
        .execute(
            "DELETE FROM bash_tasks WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
        )
        .expect("erase watched task row");
    assert_eq!(deleted, 1, "armed task row must exist before mutation");
    assert_eq!(
        durable_task_and_watch_rows(&conn, &task_id),
        (0, 0),
        "the task-row delete must cascade to the watch row"
    );

    // The watchdog's erased-row check runs every 500 ms.
    let deadline = Instant::now() + Duration::from_secs(6);
    while durable_task_and_watch_rows(&conn, &task_id) != (1, 1) {
        assert!(
            Instant::now() < deadline,
            "running task's rows were not restored: {:?}",
            durable_task_and_watch_rows(&conn, &task_id)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(_release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(
        frame["reason"], "pattern_match",
        "a running task must not be tombstoned: {frame:?}"
    );
    assert_eq!(frame["match_text"], "READY-AFTER-ERASE");
    assert_no_pattern_frame(&mut aft, &task_id, Duration::from_millis(1_200));
    assert!(aft.shutdown().success());
}

/// One project root is shared by routes from different harnesses, and each
/// configure replaces the root's harness. A task started under the first
/// harness keeps its task and watch rows under that harness, stays running in
/// `bash_status`, and its watch still fires after a second harness configures
/// the same root.
#[test]
fn watch_and_status_survive_another_harness_configuring_the_root() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("harness-switch-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let task_id = spawn(
        &mut aft,
        &release_gate_command(&release, "READY-AFTER-HARNESS-SWITCH"),
    );
    let reconfigured = aft.send(
        &json!({
            "id": "cfg-watch-bg-second-harness",
            "command": "configure",
            "harness": "pi",
            "project_root": dir.path(),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        reconfigured["success"], true,
        "second-harness configure failed: {reconfigured:?}"
    );

    let response = notify(
        &mut aft,
        &task_id,
        json!({ "pattern": "READY-AFTER-HARNESS-SWITCH" }),
    );
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    let running = status(&mut aft, &task_id);
    assert_eq!(
        running["success"], true,
        "a running task must not be reported erased: {running:?}"
    );
    assert_eq!(running["status"], "running");
    let conn = aft::db::open(&aft.cache_dir().join("aft").join("aft.db"))
        .expect("open isolated test database");
    assert_eq!(
        durable_task_and_watch_rows(&conn, &task_id),
        (1, 1),
        "task and watch rows must both be keyed under the spawning harness"
    );

    drop(_release_guard);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["reason"], "pattern_match", "{frame:?}");
    assert_eq!(frame["match_text"], "READY-AFTER-HARNESS-SWITCH");
    assert!(aft.shutdown().success());
}

/// `bash_status` reports a running task as running even after its row is
/// erased (the row is written again from the live task), and still reports a
/// never-existing ID as not found.
#[test]
fn bash_status_reports_running_task_after_its_row_is_erased() {
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let storage_dir = project.path().join("task-storage");
    fs::create_dir_all(&storage_dir).unwrap();
    let mut aft = AftProcess::spawn_with_env(&[("AFT_CACHE_DIR", cache.path().as_os_str())]);
    configure_background_with_storage(&mut aft, project.path(), &storage_dir);
    let release = project.path().join("erased-status-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let release_guard = ReleaseOnDrop::new(release.clone());
    let task_id = spawn(
        &mut aft,
        &release_gate_command(&release, "never-reached-erased-status"),
    );
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    let running = status(&mut aft, &task_id);
    let child_pid = running["child_pid"].as_u64().expect("gated task child PID") as u32;

    let db_path = storage_dir.join("aft.db");
    let conn = aft::db::open(&db_path).expect("open isolated test database");
    assert_eq!(
        conn.execute(
            "DELETE FROM bash_tasks WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
        )
        .expect("erase watched task row"),
        1
    );

    let after_erase = status(&mut aft, &task_id);
    assert_eq!(
        after_erase["success"], true,
        "a running task must stay addressable after its row is erased: {after_erase:?}"
    );
    assert_eq!(after_erase["status"], "running");
    assert_eq!(
        durable_task_and_watch_rows(&conn, &task_id),
        (1, 1),
        "status must find the running task's rows restored"
    );

    let never_existed_id = "bash-000000000000dead";
    let unknown = status(&mut aft, never_existed_id);
    assert_eq!(unknown["success"], false);
    assert_eq!(unknown["code"], "task_not_found");
    assert!(!unknown["message"]
        .as_str()
        .unwrap()
        .contains("row was erased"));

    drop(release_guard);
    // A positive liveness wait: the gated child polls for the release file and
    // then exits. On a loaded Windows runner, PowerShell can take several
    // seconds to notice and exit, so the bound is generous; a child that never
    // exits still fails here.
    let child_deadline = Instant::now() + Duration::from_secs(30);
    while aft::bash_background::process::is_process_alive(child_pid)
        && Instant::now() < child_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !aft::bash_background::process::is_process_alive(child_pid),
        "gated child {child_pid} did not exit after release"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn registering_watch_after_completion_removes_completion_and_emits_one_watch_frame() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, print_ready_after_complete_command());

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] == task_id {
                assert_eq!(
                    frame["type"], "bash_completed",
                    "task should first complete normally before watch registration: {frame:?}"
                );
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for completion frame before watch registration"
        );
    }

    let response = notify(
        &mut aft,
        &task_id,
        json!({ "pattern": "READY-AFTER-COMPLETE" }),
    );
    assert_eq!(response["success"], true, "notify failed: {response:?}");

    let mut task_frames = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) || task_frames.is_empty() {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(100)) {
            if frame["task_id"] == task_id {
                task_frames.push(frame);
            }
        }
        if started.elapsed() > Duration::from_secs(6) {
            break;
        }
    }

    assert_eq!(
        task_frames.len(),
        1,
        "watch-after-completion should emit exactly one task frame: {task_frames:?}"
    );
    assert_eq!(task_frames[0]["type"], "bash_pattern_match");
    assert_eq!(task_frames[0]["reason"], "pattern_match");
    assert_eq!(task_frames[0]["match_text"], "READY-AFTER-COMPLETE");

    let drained = aft.send(
        &json!({
            "id": "drain-after-late-watch",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|completion| completion["task_id"] != task_id),
        "late watch should remove queued normal completion: {drained:?}"
    );
    assert!(aft.shutdown().success());
}
