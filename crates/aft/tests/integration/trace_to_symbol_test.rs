//! Integration tests for `trace_to_symbol`.

use crate::helpers::AftProcess;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn configure_project(aft: &mut AftProcess, root: &Path) {
    let resp = aft.send(
        &json!({
            "id": "configure",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn trace_to_symbol(
    aft: &mut AftProcess,
    file: &Path,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&Path>,
    depth: Option<usize>,
) -> Value {
    let mut req = json!({
        "id": "trace",
        "command": "trace_to_symbol",
        "file": file.to_string_lossy(),
        "symbol": symbol,
        "toSymbol": to_symbol,
    });
    if let Some(to_file) = to_file {
        req["toFile"] = json!(to_file.to_string_lossy());
    }
    if let Some(depth) = depth {
        req["depth"] = json!(depth);
    }
    aft.send(&req.to_string())
}

fn trace_to_symbol_with_include_tests(
    aft: &mut AftProcess,
    file: &Path,
    symbol: &str,
    to_symbol: &str,
    include_tests: bool,
) -> Value {
    aft.send(
        &json!({
            "id": "trace",
            "command": "trace_to_symbol",
            "file": file.to_string_lossy(),
            "symbol": symbol,
            "toSymbol": to_symbol,
            "include_tests": include_tests,
        })
        .to_string(),
    )
}

fn path_symbols(resp: &Value) -> Vec<String> {
    resp["path"]
        .as_array()
        .expect("path should be an array")
        .iter()
        .map(|hop| hop["symbol"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn trace_to_symbol_direct_call_returns_two_hop_path() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("direct.ts");
    fs::write(
        &file,
        "export function a(): string { return b(); }\nexport function b(): string { return 'b'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "a", "b", None, None);

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(path_symbols(&resp), vec!["a", "b"]);
    assert_eq!(resp["path"].as_array().unwrap().len(), 2);
    assert_eq!(resp["path"][0]["line"], 1);
    assert_eq!(resp["path"][1]["line"], 2);

    aft.shutdown();
}

#[test]
fn trace_to_symbol_two_hop_chain_returns_three_hop_path() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("chain.ts");
    fs::write(
        &file,
        "export function a(): string { return b(); }\nfunction b(): string { return c(); }\nfunction c(): string { return 'c'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "a", "c", None, None);

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(path_symbols(&resp), vec!["a", "b", "c"]);
    assert_eq!(resp["path"].as_array().unwrap().len(), 3);

    aft.shutdown();
}

#[test]
fn trace_to_symbol_no_path_reports_complete_no_path_found() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("unrelated.ts");
    fs::write(
        &file,
        "export function a(): string { return b(); }\nfunction b(): string { return 'b'; }\nfunction c(): string { return 'c'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "a", "c", None, None);

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert!(resp["path"].is_null(), "path should be null: {resp:?}");
    assert_eq!(resp["reason"], "no_path_found");

    aft.shutdown();
}

#[test]
fn trace_to_symbol_max_depth_exhausted_reports_partial() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("depth.ts");
    fs::write(
        &file,
        "export function a(): string { return b(); }\nfunction b(): string { return c(); }\nfunction c(): string { return 'c'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "a", "c", None, Some(1));

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], false);
    assert!(resp["path"].is_null(), "path should be null: {resp:?}");
    assert_eq!(resp["reason"], "max_depth_exhausted");

    aft.shutdown();
}

#[test]
fn trace_to_symbol_from_symbol_not_found_returns_error() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("missing.ts");
    fs::write(&file, "export function a(): string { return 'a'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "missing", "a", None, None);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

#[test]
fn target_symbol_not_found_returns_specific_error() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("target_missing.ts");
    fs::write(&file, "export function a(): string { return 'a'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "a", "doesNotExist", None, None);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "target_symbol_not_found");
    assert!(resp["message"]
        .as_str()
        .unwrap_or("")
        .contains("doesNotExist"));

    aft.shutdown();
}

#[test]
fn to_file_not_found_returns_specific_error() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("missing_to_file.ts");
    fs::write(
        &file,
        "export function a(): string { return b(); }\nfunction b(): string { return 'b'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(
        &mut aft,
        &file,
        "a",
        "b",
        Some(Path::new("/nonexistent/path.rs")),
        None,
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "to_file_not_found");

    aft.shutdown();
}

#[test]
fn target_symbol_not_in_file_returns_candidates() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let start = root.join("start.ts");
    let has_x = root.join("has_x.ts");
    let without_x = root.join("fileWithoutX.ts");
    fs::write(
        &start,
        "import { X } from './has_x.js';\nexport function run(): string { return X(); }\n",
    )
    .unwrap();
    fs::write(&has_x, "export function X(): string { return 'x'; }\n").unwrap();
    fs::write(&without_x, "export function Y(): string { return 'y'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &start, "run", "X", Some(&without_x), None);

    assert_eq!(
        resp["success"], false,
        "wrong target file should fail: {resp:?}"
    );
    assert_eq!(resp["code"], "target_symbol_not_in_file");
    let candidates = resp["candidates"].as_array().expect("candidate list");
    assert_eq!(
        candidates.len(),
        1,
        "should list the file defining X: {resp:?}"
    );
    assert!(candidates.iter().any(|candidate| candidate["file"]
        .as_str()
        .unwrap_or("")
        .ends_with("has_x.ts")));

    aft.shutdown();
}

#[test]
fn trace_to_symbol_ambiguous_target_without_to_file_returns_candidates() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let start = root.join("start.ts");
    let one = root.join("one.ts");
    let two = root.join("two.ts");
    fs::write(
        &start,
        "import { target } from './one.js';\nexport function run(): string { return target(); }\n",
    )
    .unwrap();
    fs::write(&one, "export function target(): string { return 'one'; }\n").unwrap();
    fs::write(&two, "export function target(): string { return 'two'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &start, "run", "target", None, None);

    assert_eq!(
        resp["success"], false,
        "ambiguous target should fail: {resp:?}"
    );
    assert_eq!(resp["code"], "ambiguous_target");
    let candidates = resp["candidates"].as_array().expect("candidate list");
    assert_eq!(
        candidates.len(),
        2,
        "should list both target files: {resp:?}"
    );
    assert!(candidates
        .iter()
        .any(|candidate| candidate["file"].as_str().unwrap_or("").ends_with("one.ts")));
    assert!(candidates
        .iter()
        .any(|candidate| candidate["file"].as_str().unwrap_or("").ends_with("two.ts")));

    aft.shutdown();
}

#[test]
fn trace_to_symbol_ambiguous_target_with_to_file_traces_selected_file() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let start = root.join("start.ts");
    let one = root.join("one.ts");
    let two = root.join("two.ts");
    fs::write(
        &start,
        "import { target } from './one.js';\nexport function run(): string { return target(); }\n",
    )
    .unwrap();
    fs::write(&one, "export function target(): string { return 'one'; }\n").unwrap();
    fs::write(&two, "export function target(): string { return 'two'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &start, "run", "target", Some(&one), None);

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(path_symbols(&resp), vec!["run", "target"]);
    assert!(resp["path"][1]["file"]
        .as_str()
        .unwrap_or("")
        .ends_with("one.ts"));

    aft.shutdown();
}

#[test]
fn callgraph_public_to_path_guidance_and_retry() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("start.ts"),
        "import { target } from './one';\nexport function run() { target(); }\n",
    )
    .unwrap();
    fs::write(root.join("one.ts"), "export function target() {}\n").unwrap();
    fs::write(root.join("two.ts"), "export function target() {}\n").unwrap();
    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let mut request = serde_json::json!({"id":"public", "command":"tool_call", "name":"callgraph", "arguments": {"op":"trace_to_symbol", "path":root.join("start.ts"), "symbol":"run", "toSymbol":"target"}});
    let ambiguous = aft.send(&request.to_string());
    assert_eq!(ambiguous["message"], "trace_to_symbol: target symbol 'target' exists in multiple files; pass 'toPath' to disambiguate");
    assert_eq!(ambiguous["text"], "trace_to_symbol: ambiguous_target — multiple symbols named \"target\". Pass toPath to disambiguate:\n  - one.ts:1\n  - two.ts:1");
    request["arguments"]["toPath"] = serde_json::json!(root.join("one.ts"));
    let selected = aft.send(&request.to_string());
    assert_eq!(selected["success"], true, "{selected}");
    // Diagnostic footer availability depends on background analysis, not navigation semantics.
    let text = selected["text"]
        .as_str()
        .unwrap()
        .split("\n\n[AFT ")
        .next()
        .unwrap();
    assert_eq!(
        text,
        "2 hops\n  ↳ run [start.ts:2]\n    ↳ target [one.ts:1]"
    );
    aft.shutdown();
}

#[test]
fn trace_to_symbol_cycle_does_not_loop_and_finds_shortest_path() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("cycle.ts");
    fs::write(
        &file,
        "export function a(): string { b(); return c(); }\nfunction b(): string { return a(); }\nfunction c(): string { return 'c'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "b", "c", None, None);

    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(path_symbols(&resp), vec!["b", "a", "c"]);

    aft.shutdown();
}

#[test]
fn trace_to_symbol_include_tests_controls_test_only_path() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let start = root.join("start.ts");
    let tests = root.join("tests");
    let bridge = tests.join("bridge.ts");
    let target = root.join("target.ts");
    fs::create_dir_all(&tests).unwrap();
    fs::write(
        &start,
        "import { testBridge } from './tests/bridge.js';\nexport function start(): string { return testBridge(); }\n",
    )
    .unwrap();
    fs::write(
        &bridge,
        "import { target } from '../target.js';\nexport function testBridge(): string { return target(); }\n",
    )
    .unwrap();
    fs::write(
        &target,
        "export function target(): string { return 'target'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let hidden = trace_to_symbol_with_include_tests(&mut aft, &start, "start", "target", false);
    assert_eq!(hidden["success"], true, "trace should succeed: {hidden:?}");
    assert_eq!(hidden["complete"], true);
    assert!(
        hidden["path"].is_null(),
        "path should be hidden: {hidden:?}"
    );
    assert_eq!(hidden["reason"], "no_path_found");

    let included = trace_to_symbol_with_include_tests(&mut aft, &start, "start", "target", true);
    assert_eq!(
        included["success"], true,
        "trace should succeed: {included:?}"
    );
    assert_eq!(included["complete"], true);
    assert_eq!(
        path_symbols(&included),
        vec!["start", "testBridge", "target"]
    );

    aft.shutdown();
}

#[test]
fn trace_to_symbol_later_edge_still_finds_shorter_bfs_path() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("breadth.ts");
    fs::write(
        &file,
        "export function start(): string { slow(); return fast(); }\nfunction slow(): string { return middle(); }\nfunction middle(): string { return target(); }\nfunction fast(): string { return target(); }\nfunction target(): string { return 'target'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let resp = trace_to_symbol(&mut aft, &file, "start", "target", None, None);
    assert_eq!(resp["success"], true, "trace should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(path_symbols(&resp), vec!["start", "fast", "target"]);

    aft.shutdown();
}

#[test]
fn trace_to_symbol_rejects_out_of_project_file_path() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("outside.ts");
    fs::write(
        &outside_file,
        "export function outside(): string { return 'x'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root.path());

    let resp = trace_to_symbol(&mut aft, &outside_file, "outside", "outside", None, None);

    assert_eq!(resp["success"], false, "outside path should fail: {resp:?}");
    assert_eq!(resp["code"], "path_outside_project_root");

    aft.shutdown();
}
