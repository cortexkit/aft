use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

const SESSION: &str = "bash-token-count-test";

fn configure_background(aft: &mut AftProcess, project_root: &std::path::Path) {
    let response = aft.send(
        &json!({
            "id": "cfg-bash-token-counts",
            "session_id": SESSION,
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root,
            "experimental_bash_background": true,
            "experimental_bash_compress": true,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn spawn_background(
    aft: &mut AftProcess,
    command: &str,
    workdir: Option<&std::path::Path>,
) -> String {
    let mut params = json!({
        "command": command,
        "background": true,
    });
    if let Some(workdir) = workdir {
        params["workdir"] = json!(workdir);
    }
    let response = aft.send(
        &json!({
            "id": "spawn-bash-token-counts",
            "session_id": SESSION,
            "command": "bash",
            "params": params,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "spawn failed: {response:?}");
    response["task_id"].as_str().unwrap().to_string()
}

fn wait_for_completed_frame(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(100)) {
            if frame.get("type").and_then(|kind| kind.as_str()) == Some("bash_completed")
                && frame.get("task_id").and_then(|id| id.as_str()) == Some(task_id)
            {
                return frame;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for bash_completed frame for {task_id}"
        );
    }
}

fn run_background_command(command: &str) -> Value {
    let project = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path());
    let task_id = spawn_background(&mut aft, command, None);
    let frame = wait_for_completed_frame(&mut aft, &task_id);
    assert!(aft.shutdown().success());
    frame
}

#[test]
fn bash_completed_frame_includes_token_counts() {
    let frame = run_background_command("printf 'hello world'");
    let expected = aft_tokenizer::count_tokens("hello world") as u64;

    assert_eq!(frame["original_tokens"], expected);
    assert_eq!(frame["compressed_tokens"], expected);
    assert_eq!(frame["tokens_skipped"], false);
}

#[test]
fn bash_completed_frame_token_count_below_cap() {
    let output = "hello world\n".repeat(5_461);
    let frame = run_background_command("perl -e 'print \"hello world\\n\" x 5461'");

    assert_eq!(
        frame["original_tokens"],
        aft_tokenizer::count_tokens(&output) as u64
    );
    assert!(frame["compressed_tokens"].as_u64().is_some());
    assert_eq!(frame["tokens_skipped"], false);
}

#[test]
fn bash_completed_frame_tokenizes_tail_above_cap() {
    // Large output (~200KB) exceeds the 128KB-per-stream tokenize cap.
    // Previous behavior: skip tokenization entirely → no compression
    // accounting for any large output. New behavior: tokenize the last
    // 128KB so the compression-events table still records something
    // meaningful for the tasks that benefit most from compression.
    // `tokens_skipped` stays false because we DID produce a count;
    // truncation is silent at this layer.
    let frame = run_background_command("perl -e 'print \"hello world\\n\" x 17067'");

    let original = frame
        .get("original_tokens")
        .and_then(|v| v.as_u64())
        .expect(
            "original_tokens must be present even when output exceeds the 128KB tokenize cap — \
         large outputs now tokenize the tail rather than skipping",
        );
    let compressed = frame
        .get("compressed_tokens")
        .and_then(|v| v.as_u64())
        .expect("compressed_tokens must be present alongside original_tokens");
    // The tail capture is bounded by 128KB; the full output is ~200KB and
    // would tokenize to ~70K tokens. We expect somewhere between the
    // ~1K tokens from a tiny output and the full 70K — anything in that
    // window proves we read non-trivially from the spill.
    assert!(
        original > 1_000,
        "expected substantial token count from 128KB tail, got {original}"
    );
    assert!(compressed > 0);
    assert_eq!(frame["tokens_skipped"], false);
}

#[test]
fn bash_completed_frame_compressed_tokens_reflect_compression() {
    let git_version = Command::new("git").arg("--version").output();
    if !git_version.is_ok_and(|output| output.status.success()) {
        eprintln!("skipping git compression token test because git is unavailable");
        return;
    }

    let project = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .arg("init")
        .current_dir(project.path())
        .output()
        .unwrap()
        .status
        .success());
    for index in 0..80 {
        fs::write(
            project.path().join(format!("file_{index}.txt")),
            "changed\n",
        )
        .unwrap();
    }

    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path());
    let task_id = spawn_background(&mut aft, "git status", Some(project.path()));
    let frame = wait_for_completed_frame(&mut aft, &task_id);
    assert!(aft.shutdown().success());

    let original = frame["original_tokens"].as_u64().unwrap();
    let compressed = frame["compressed_tokens"].as_u64().unwrap();
    assert!(
        compressed < original,
        "expected compressed token count {compressed} to be lower than original {original}: {frame:?}"
    );
    assert_eq!(frame["tokens_skipped"], false);
}

#[test]
fn bash_completed_frame_empty_output_zero_tokens() {
    let frame = run_background_command("true");

    assert_eq!(frame["original_tokens"], 0);
    assert_eq!(frame["compressed_tokens"], 0);
    assert_eq!(frame["tokens_skipped"], false);
}
