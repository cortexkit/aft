//! Integration tests for LSP-enhanced symbol disambiguation.

use super::helpers::{fixture_path, AftProcess};

/// edit_symbol with matching lsp_hints resolves the ambiguous symbol to a single result.
#[test]
fn edit_symbol_with_lsp_hints_disambiguates() {
    let mut aft = AftProcess::spawn();

    // Copy fixture to temp dir so we don't mutate the original
    let fixture = fixture_path("ambiguous.ts");
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ambiguous.ts");
    std::fs::copy(&fixture, &target).unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        dir.path().display()
    ));
    assert_eq!(resp["success"], true, "configure failed: {:?}", resp);

    // The fixture has two "process" symbols:
    //   - line 2 (0-indexed): standalone function (lines 2-4)
    //   - line 7 (0-indexed): method inside DataHandler (lines 7-9)
    // Send edit_symbol with lsp_hints pointing to the standalone function (line 3 is within range).
    let resp = aft.send(&format!(
        r#"{{"id":"lsp-1","command":"edit_symbol","file":"{}","symbol":"process","operation":"replace","content":"export function process(data: string): string {{\n  return data.toLowerCase();\n}}","lsp_hints":{{"symbols":[{{"name":"process","file":"{}","line":2}}]}}}}"#,
        target.display(),
        target.display()
    ));

    // Should succeed — not ambiguous_symbol
    assert_eq!(resp["success"], true, "expected success, got: {:?}", resp);
    assert_eq!(resp["symbol"], "process");

    aft.shutdown();
}

/// edit_symbol without lsp_hints returns ambiguous_symbol candidates for the same fixture.
#[test]
fn edit_symbol_without_lsp_hints_returns_candidates() {
    let mut aft = AftProcess::spawn();

    let fixture = fixture_path("ambiguous.ts");
    let dir = fixture.parent().unwrap().parent().unwrap();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        dir.display()
    ));
    assert_eq!(resp["success"], true, "configure failed: {:?}", resp);

    let resp = aft.send(&format!(
        r#"{{"id":"no-hints","command":"edit_symbol","file":"{}","symbol":"process","operation":"replace","content":"export function process(data: string): string {{\n  return data.toLowerCase();\n}}"}}"#,
        fixture.display()
    ));

    // Should return ambiguous_symbol with candidates
    assert_eq!(
        resp["code"], "ambiguous_symbol",
        "expected ambiguous, got: {:?}",
        resp
    );
    assert!(
        resp["candidates"].is_array(),
        "expected candidates array: {:?}",
        resp
    );
    assert!(
        resp["candidates"].as_array().unwrap().len() >= 2,
        "expected >= 2 candidates"
    );

    aft.shutdown();
}

/// edit_symbol with malformed lsp_hints falls back to returning candidates (not a hard error).
#[test]
fn edit_symbol_with_malformed_lsp_hints_falls_back() {
    let mut aft = AftProcess::spawn_with_stderr();

    let fixture = fixture_path("ambiguous.ts");
    let dir = fixture.parent().unwrap().parent().unwrap();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        dir.display()
    ));
    assert_eq!(resp["success"], true, "configure failed: {:?}", resp);

    // Malformed: lsp_hints is not the expected schema
    let resp = aft.send(&format!(
        r#"{{"id":"bad-hints","command":"edit_symbol","file":"{}","symbol":"process","operation":"replace","content":"export function process(data: string): string {{\n  return data.toLowerCase();\n}}","lsp_hints":{{"not_symbols":true}}}}"#,
        fixture.display()
    ));

    // Should fall back to ambiguous_symbol — malformed hints are silently ignored
    assert_eq!(
        resp["code"], "ambiguous_symbol",
        "expected ambiguous fallback, got: {:?}",
        resp
    );

    let (status, stderr) = aft.stderr_output();
    assert!(status.success());
    assert!(
        stderr.contains("ignoring malformed data"),
        "expected malformed warning in stderr, got: {}",
        stderr
    );
}

/// zoom with matching lsp_hints resolves to a single result.
#[test]
fn zoom_with_lsp_hints_disambiguates() {
    let mut aft = AftProcess::spawn();

    let fixture = fixture_path("ambiguous.ts");
    let dir = fixture.parent().unwrap().parent().unwrap();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        dir.display()
    ));
    assert_eq!(resp["success"], true, "configure failed: {:?}", resp);

    // Zoom into the method version (line 7, inside DataHandler)
    let resp = aft.send(&format!(
        r#"{{"id":"zoom-lsp","command":"zoom","file":"{}","symbol":"process","lsp_hints":{{"symbols":[{{"name":"process","file":"{}","line":7}}]}}}}"#,
        fixture.display(),
        fixture.display()
    ));

    // Should succeed with the method, not an ambiguous error
    assert_eq!(
        resp["success"], true,
        "expected zoom success, got: {:?}",
        resp
    );
    assert_eq!(resp["name"], "process");

    aft.shutdown();
}
