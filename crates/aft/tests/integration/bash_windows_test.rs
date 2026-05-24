//! Windows-specific bash behavior. These tests run as part of the standard
//! `cargo test` suite, but their bodies are gated to `#[cfg(target_os = "windows")]`
//! so they only execute on Windows builders. On Unix builders they compile to
//! empty test bodies and pass trivially.
//!
//! Why this file exists separately from `bash_test.rs`:
//!   - The Windows shell is `powershell.exe`, not `/bin/sh`. Process spawn
//!     overhead is materially higher (typically 200-2000ms cold, vs <50ms on
//!     Unix). Issue #26 reported bash timing out at 65s; the bridge transport
//!     timeout is `max(30s, requested+5s)`, which leaves only 5s of headroom
//!     for spawn + initial read on top of the requested timeout.
//!   - PowerShell quoting differs from sh; verify our shell_command() shape
//!     handles common command syntax without escaping bugs.
//!   - The blocked_env_var() check uses case-insensitive comparison ONLY on
//!     Windows (per #[cfg(windows)] in commands/bash.rs:183) — this test
//!     locks that contract in.

#![allow(unused_imports)]

use super::helpers::AftProcess;
use serde_json::json;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Sanity check: a trivial echo on Windows must complete well under the
/// transport timeout budget. If this regresses to multiple seconds, it
/// would explain the issue #26 class of failures (every bash call paying
/// real cost just for spawn overhead).
#[test]
#[cfg(target_os = "windows")]
fn windows_bash_echo_completes_quickly() {
    let mut aft = AftProcess::spawn();

    let started = Instant::now();
    let frames = aft.send_until(
        r#"{"id":"win-bash-1","method":"bash","params":{"command":"Write-Output hello"}}"#,
        |value| value["id"] == "win-bash-1",
    );
    let elapsed = started.elapsed();

    let response = frames
        .iter()
        .find(|frame| frame["id"] == "win-bash-1")
        .expect("expected response for win-bash-1");
    assert_eq!(response["success"], true, "bash command should succeed");

    // Be generous — Windows runners (esp. GH Actions) can be slow under load.
    // The point isn't to assert a specific upper bound; it's to catch a
    // regression where echo takes 30s+ (which is the issue #26 failure mode).
    assert!(
        elapsed < Duration::from_secs(15),
        "trivial echo took {}ms — investigate Windows bash spawn overhead",
        elapsed.as_millis()
    );
}

/// PowerShell + cmd-style command separators should both work. The shell
/// command wrapper uses `-Command` which expects PowerShell syntax; this
/// guards against accidentally switching to `cmd.exe /c` and breaking
/// existing user invocations.
#[test]
#[cfg(target_os = "windows")]
fn windows_bash_handles_powershell_pipe() {
    let mut aft = AftProcess::spawn();

    let frames = aft.send_until(
        r#"{"id":"win-pipe","method":"bash","params":{"command":"Write-Output hello | Select-String hello"}}"#,
        |value| value["id"] == "win-pipe",
    );

    let response = frames
        .iter()
        .find(|frame| frame["id"] == "win-pipe")
        .expect("expected response for win-pipe");
    assert_eq!(response["success"], true);
    let output = response["data"]["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("hello"),
        "expected pipe output to contain 'hello', got: {output}"
    );
}

/// Issue #26 protection: a 30-second sleep with a 60-second user-timeout
/// must complete cleanly without the bridge transport timing out first.
/// Transport budget is max(30s, 60s+5s) = 65s, leaving 35s of headroom past
/// the actual sleep. This is the canary for "bridge gives up before bash
/// returns" on Windows.
///
/// Marked `#[ignore]` by default because it deliberately runs for ~30s.
/// CI runs it explicitly via `cargo test -- --ignored bash_windows`.
#[test]
#[ignore]
#[cfg(target_os = "windows")]
fn windows_bash_30s_sleep_completes_under_transport_budget() {
    let mut aft = AftProcess::spawn();

    let started = Instant::now();
    let frames = aft.send_until(
        r#"{"id":"win-sleep-30","method":"bash","params":{"command":"Start-Sleep -Seconds 30; Write-Output done","timeout":60000}}"#,
        |value| value["id"] == "win-sleep-30",
    );
    let elapsed = started.elapsed();

    let response = frames
        .iter()
        .find(|frame| frame["id"] == "win-sleep-30")
        .expect("expected response for win-sleep-30");

    // Bridge must NOT have given up — success=true is the contract here.
    // If this fails with a transport timeout, increase the headroom in
    // bridge.ts transportTimeoutMs OR investigate Windows spawn overhead.
    assert_eq!(
        response["success"], true,
        "30s sleep with 60s user-timeout must complete (transport budget = 65s)"
    );

    // Sleep was ~30s; full round-trip should be well under 60s. If we're
    // taking 50+ seconds, spawn overhead is eating headroom that's supposed
    // to absorb timeout slack.
    assert!(
        elapsed < Duration::from_secs(50),
        "30s sleep took {}s round-trip — Windows spawn overhead is dangerous",
        elapsed.as_secs()
    );
}

/// Blocked env vars are matched case-insensitively on Windows only (Windows
/// env var names are case-insensitive in practice). This locks the contract
/// in — we don't want a future change that "fixes" the case sensitivity
/// inconsistency without realizing it breaks Windows.
#[test]
#[cfg(target_os = "windows")]
fn windows_bash_blocks_path_env_case_insensitively() {
    let mut aft = AftProcess::spawn();

    // The `path` env var (lowercase) must be blocked on Windows even though
    // the canonical blocklist entry is `PATH`. Sending it should produce a
    // permission/error response, not let the user override PATH silently.
    let frames = aft.send_until(
        r#"{"id":"win-env-block","method":"bash","params":{"command":"Write-Output ok","env":{"path":"C:\\evil"}}}"#,
        |value| value["id"] == "win-env-block",
    );

    let response = frames
        .iter()
        .find(|frame| frame["id"] == "win-env-block")
        .expect("expected response for win-env-block");

    // Either an error OR a successful response with the env var rejected.
    // The exact shape may vary as bash_permissions evolves; what matters
    // is that "path" was treated as a blocked variable.
    if response["success"] == true {
        // Expect the response to surface the blocked var somehow.
        let stringified = response.to_string();
        assert!(
            stringified.to_lowercase().contains("path")
                || stringified.to_lowercase().contains("blocked"),
            "expected blocked env var to surface in response: {stringified}"
        );
    } else {
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.to_lowercase().contains("path") || message.to_lowercase().contains("blocked"),
            "expected error message to mention blocked env var, got: {message}"
        );
    }
}

#[test]
#[cfg(target_os = "windows")]
fn windows_cmd_background_wrapper_allows_bang_in_path() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("project!bang");
    std::fs::create_dir_all(&project).unwrap();
    let storage = parent.path().join("storage!bang");
    std::fs::create_dir_all(&storage).unwrap();
    let mut aft = AftProcess::spawn();

    let cfg = aft.send(
        &json!({
            "id":"cfg-win-bang",
            "command":"configure",
            "harness":"opencode",
            "project_root": project,
            "storage_dir": storage,
            "experimental_bash_background": true,
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure failed: {cfg:?}");

    let spawned = aft.send(
        &json!({
            "id":"spawn-win-bang",
            "command":"bash",
            "params":{"command":"cmd /D /C exit /B 0","background":true}
        })
        .to_string(),
    );
    assert_eq!(spawned["success"], true, "spawn failed: {spawned:?}");
    let task_id = spawned["task_id"].as_str().unwrap();

    let started = Instant::now();
    loop {
        let status = aft.send(
            &json!({
                "id":"status-win-bang",
                "command":"bash_status",
                "params":{"task_id":task_id}
            })
            .to_string(),
        );
        if status["status"] == "completed" {
            assert_eq!(status["exit_code"], 0);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(aft.shutdown().success());
}

/// Compile-time stub for non-Windows builds so this file isn't empty in
/// the test binary on Linux/macOS (avoids dead-test-file warnings).
#[test]
#[cfg(not(target_os = "windows"))]
fn windows_bash_tests_skipped_on_non_windows() {
    // Intentionally trivial — tests in this file only run on Windows.
}
