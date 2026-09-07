#[cfg(unix)]
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
    let deadline = Instant::now() + Duration::from_secs(2);
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

#[test]
fn erased_watch_target_emits_tombstone_and_terminalizes_watch() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("erased-watch-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let task_id = spawn(
        &mut aft,
        &release_gate_command(&release, "never-reached-erased-watch"),
    );
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
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

    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["reason"], "task_exit");
    assert_eq!(frame["match_text"], "watch target erased");
    assert!(
        frame["context"]
            .as_str()
            .unwrap()
            .contains("background task row was erased"),
        "tombstone must explain the storage failure: {frame:?}"
    );
    let watch_state: (i64, i64, Option<String>) = conn
        .query_row(
            "SELECT scanning, pending_match, match_text
             FROM bash_pattern_watches
             WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("tombstoned watch row remains pending until ack");
    assert_eq!(watch_state, (0, 1, Some("watch target erased".into())));

    let ack = aft.send(
        &json!({
            "id": "ack-erased-watch",
            "command": "bash_ack_completions",
            "params": { "task_ids": [&task_id] }
        })
        .to_string(),
    );
    assert_eq!(ack["success"], true, "tombstone ack failed: {ack:?}");
    assert_eq!(ack["acked_task_ids"], json!([task_id]));
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM bash_pattern_watches WHERE task_id = ?1",
            [&task_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "acked tombstone must be terminally removed");
    drop(_release_guard);

    assert!(aft.shutdown().success());
}

#[test]
fn bash_status_distinguishes_erased_watched_task_from_never_existing_task() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("erased-status-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let task_id = spawn(
        &mut aft,
        &release_gate_command(&release, "never-reached-erased-status"),
    );
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");

    let conn = aft::db::open(&aft.cache_dir().join("aft").join("aft.db"))
        .expect("open isolated test database");
    assert_eq!(
        conn.execute(
            "DELETE FROM bash_tasks WHERE harness = 'opencode' AND task_id = ?1",
            [&task_id],
        )
        .expect("erase watched task row"),
        1
    );

    let erased = status(&mut aft, &task_id);
    assert_eq!(
        erased["success"], false,
        "erased status must fail: {erased:?}"
    );
    assert_eq!(erased["code"], "task_erased");
    assert!(
        erased["message"]
            .as_str()
            .unwrap()
            .contains("background task row was erased"),
        "erased error must not resemble a phantom id: {erased:?}"
    );

    let unknown = status(&mut aft, "bash-000000000000dead");
    assert_eq!(unknown["success"], false);
    assert_eq!(unknown["code"], "task_not_found");
    assert!(!unknown["message"]
        .as_str()
        .unwrap()
        .contains("row was erased"));
    drop(_release_guard);

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
