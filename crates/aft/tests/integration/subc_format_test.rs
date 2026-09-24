#![cfg(unix)]

//! Cross-language parity gate for subc native responses -> agent-facing text.
//!
//! Feeds the golden fixtures captured from the current TypeScript OpenCode tool
//! wrappers (`scripts/capture-subc-parity.ts`) through `aft::subc_format` and
//! asserts the rendered text matches byte-for-byte.

use std::fs;
use std::path::{Path, PathBuf};

use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde::Deserialize;
use serde_json::Value;

const PROJECT_ROOT_TOKEN: &str = "<PROJECT_ROOT>";
const HOME_ROOT_TOKEN: &str = "<HOME>";

#[derive(Debug, Deserialize)]
struct FormatFixture {
    tool_name: String,
    native_response_json: Value,
    ctx: FormatFixtureContext,
}

#[derive(Debug, Deserialize)]
struct FormatFixtureContext {
    agent_args: Value,
    project_root: String,
}

fn fixtures_root() -> PathBuf {
    crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("subc_parity")
        .join("format")
}

fn setup_project_fixture(root: &Path) {
    if root.exists() {
        return;
    }
    fs::create_dir_all(root.join("src")).expect("create src fixture dir");
    fs::write(root.join("src/main.ts"), "const value = 1;\n").expect("write main fixture");
}

/// The fixture project lives in this test's private scratch dir, so concurrent
/// runs cannot share it and it is removed when the test ends. The goldens
/// store it as a token, so its exact location does not matter.
fn fixture_project_root() -> PathBuf {
    let root = crate::helpers::thread_scratch_dir()
        .join("aft-subc-parity")
        .join("project");
    setup_project_fixture(&root);
    root
}

fn project_root_for_input(raw: &str) -> PathBuf {
    if raw == PROJECT_ROOT_TOKEN {
        fixture_project_root()
    } else {
        PathBuf::from(raw)
    }
}

fn replace_project_root(text: String, project_root: &Path) -> String {
    text.replace(
        &project_root.to_string_lossy().to_string(),
        PROJECT_ROOT_TOKEN,
    )
}

fn expand_stable_path_tokens(value: Value, project_root: &Path) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| expand_stable_path_tokens(item, project_root))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                out.insert(key, expand_stable_path_tokens(value, project_root));
            }
            Value::Object(out)
        }
        Value::String(s) => {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_default();
            Value::String(
                s.replace(PROJECT_ROOT_TOKEN, &project_root.to_string_lossy())
                    .replace(HOME_ROOT_TOKEN, &home),
            )
        }
        other => other,
    }
}

fn response_from_flattened(value: Value) -> Response {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("native_response_json must be an object"));
    let id = obj
        .get("id")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "fixture".to_string());
    let success = obj.get("success").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut data = serde_json::Map::new();
    for (key, value) in obj {
        if key != "id" && key != "success" {
            data.insert(key.clone(), value.clone());
        }
    }
    Response {
        id,
        success,
        data: Value::Object(data),
    }
}

fn assert_case(dir: &Path) -> Option<String> {
    let case = dir.file_name().unwrap().to_string_lossy().to_string();
    let input: FormatFixture =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).expect("read input.json"))
            .expect("parse input.json");
    let project_root = project_root_for_input(&input.ctx.project_root);

    let native_response_json = expand_stable_path_tokens(input.native_response_json, &project_root);
    let agent_args = expand_stable_path_tokens(input.ctx.agent_args, &project_root);
    let response = response_from_flattened(native_response_json);
    let ctx = FormatContext::from_tool_call(&input.tool_name, &agent_args, &project_root);
    let actual = replace_project_root(
        format_response_with_context(&input.tool_name, &response, &ctx),
        &project_root,
    );
    let expected = fs::read_to_string(dir.join("expected.txt")).expect("read expected.txt");
    if actual == expected {
        None
    } else {
        Some(format!(
            "case `{case}`:\n  actual:\n{actual}\n  expected:\n{expected}"
        ))
    }
}

#[test]
fn subc_format_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 20,
        "expected >=20 format parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} format parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn bash_error_renders_as_text_not_raw_json() {
    // The MCP e2e probe caught a denied bash call returning the raw response
    // JSON as its text content because "bash" was missing from the formatted
    // tool set. Every bash render (deny, deferred result, promotion) must
    // produce prose, never a serialized envelope.
    let response = Response::error(
        "probe",
        "bash_denied_untrusted",
        "remote/MCP-facade binds cannot run shell commands",
    );
    let text = format_response_with_context("bash", &response, &FormatContext::default());
    assert_eq!(text, "remote/MCP-facade binds cannot run shell commands");
    assert!(
        !text.contains("\"success\""),
        "bash error text leaked raw JSON: {text}"
    );
}

#[test]
fn inspect_phase_failed_text_preserves_failure_details_and_retry_guidance() {
    let response = Response {
        id: "inspect".to_string(),
        success: false,
        data: serde_json::json!({
            "inspect_terminal": "phase_failed",
            "completed_phases": [{"id": "stat_verification"}],
            "failure_reason": "inspect_not_fresh",
            "failure_detail": "metrics did not complete",
            "text": "request failed"
        }),
    };

    let text = format_response_with_context("inspect", &response, &FormatContext::default());
    assert!(
        text.contains("inspect could not complete: metrics did not complete (inspect_not_fresh).")
    );
    assert!(text.contains("Completed phases: 1. Retry, or narrow with sections=..."));
    assert!(
        !text.contains("request failed"),
        "generic failure leaked into text: {text}"
    );
}

#[test]
fn inspect_interrupted_text_names_the_terminal_and_safe_retry() {
    let response = Response {
        id: "inspect".to_string(),
        success: false,
        data: serde_json::json!({
            "inspect_terminal": "interrupted",
            "completed_phases": [{"id": "lsp_quiescence"}],
            "text": "request failed"
        }),
    };

    let text = format_response_with_context("inspect", &response, &FormatContext::default());
    assert!(text.contains("inspect was interrupted"));
    assert!(text.contains("Retry is safe"));
    assert!(
        !text.contains("request failed"),
        "generic failure leaked into text: {text}"
    );
}

#[test]
fn callgraph_format_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("callgraph_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 10,
        "expected >=10 callgraph format parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} callgraph format parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn zoom_format_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("zoom_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 7,
        "expected >=7 zoom format parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} zoom format parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn safety_format_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("safety_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 7,
        "expected >=7 safety format parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} safety format parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
