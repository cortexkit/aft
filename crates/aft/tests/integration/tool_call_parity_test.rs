use std::fs;
use std::path::Path;

use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde_json::{json, Map, Value};

use super::helpers::AftProcess;

const SESSION_ID: &str = "tool-call-parity-session";

struct ParityCase {
    label: &'static str,
    tool: &'static str,
    arguments: Value,
}

#[derive(Clone, Copy)]
enum GlobMtimeProfile {
    Distinct,
    Equal,
}

impl GlobMtimeProfile {
    fn expected_text(self) -> &'static str {
        match self {
            Self::Distinct => {
                "5 files matching **/*.txt\n\nsrc/read.txt\nsrc/edit_ambiguous.txt\nsrc/search.txt\nsrc/edit.txt\nsrc/patch.txt"
            }
            Self::Equal => {
                "5 files matching **/*.txt\n\nsrc/edit.txt\nsrc/edit_ambiguous.txt\nsrc/patch.txt\nsrc/read.txt\nsrc/search.txt"
            }
        }
    }
}

#[test]
fn tool_call_matches_direct_spine_envelopes() {
    run_tool_call_parity(GlobMtimeProfile::Distinct, parity_cases());
}

#[test]
fn tool_call_glob_matches_direct_spine_with_equal_mtimes() {
    run_tool_call_parity(GlobMtimeProfile::Equal, vec![glob_parity_case()]);
}

fn run_tool_call_parity(glob_mtimes: GlobMtimeProfile, cases: Vec<ParityCase>) {
    let direct_project = tempfile::tempdir().expect("direct temp project");
    let tool_call_project = tempfile::tempdir().expect("tool_call temp project");
    create_fixture_project(direct_project.path(), glob_mtimes);
    create_fixture_project(tool_call_project.path(), glob_mtimes);

    let mut direct_aft = AftProcess::spawn();
    let mut tool_call_aft = AftProcess::spawn();
    configure_project(&mut direct_aft, direct_project.path(), "cfg-direct");
    configure_project(
        &mut tool_call_aft,
        tool_call_project.path(),
        "cfg-tool-call",
    );

    for case in cases {
        let request_id = format!("tool-call-parity-{}", case.label);
        let direct_request = direct_request(
            &request_id,
            case.tool,
            &case.arguments,
            direct_project.path(),
        );
        let tool_call_request = json!({
            "id": request_id,
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": case.tool,
            "arguments": case.arguments,
        });

        let direct_response = send_json(&mut direct_aft, direct_request);
        let tool_call_response = send_json(&mut tool_call_aft, tool_call_request);
        if case.label == "status_snapshot" {
            assert_lifecycle_census_presence(&direct_response);
            assert_lifecycle_census_presence(&tool_call_response);
        }

        assert_eq!(
            direct_response["success"], tool_call_response["success"],
            "success mismatch for {}: direct={direct_response:#} tool_call={tool_call_response:#}",
            case.label,
        );
        assert_eq!(
            direct_response["success"].as_bool().map(|success| !success),
            tool_call_response["success"]
                .as_bool()
                .map(|success| !success),
            "derived is_error mismatch for {}",
            case.label
        );
        assert_eq!(
            direct_response.get("code"),
            tool_call_response.get("code"),
            "error code mismatch for {}",
            case.label
        );
        if case.label == "edit_ambiguous_match" {
            assert_eq!(direct_response["code"], "ambiguous_match");
            let message = direct_response["message"].as_str().unwrap_or_default();
            assert!(message.contains("occurrence"));
            assert!(message.contains("1-based"));
            assert!(!message.contains("0-based"));
            assert!(!message.contains("0-indexed"));
            let occurrences = direct_response["occurrences"]
                .as_array()
                .expect("ambiguous edit occurrences");
            assert_eq!(occurrences[0]["occurrence"], 1);
            assert_eq!(occurrences[1]["occurrence"], 2);
        }

        let expected_text = formatted_text_from_direct_response(
            case.tool,
            &case.arguments,
            direct_project.path(),
            &direct_response,
        );
        let actual_text = tool_call_response["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool_call response missing text for {}", case.label));
        let normalized_expected_text = normalize_text(
            &expected_text,
            direct_project.path(),
            direct_aft.cache_dir(),
        );
        let normalized_actual_text = normalize_text(
            actual_text,
            tool_call_project.path(),
            tool_call_aft.cache_dir(),
        );
        assert_eq!(
            normalized_expected_text, normalized_actual_text,
            "formatted text mismatch for {}",
            case.label
        );
        if case.label == "glob_matches" {
            assert_eq!(
                normalized_actual_text.replace('\\', "/"),
                glob_mtimes.expected_text()
            );
        }

        let mut expected_envelope = direct_response;
        expected_envelope
            .as_object_mut()
            .expect("direct response is object")
            .insert("text".to_string(), Value::String(expected_text));

        let expected_envelope = normalized_envelope(
            expected_envelope,
            direct_project.path(),
            direct_aft.cache_dir(),
        );
        let actual_envelope = normalized_envelope(
            tool_call_response,
            tool_call_project.path(),
            tool_call_aft.cache_dir(),
        );
        assert_eq!(
            expected_envelope, actual_envelope,
            "full envelope mismatch for {}",
            case.label
        );
    }

    assert!(direct_aft.shutdown().success());
    assert!(tool_call_aft.shutdown().success());
}

#[test]
fn standalone_tool_call_carries_hashline_registration_for_later_sessions() {
    let project = tempfile::tempdir().expect("hashline carrier temp project");
    let root = std::fs::canonicalize(project.path()).expect("canonical project");
    let file = root.join("hashline-carrier.txt");
    fs::write(&file, "alpha\nbeta\n").expect("write hashline carrier fixture");
    let mut aft = AftProcess::spawn();

    let configure = send_json(
        &mut aft,
        json!({
            "id": "hashline-carrier-configure",
            "command": "configure",
            "session_id": "configure-owner",
            "project_root": root,
            "harness": "opencode",
            "edit_slot_survives": true,
            "config": [{
                "tier": "project",
                "source": root.join(".cortexkit/aft.jsonc"),
                "doc": json!({
                    "edit_mode": "hashline",
                    "search_index": false,
                    "semantic_search": false
                }).to_string()
            }]
        }),
    );
    assert!(
        configure["success"].as_bool().unwrap_or(false),
        "{configure:#}"
    );

    let read = send_json(
        &mut aft,
        json!({
            "id": "hashline-carrier-read",
            "command": "tool_call",
            "session_id": "later-hashline-session",
            "edit_slot_survives": true,
            "name": "read",
            "arguments": { "path": file }
        }),
    );
    assert!(read["success"].as_bool().unwrap_or(false), "{read:#}");
    assert!(read["hashline_tag"].as_str().is_some(), "{read:#}");
    assert!(read["text"].as_str().unwrap_or_default().starts_with('['));

    let downgraded = send_json(
        &mut aft,
        json!({
            "id": "hashline-carrier-downgrade",
            "command": "tool_call",
            "session_id": "later-default-session",
            "edit_slot_survives": false,
            "name": "edit",
            "arguments": {
                "path": file,
                "edits": [{ "oldString": "alpha", "newString": "omega" }]
            }
        }),
    );
    assert!(
        downgraded["success"].as_bool().unwrap_or(false),
        "{downgraded:#}"
    );
    assert_eq!(downgraded["warnings"][0]["code"], "hashline_downgraded");
    assert!(downgraded["text"]
        .as_str()
        .unwrap_or_default()
        .contains("Hashline mode was downgraded"));

    let repeat = send_json(
        &mut aft,
        json!({
            "id": "hashline-carrier-downgrade-repeat",
            "command": "tool_call",
            "session_id": "later-default-session",
            "edit_slot_survives": false,
            "name": "read",
            "arguments": { "path": file }
        }),
    );
    assert!(repeat["success"].as_bool().unwrap_or(false), "{repeat:#}");
    assert!(repeat.get("warnings").is_none(), "{repeat:#}");

    assert!(aft.shutdown().success());
}

#[test]
fn hashline_tool_call_activates_read_edit_undo_and_seamless_default_rebind() {
    let project = tempfile::tempdir().expect("hashline temp project");
    let root = std::fs::canonicalize(project.path()).expect("canonical project");
    let file = root.join("hashline.txt");
    fs::write(&file, "alpha\nbeta\n").expect("write hashline fixture");
    let mut aft = AftProcess::spawn();

    let configure = |aft: &mut AftProcess, id: &str, edit_mode: &str| {
        send_json(
            aft,
            json!({
                "id": id,
                "command": "configure",
                "session_id": SESSION_ID,
                "project_root": root,
                "harness": "opencode",
                "edit_slot_survives": true,
                "config": [{
                    "tier": "project",
                    "source": root.join(".cortexkit/aft.jsonc"),
                    "doc": json!({
                        "edit_mode": edit_mode,
                        "search_index": false,
                        "semantic_search": false
                    }).to_string()
                }]
            }),
        )
    };
    assert!(
        configure(&mut aft, "hashline-configure-on", "hashline")["success"]
            .as_bool()
            .unwrap_or(false)
    );

    let read = send_json(
        &mut aft,
        json!({
            "id": "hashline-read",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "read",
            "arguments": { "filePath": file }
        }),
    );
    assert!(read["success"].as_bool().unwrap_or(false), "{read:#}");
    let tag = read["hashline_tag"]
        .as_str()
        .expect("tool-call read must carry a hashline tag");
    assert!(read["text"].as_str().unwrap_or_default().starts_with('['));

    let patch = format!("*** Begin Patch\n[hashline.txt#{tag}]\nPUT 1:\n+omega\n*** End Patch");
    let edit = send_json(
        &mut aft,
        json!({
            "id": "hashline-edit",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": { "patch": patch }
        }),
    );
    assert!(edit["success"].as_bool().unwrap_or(false), "{edit:#}");
    assert_eq!(fs::read(&file).unwrap(), b"omega\nbeta\n");
    assert!(edit["op_id"].as_str().is_some());

    let legacy_rejection = send_json(
        &mut aft,
        json!({
            "id": "hashline-legacy-rejection",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "filePath": file,
                "oldString": "omega",
                "newString": "legacy"
            }
        }),
    );
    assert_eq!(legacy_rejection["code"], "hashline_parse_error");

    let undo = send_json(
        &mut aft,
        json!({
            "id": "hashline-undo",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "safety",
            "arguments": { "op": "undo" }
        }),
    );
    assert!(undo["success"].as_bool().unwrap_or(false), "{undo:#}");
    assert_eq!(fs::read(&file).unwrap(), b"alpha\nbeta\n");

    assert!(
        configure(&mut aft, "hashline-configure-off", "default")["success"]
            .as_bool()
            .unwrap_or(false)
    );
    let default_read = send_json(
        &mut aft,
        json!({
            "id": "default-read",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "read",
            "arguments": { "filePath": file }
        }),
    );
    assert!(default_read.get("hashline_tag").is_none());
    assert!(default_read["text"]
        .as_str()
        .unwrap_or_default()
        .starts_with("1: alpha"));
    let default_edit = send_json(
        &mut aft,
        json!({
            "id": "default-edit",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "filePath": file,
                "oldString": "alpha",
                "newString": "default"
            }
        }),
    );
    assert!(
        default_edit["success"].as_bool().unwrap_or(false),
        "{default_edit:#}"
    );
    assert_eq!(fs::read(&file).unwrap(), b"default\nbeta\n");

    assert!(aft.shutdown().success());
}

#[test]
fn plugin_harness_without_edit_registration_fails_safe_to_default_once() {
    let project = tempfile::tempdir().expect("unregistered plugin temp project");
    let root = std::fs::canonicalize(project.path()).expect("canonical project");
    let file = root.join("default.txt");
    fs::write(&file, "alpha\nbeta\n").expect("write default fixture");
    let mut aft = AftProcess::spawn();

    let configure = send_json(
        &mut aft,
        json!({
            "id": "unregistered-configure",
            "command": "configure",
            "session_id": SESSION_ID,
            "project_root": root,
            "harness": "opencode",
            "config": [{
                "tier": "project",
                "source": root.join(".cortexkit/aft.jsonc"),
                "doc": json!({
                    "edit_mode": "hashline",
                    "search_index": false,
                    "semantic_search": false
                }).to_string()
            }]
        }),
    );
    assert!(configure["success"].as_bool().unwrap_or(false));
    assert_eq!(configure["warnings"][0]["code"], "hashline_downgraded");

    let first = send_json(
        &mut aft,
        json!({
            "id": "unregistered-default-first",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "path": file,
                "edits": [{ "oldString": "alpha", "newString": "default" }]
            }
        }),
    );
    assert!(first["success"].as_bool().unwrap_or(false), "{first:#}");
    assert!(first.get("warnings").is_none(), "{first:#}");
    assert!(!first["text"]
        .as_str()
        .unwrap_or_default()
        .contains("Hashline mode was downgraded"));

    let second = send_json(
        &mut aft,
        json!({
            "id": "unregistered-default-second",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "path": file,
                "edits": [{ "oldString": "default", "newString": "still-default" }]
            }
        }),
    );
    assert!(second["success"].as_bool().unwrap_or(false), "{second:#}");
    assert!(second.get("warnings").is_none(), "{second:#}");
    assert!(!second["text"]
        .as_str()
        .unwrap_or_default()
        .contains("Hashline mode was downgraded"));

    let patch = send_json(
        &mut aft,
        json!({
            "id": "unregistered-patch",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": { "patch": "[default.txt#STALE]\nPUT 1:\n+blocked" }
        }),
    );
    assert!(!patch["success"].as_bool().unwrap_or(true), "{patch:#}");
    assert_ne!(patch["code"], "hashline_parse_error");
    assert_eq!(fs::read_to_string(file).unwrap(), "still-default\nbeta\n");

    assert!(aft.shutdown().success());
}

#[test]
fn known_tool_translate_errors_surface_as_invalid_request() {
    let mut aft = AftProcess::spawn();
    for (label, name, arguments, expected_message) in [
        (
            "callgraph-missing-op",
            "callgraph",
            json!({}),
            "'op' is required",
        ),
        (
            "zoom-mutually-exclusive-targets",
            "zoom",
            json!({"filePath": "src/main.ts", "url": "https://example.com/doc", "symbols": "run"}),
            "Provide exactly ONE of 'filePath' or 'url'",
        ),
        (
            "apply-patch-missing-patch-text",
            "apply_patch",
            json!({}),
            "apply_patch: missing required param 'patchText'",
        ),
    ] {
        let response = send_json(
            &mut aft,
            json!({
                "id": format!("tool-call-{label}"),
                "command": "tool_call",
                "session_id": SESSION_ID,
                "name": name,
                "arguments": arguments,
            }),
        );
        assert_eq!(response["success"], false, "expected failure: {response:#}");
        assert_eq!(
            response["code"], "invalid_request",
            "translation errors for known tools must not fall through to raw dispatch: {response:#}"
        );
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_message),
            "message should include {expected_message:?}: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn edit_contract_translation_preserves_exact_code_then_message() {
    let mut aft = AftProcess::spawn();
    let cases = [
        (
            "no-mode",
            json!({"path": "src/main.ts"}),
            "edit: exactly one of `appendContent`, `edits`, or `symbol` plus `content` is required. Omit unused optional fields entirely; do not send empty strings or empty arrays for them.",
        ),
        (
            "top-level-start-line",
            json!({"path": "src/main.ts", "startLine": 1}),
            "edit: top-level 'startLine' are invalid; line-range fields are valid only inside 'edits[]'. Use edits: [{ startLine, endLine, content }].",
        ),
        (
            "occurrence-zero",
            json!({"path": "src/main.ts", "edits": [{"oldString": "value", "occurrence": 0}]}),
            "edit: edits[0].occurrence must be a positive integer",
        ),
        (
            "replace-all-occurrence-conflict",
            json!({"path": "src/main.ts", "edits": [{"oldString": "value", "replaceAll": true, "occurrence": 1}]}),
            "edit: edits[0] cannot contain both 'replaceAll' and 'occurrence'",
        ),
    ];

    for (label, arguments, expected_message) in cases {
        let response = send_json(
            &mut aft,
            json!({
                "id": format!("tool-call-edit-contract-{label}"),
                "command": "tool_call",
                "session_id": SESSION_ID,
                "name": "edit",
                "arguments": arguments,
            }),
        );
        assert_eq!(response["success"], false, "expected failure: {response:#}");
        assert_eq!(
            response["code"], "invalid_request",
            "wrong code: {response:#}"
        );
        assert_eq!(
            response["message"], expected_message,
            "normalized contract message drifted for {label}: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn edit_tool_call_applies_edits_with_empty_optional_mode_sentinels() {
    let project = tempfile::tempdir().expect("tool_call edit temp project");
    let target = project.path().join("src/example.ts");
    fs::create_dir_all(target.parent().expect("example parent")).expect("create src directory");
    fs::write(&target, "const value = old;\n").expect("write edit fixture");

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, project.path(), "cfg-edit-empty-sentinels");
    let response = send_json(
        &mut aft,
        json!({
            "id": "tool-call-edit-empty-sentinels",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "filePath": "src/example.ts",
                "edits": [{ "oldString": "old", "newString": "new" }],
                "appendContent": "",
                "symbol": "",
                "content": "",
            },
        }),
    );

    assert_eq!(response["success"], true, "edit failed: {response:#}");
    assert_eq!(
        fs::read_to_string(target).expect("read edited fixture"),
        "const value = new;\n"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn edit_tool_call_applies_find_replace_with_empty_line_range_sentinels() {
    let project = tempfile::tempdir().expect("tool_call find/replace sentinel temp project");
    let target = project.path().join("src/example.ts");
    fs::create_dir_all(target.parent().expect("example parent")).expect("create src directory");
    fs::write(&target, "const value = before;\n").expect("write edit fixture");

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, project.path(), "cfg-edit-find-range-sentinels");
    let response = send_json(
        &mut aft,
        json!({
            "id": "tool-call-edit-find-range-sentinels",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "filePath": "src/example.ts",
                "edits": [{
                    "oldString": "before",
                    "newString": "after",
                    "startLine": 1,
                    "endLine": 1,
                    "content": "",
                }],
            },
        }),
    );

    assert_eq!(response["success"], true, "edit failed: {response:#}");
    assert_eq!(
        fs::read_to_string(target).expect("read edited fixture"),
        "const value = after;\n"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn edit_tool_call_applies_line_range_edit_with_find_replace_sentinels() {
    let project = tempfile::tempdir().expect("tool_call line-range sentinel temp project");
    let target = project.path().join("src/example.ts");
    fs::create_dir_all(target.parent().expect("example parent")).expect("create src directory");
    let original = (1..14)
        .map(|line| format!("line {line}\n"))
        .chain(std::iter::once("const value = old;\n".to_string()))
        .collect::<String>();
    fs::write(&target, original).expect("write edit fixture");

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, project.path(), "cfg-edit-line-range-sentinels");
    let response = send_json(
        &mut aft,
        json!({
            "id": "tool-call-edit-line-range-sentinels",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "edit",
            "arguments": {
                "filePath": "src/example.ts",
                "edits": [{
                    "content": "const value = new;",
                    "startLine": 14,
                    "endLine": 14,
                    "oldString": "",
                    "newString": "",
                    "replaceAll": false,
                    "occurrence": 1,
                }],
            },
        }),
    );

    assert_eq!(response["success"], true, "edit failed: {response:#}");
    assert_eq!(
        fs::read_to_string(target).expect("read edited fixture"),
        (1..14)
            .map(|line| format!("line {line}\n"))
            .chain(std::iter::once("const value = new;\n".to_string()))
            .collect::<String>(),
    );
    assert!(aft.shutdown().success());
}

#[test]
fn unsupported_translate_tools_still_raw_dispatch_native_commands() {
    let project = tempfile::tempdir().expect("tool_call configure temp project");
    let mut aft = AftProcess::spawn();
    let response = send_json(
        &mut aft,
        json!({
            "id": "tool-call-native-configure",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "configure",
            "arguments": {
                "project_root": project.path().to_string_lossy(),
                "harness": "opencode",
                "config": crate::helpers::user_config(json!({
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false
                }))
            }
        }),
    );
    assert_eq!(
        response["success"], true,
        "configure raw dispatch failed: {response:#}"
    );
    assert!(
        response["text"].is_string(),
        "raw-dispatched native tool_call should still carry rendered text: {response:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn tool_call_rejects_missing_or_invalid_name() {
    let mut aft = AftProcess::spawn();
    for request in [
        json!({"id": "tool-call-missing-name", "command": "tool_call", "arguments": {}}),
        json!({"id": "tool-call-invalid-name", "command": "tool_call", "name": 7, "arguments": {}}),
    ] {
        let response = send_json(&mut aft, request);
        assert_eq!(response["success"], false, "expected failure: {response:#}");
        assert_eq!(
            response["code"], "invalid_request",
            "wrong code: {response:#}"
        );
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains("name"),
            "message should mention the invalid name field: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

fn glob_parity_case() -> ParityCase {
    ParityCase {
        label: "glob_matches",
        tool: "glob",
        arguments: json!({"pattern": "**/*.txt", "path": "src"}),
    }
}

fn parity_cases() -> Vec<ParityCase> {
    vec![
        ParityCase {
            label: "read_text_file",
            tool: "read",
            arguments: json!({"filePath": "src/read.txt"}),
        },
        ParityCase {
            label: "grep_matches",
            tool: "grep",
            arguments: json!({"pattern": "needle", "path": "src"}),
        },
        glob_parity_case(),
        ParityCase {
            label: "search_literal",
            tool: "search",
            arguments: json!({"query": "needle", "hint": "literal", "topK": 5}),
        },
        ParityCase {
            label: "search_empty_path_sentinel",
            tool: "search",
            arguments: json!({"query": "needle", "path": ""}),
        },
        ParityCase {
            label: "inspect_todos",
            tool: "inspect",
            arguments: json!({"sections": "todos", "scope": "src", "topK": 5}),
        },
        ParityCase {
            label: "status_snapshot",
            tool: "status",
            arguments: json!({}),
        },
        ParityCase {
            label: "write_create_file",
            tool: "write",
            arguments: json!({"filePath": "src/new.txt", "content": "created by tool_call parity\n"}),
        },
        ParityCase {
            label: "edit_replace_string",
            tool: "edit",
            arguments: json!({"filePath": "src/edit.txt", "oldString": "old", "newString": "new"}),
        },
        ParityCase {
            label: "edit_ambiguous_match",
            tool: "edit",
            arguments: json!({
                "filePath": "src/edit_ambiguous.txt",
                "edits": [{"oldString": "same", "newString": "new"}]
            }),
        },
        ParityCase {
            label: "apply_patch_update",
            tool: "apply_patch",
            arguments: json!({"patchText": "*** Begin Patch\n*** Update File: src/patch.txt\n@@\n-before\n+after\n*** End Patch"}),
        },
        ParityCase {
            label: "read_missing_file_error",
            tool: "read",
            arguments: json!({"filePath": "src/missing.txt"}),
        },
        ParityCase {
            label: "conflicts_not_git_repo_error",
            tool: "conflicts",
            arguments: json!({}),
        },
        ParityCase {
            label: "zoom_single_symbol",
            tool: "zoom",
            arguments: json!({"filePath": "src/zoom.ts", "symbols": "helper", "contextLines": 1}),
        },
        ParityCase {
            label: "zoom_multi_symbol_partial",
            tool: "zoom",
            arguments: json!({"filePath": "src/zoom.ts", "symbols": ["helper", "missingSymbol"]}),
        },
        ParityCase {
            label: "zoom_multi_target_all_success",
            tool: "zoom",
            arguments: json!({
                "targets": [
                    {"filePath": "src/zoom.ts", "symbol": "helper"},
                    {"filePath": "src/zoom_other.ts", "symbol": "otherHelper"}
                ],
                "callgraph": true
            }),
        },
        ParityCase {
            label: "safety_list_empty",
            tool: "safety",
            arguments: json!({"op": "list"}),
        },
    ]
}

fn create_fixture_project(root: &Path, glob_mtimes: GlobMtimeProfile) {
    fs::create_dir_all(root.join("src")).expect("create src dir");
    fs::create_dir_all(root.join("docs")).expect("create docs dir");
    fs::write(
        root.join("src/read.txt"),
        "alpha\nneedle in a haystack\nomega\n",
    )
    .expect("write read fixture");
    fs::write(root.join("src/search.txt"), "another needle\n").expect("write search fixture");
    fs::write(root.join("src/edit.txt"), "replace old value\n").expect("write edit fixture");
    fs::write(root.join("src/edit_ambiguous.txt"), "same same\n")
        .expect("write ambiguous edit fixture");
    fs::write(root.join("src/patch.txt"), "before\n").expect("write patch fixture");
    fs::write(
        root.join("src/todos.rs"),
        "// TODO: keep the parity fixture visible to inspect\nfn main() {}\n",
    )
    .expect("write todo fixture");
    fs::write(
        root.join("src/zoom.ts"),
        "export function helper(): string {\n  return 'ok';\n}\n\nexport function caller(): string {\n  return helper();\n}\n",
    )
    .expect("write zoom fixture");
    fs::write(
        root.join("src/zoom_other.ts"),
        "export function otherHelper(): string {\n  return 'other';\n}\n",
    )
    .expect("write zoom multi-target fixture");
    fs::write(root.join("docs/zoom.md"), "# Zoom Doc\n\nIntro line\n")
        .expect("write zoom docs fixture");

    // The parity processes use separate project directories, while glob's public
    // order depends on metadata. Give corresponding files the same profile so the
    // test compares dispatch routes rather than filesystem timestamp granularity.
    for (fixture, distinct_offset) in [
        ("src/read.txt", 500),
        ("src/edit_ambiguous.txt", 400),
        ("src/search.txt", 300),
        ("src/edit.txt", 200),
        ("src/patch.txt", 100),
    ] {
        let offset = match glob_mtimes {
            GlobMtimeProfile::Distinct => distinct_offset,
            GlobMtimeProfile::Equal => 0,
        };
        let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + offset, 0);
        filetime::set_file_mtime(root.join(fixture), mtime).expect("pin glob fixture mtime");
    }
}

fn configure_project(aft: &mut AftProcess, root: &Path, id: &str) {
    let response = send_json(
        aft,
        json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "config": crate::helpers::user_config(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            })),
        }),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn direct_request(id: &str, tool: &str, arguments: &Value, project_root: &Path) -> Value {
    let translated = subc_translate_with_context(
        tool,
        arguments,
        project_root,
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
            effective_hashline: false,
        },
    )
    .unwrap_or_else(|error| panic!("translate {tool} failed: {}", error.message));
    let mut request = translated.args;
    request.insert("id".to_string(), Value::String(id.to_string()));
    request.insert("command".to_string(), Value::String(translated.command));
    request.insert(
        "session_id".to_string(),
        Value::String(SESSION_ID.to_string()),
    );
    Value::Object(request)
}

fn send_json(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn formatted_text_from_direct_response(
    tool: &str,
    arguments: &Value,
    project_root: &Path,
    direct_response: &Value,
) -> String {
    let response = response_from_wire(direct_response);
    let context = FormatContext::from_tool_call(tool, arguments, project_root);
    format_response_with_context(tool, &response, &context)
}

fn response_from_wire(value: &Value) -> Response {
    let object = value.as_object().expect("response is object");
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let success = object
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut data = Map::new();
    for (key, value) in object {
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

fn normalized_envelope(mut value: Value, project_root: &Path, cache_dir: &Path) -> Value {
    normalize_value(&mut value, project_root, cache_dir);
    value
}

fn assert_lifecycle_census_presence(response: &Value) {
    for (section, fields) in [
        (
            "lsp",
            &[
                "children_total",
                "children_by_root",
                "children_without_client",
                "children_with_deleted_cwd",
            ][..],
        ),
        ("threads", &["total", "classified", "by_class"][..]),
        (
            "sqlite",
            &[
                "open_connections",
                "open_by_store",
                "uninstrumented_openers",
            ][..],
        ),
        ("children", &["detached_total"][..]),
        ("fds", &["open", "soft_limit"][..]),
    ] {
        let value = response.get(section).unwrap_or_else(|| {
            panic!("status response omitted lifecycle section {section}: {response:#}")
        });
        for field in fields {
            assert!(
                value.get(*field).is_some(),
                "status response omitted {section}.{field}: {response:#}"
            );
        }
    }
}

fn normalize_value(value: &mut Value, project_root: &Path, cache_dir: &Path) {
    match value {
        Value::String(text) => *text = normalize_text(text, project_root, cache_dir),
        Value::Array(items) => {
            for item in items {
                normalize_value(item, project_root, cache_dir);
            }
        }
        Value::Object(map) => {
            // Memory attribution is sampled independently in the direct and
            // tool_call processes. Preserve its schema/status fields while
            // masking every volatile byte/count number.
            if let Some(memory) = map.get_mut("memory") {
                mask_memory_snapshot(memory);
            }
            // Lifecycle counters are sampled by the health rollup independently
            // in each process. Mask VALUES while retaining the full nested
            // schema, so parity still fails if a required census field vanishes.
            mask_lifecycle_census(map);
            // These fields are intentionally volatile: grep reports wall-clock timing,
            // backup ids are per-operation identifiers, cache keys derive from the
            // temporary root path, and `tier2_last_run` is a wall-clock unix-seconds
            // stamp the inspect scanner sets independently in each process — the direct
            // and tool_call runs each trigger their own tier2 scan, so under CI load the
            // two stamps can straddle a one-second tick (same flake class as the
            // callgraph `indexed_at`/`updated_at` columns). The parity assertion keeps
            // every stable field intact.
            for key in [
                "search_ms",
                "backup_id",
                "project_cache_key",
                "tier2_last_run",
                // The artifact-owner block reports the cache key (derived from
                // the temp root path, differs per process) and the manifest
                // path (under the temp storage dir, outside root masking).
                "project_key",
                "manifest_path",
                "owner_project_scope_key",
                // Callgraph write instrumentation counts real commits inside a
                // rolling 60s window; the direct and tool_call processes each
                // run their own store setup, so under CI load their commit
                // counts and byte totals legitimately diverge. The FIELDS'
                // presence stays asserted (always-present-total contract);
                // only the sampled values are masked.
                "callgraph_commits_60s_total",
                "callgraph_pages_or_bytes_written_60s_total",
                "callgraph_commits_60s",
                "callgraph_pages_or_bytes_written_60s",
                "callgraph_repair_entries_60s_total",
                "callgraph_repair_entries_60s",
                // Semantic embedding advances independently in each parity
                // process. Preserve the progress schema while masking sampled
                // counts that may differ at the same status snapshot.
                "embedded_chunks",
                "total_chunks",
                "current_batch",
                "total_batches",
            ] {
                if map.contains_key(key) {
                    map.insert(key.to_string(), Value::String(format!("<{key}>")));
                }
            }
            for value in map.values_mut() {
                normalize_value(value, project_root, cache_dir);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn mask_lifecycle_census(map: &mut Map<String, Value>) {
    for key in ["lsp", "threads", "sqlite", "children", "fds"] {
        if let Some(value) = map.get_mut(key) {
            mask_lifecycle_numbers(value);
        }
    }
}

fn mask_lifecycle_numbers(value: &mut Value) {
    match value {
        Value::Number(_) => *value = Value::String("<lifecycle_number>".to_string()),
        Value::Array(values) => values.iter_mut().for_each(mask_lifecycle_numbers),
        Value::Object(values) => values.values_mut().for_each(mask_lifecycle_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

fn mask_memory_snapshot(memory: &mut Value) {
    if let Some(roots) = memory.get_mut("roots").and_then(Value::as_object_mut) {
        let values = std::mem::take(roots)
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        for (index, value) in values.into_iter().enumerate() {
            roots.insert(format!("<memory_root_{index}>"), value);
        }
    }
    mask_memory_numbers(memory);
}

fn mask_memory_numbers(value: &mut Value) {
    match value {
        Value::Number(_) => *value = Value::String("<memory_number>".to_string()),
        Value::Array(values) => values.iter_mut().for_each(mask_memory_numbers),
        Value::Object(values) => values.values_mut().for_each(mask_memory_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

fn normalize_text(text: &str, project_root: &Path, cache_dir: &Path) -> String {
    // The builder-state honesty detail carries a wall-clock start stamp
    // ("building since <unix> (age_s=<n>)"). The direct and tool_call runs
    // mint their registrations independently, so the stamps can straddle a
    // one-second tick (same class as tier2_last_run above). Mask the values,
    // keep the shape asserted.
    let text = regex::Regex::new(r"building since \d+ \(age_s=\d+\)")
        .expect("static regex")
        .replace_all(text, "building since <ts> (age_s=<n>)")
        .into_owned();
    let text = text.as_str();
    // Base root forms: the raw path plus its canonicalized form (macOS /var ->
    // /private/var, Windows verbatim prefixes, etc.).
    let mut base_roots = vec![
        project_root.to_string_lossy().to_string(),
        cache_dir.to_string_lossy().to_string(),
    ];
    if let Ok(canonical) = fs::canonicalize(project_root) {
        base_roots.push(canonical.to_string_lossy().to_string());
    }
    if let Ok(canonical) = fs::canonicalize(cache_dir) {
        base_roots.push(canonical.to_string_lossy().to_string());
    }
    // Windows: the raw temp root can carry an 8.3 short component
    // (RUNNER~1) while renderers that canonicalize-and-strip (apply_patch
    // diff headers) emit the LONG spelling without the `\\?\` prefix — a
    // form neither the raw nor the verbatim-canonical entry matches. Add the
    // prefix-stripped canonical spellings so those paths mask too.
    for stripped in base_roots
        .iter()
        .filter_map(|root| {
            root.strip_prefix("\\\\?\\UNC\\")
                .map(|rest| format!("\\\\{rest}"))
                .or_else(|| root.strip_prefix("\\\\?\\").map(str::to_string))
        })
        .collect::<Vec<_>>()
    {
        base_roots.push(stripped);
    }

    // The `status` tool is the only spine case rendered via serde_json
    // `to_string_pretty`, which JSON-ESCAPES backslashes — so a Windows root
    // embedded in that blob appears as `C:\\Users\\...` (doubled), not the raw
    // `C:\Users\...` the path yields. Forward-slash `display()` variants also
    // appear in some fields. Mask every form so the two temp-project processes'
    // differing roots all collapse to the same token; otherwise this parity
    // assertion fails Windows-only on `status_snapshot`.
    let mut roots = Vec::new();
    for base in base_roots {
        let escaped = base.replace('\\', "\\\\");
        let slashed = base.replace('\\', "/");
        roots.push(escaped);
        roots.push(slashed);
        roots.push(base);
    }
    // Replace longer forms first so a shorter prefix never shadows a longer
    // match (e.g. the escaped form is longer than the raw form).
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    roots.dedup();

    let mut normalized = text.to_string();
    for root in roots {
        normalized = normalized.replace(&root, "<PROJECT_ROOT>");
    }
    // The status text embeds the artifact-owner block whose key (derived from
    // the temp root path) and manifest path (under the temp storage dir)
    // differ per process — same class as the envelope-level masking above.
    for key in ["project_key", "manifest_path", "owner_project_scope_key"] {
        normalized = mask_json_string_value(&normalized, key);
    }
    // The status formatter appends a compact memory block after the stable
    // pretty-printed status JSON. RSS, estimates, counts, and transient busy
    // states can all differ between the two parity processes.
    if let Some(memory_start) = normalized.find("\n\nMemory:") {
        normalized.truncate(memory_start);
        normalized.push_str("\n\n<MEMORY>");
    }
    normalized
}

/// Replace every `"key": "<anything>"` string value with `"key": "<key>"` in
/// a JSON-ish text blob, without a regex dependency.
fn mask_json_string_value(text: &str, key: &str) -> String {
    let needle = format!("\"{key}\": \"");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&needle) {
        let value_start = start + needle.len();
        out.push_str(&rest[..value_start]);
        let Some(end) = rest[value_start..].find('"') else {
            rest = &rest[value_start..];
            break;
        };
        out.push_str(&format!("<{key}>"));
        rest = &rest[value_start + end..];
    }
    out.push_str(rest);
    out
}
