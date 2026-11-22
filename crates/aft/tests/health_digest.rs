use std::io::Write;
use std::process::{Command, Stdio};

use aft::commands::health_digest::HEALTH_DIGEST_OPERATION;

fn canonical_operation() -> &'static str {
    include_str!("fixtures/health_digest/canonical-operation.txt").trim()
}

#[test]
fn canonical_name_fixture_matches_the_management_operation() {
    assert_eq!(HEALTH_DIGEST_OPERATION, canonical_operation());

    let main_source = include_str!("../src/main.rs");
    assert_eq!(
        main_source
            .matches("aft::commands::health_digest::HEALTH_DIGEST_OPERATION =>")
            .count(),
        1,
        "health.digest must have one canonical dispatch registration"
    );
    assert!(main_source.contains("aft::commands::health_digest::handle_health_digest(&req, ctx)"));
}

#[test]
fn health_digest_is_absent_from_agent_tool_registries_and_descriptions() {
    let agent_tool_surfaces = [
        include_str!("../../aft-subc/src/subc_tool_schemas.json"),
        include_str!("../../aft-subc/src/subc/manifest.rs"),
        include_str!("../../../packages/opencode-plugin/src/tool-registration.ts"),
        include_str!("../../../packages/pi-plugin/src/tool-registration.ts"),
    ];

    for surface in agent_tool_surfaces {
        assert!(
            !surface.contains(canonical_operation()),
            "health.digest must not be added to an agent tool surface"
        );
    }
}

fn aft_binary() -> std::path::PathBuf {
    crate::test_helpers::aft_binary::source_binary()
}

#[test]
fn management_operation_dispatches_without_agent_text() {
    let cache_dir = tempfile::tempdir().expect("create isolated cache directory");
    let mut child = Command::new(aft_binary())
        .env("AFT_CACHE_DIR", cache_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start aft");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(b"{\"id\":\"digest-dispatch\",\"command\":\"health.digest\"}\n")
        .expect("send digest request");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for aft");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("aft stdout is utf-8");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("digest response");
    assert_eq!(
        response,
        serde_json::json!({ "id": "digest-dispatch", "success": true })
    );
    assert!(!stdout.contains("health.digest"));
    assert!(!stdout.contains("text"));
}

#[test]
fn implementation_does_not_touch_alert_or_observation_state() {
    let implementation = include_str!("../../aft-tools/src/commands/health_digest.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("production implementation precedes its unit tests");

    for state_name in [
        "baseline_established",
        "alert_records",
        "observation_record",
        "pending_alert",
        "rendered",
        "live",
    ] {
        assert!(
            !implementation.contains(state_name),
            "digest must remain isolated from alert state: {state_name}"
        );
    }
}
