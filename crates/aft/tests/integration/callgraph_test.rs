//! Integration tests for `configure` and `call_tree` commands.
//!
//! Exercises multi-file call graph traversal through the binary protocol
//! using the fixtures in `tests/fixtures/callgraph/`.

use crate::helpers::{fixture_path, AftProcess};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

fn configure_project(aft: &mut AftProcess, root: &Path) {
    let resp = aft.send(&format!(
        r#"{{"id":"configure","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root.display())
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

/// Copy the callgraph fixture outside this checkout's linked worktree.
/// The binary correctly treats linked worktrees as read-only, while these
/// tests need a writer-capable synthetic project for cold-build assertions.
fn callgraph_fixture() -> (TempDir, PathBuf) {
    let source = fixture_path("callgraph");
    let temp = tempdir().expect("create callgraph fixture copy");
    for entry in fs::read_dir(source).expect("read callgraph fixture") {
        let entry = entry.expect("read callgraph fixture entry");
        fs::copy(entry.path(), temp.path().join(entry.file_name()))
            .expect("copy callgraph fixture file");
    }
    let root = temp.path().to_path_buf();
    (temp, root)
}

fn path_text_ends_with(path: &str, suffix: &str) -> bool {
    path.replace('\\', "/").ends_with(suffix)
}

fn flattened_caller_entries(resp: &Value) -> Vec<&Value> {
    resp["callers"]
        .as_array()
        .expect("callers array")
        .iter()
        .flat_map(|group| group["callers"].as_array().expect("caller entries"))
        .collect()
}

/// `configure` sets project root and returns success.
#[test]
fn callgraph_configure_sets_project_root() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );
    assert_eq!(
        resp["project_root"].as_str().unwrap(),
        root,
        "should echo back the configured root"
    );

    aft.shutdown();
}

/// `configure` with missing param returns error.
#[test]
fn callgraph_configure_missing_param() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(r#"{"id":"1","command":"configure","harness":"opencode"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");

    aft.shutdown();
}

/// `call_tree` without prior `configure` returns not_configured error.
#[test]
fn callgraph_call_tree_without_configure() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(r#"{"id":"1","command":"call_tree","file":"main.ts","symbol":"main"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `call_tree` on a cross-file call chain returns nested tree.
///
/// main.ts:main → utils.ts:processData → helpers.ts:validate → helpers.ts:checkFormat
#[test]
fn callgraph_cross_file_tree() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    // Configure first
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true);

    // Get call tree for main
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"main","depth":5}}"#,
        crate::helpers::json_string(&format!("{}/main.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "call_tree should succeed: {:?}",
        resp
    );
    assert_eq!(resp["name"], "main");
    assert_eq!(resp["resolved"], true);
    assert_eq!(resp["line"], 3, "main definition line should be 1-based");

    // main calls processData
    let children = resp["children"]
        .as_array()
        .expect("children should be array");
    let process_data = children
        .iter()
        .find(|c| c["name"] == "processData")
        .expect("main should call processData");

    // processData should be resolved to utils.ts
    assert_eq!(process_data["resolved"], true);
    assert_eq!(
        process_data["line"], 3,
        "processData line should be 1-based"
    );
    assert!(
        process_data["file"].as_str().unwrap().contains("utils.ts"),
        "processData should be in utils.ts, got: {}",
        process_data["file"]
    );

    // processData calls validate
    let pd_children = process_data["children"]
        .as_array()
        .expect("processData children");
    let validate = pd_children
        .iter()
        .find(|c| c["name"] == "validate")
        .expect("processData should call validate");

    assert_eq!(validate["resolved"], true);
    assert_eq!(validate["line"], 1, "validate line should be 1-based");
    assert!(
        validate["file"].as_str().unwrap().contains("helpers.ts"),
        "validate should be in helpers.ts, got: {}",
        validate["file"]
    );

    // validate calls checkFormat (local, so it might be unresolved cross-file
    // but resolved within the same file)
    let v_children = validate["children"].as_array().expect("validate children");
    let check_format = v_children.iter().find(|c| c["name"] == "checkFormat");
    assert!(
        check_format.is_some(),
        "validate should call checkFormat, children: {:?}",
        v_children
            .iter()
            .map(|c| c["name"].clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        check_format.unwrap()["line"],
        5,
        "checkFormat line should be 1-based definition after same-file resolution"
    );
    assert_eq!(
        check_format.unwrap()["resolved"],
        true,
        "checkFormat should resolve within helpers.ts"
    );

    aft.shutdown();
}

/// `call_tree` with depth limit truncates the tree.
#[test]
fn callgraph_depth_limit_truncates() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    // Depth 1: main → processData (no deeper)
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"main","depth":1}}"#,
        crate::helpers::json_string(&format!("{}/main.ts", root))
    ));

    assert_eq!(resp["success"], true);
    assert_eq!(resp["name"], "main");

    let children = resp["children"].as_array().expect("children");
    for child in children {
        let grandchildren = child["children"].as_array();
        if let Some(gc) = grandchildren {
            assert!(
                gc.is_empty(),
                "At depth 1, child '{}' should have no grandchildren",
                child["name"]
            );
        }
    }

    aft.shutdown();
}

/// `call_tree` rejects files outside project_root even when generic path validation is relaxed.
#[test]
fn callgraph_call_tree_rejects_path_outside_project_root() {
    let mut aft = AftProcess::spawn();
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(
        outside.path().join("outside.ts"),
        "export function main() {}\n",
    )
    .unwrap();

    configure_project(&mut aft, root.path());

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"main"}}"#,
        crate::helpers::json_string(&outside.path().join("outside.ts").display())
    ));

    assert_eq!(resp["success"], false, "outside path should fail: {resp:?}");
    assert_eq!(resp["code"], "path_outside_project_root");

    aft.shutdown();
}

/// `call_tree` for an unknown symbol returns error.
#[test]
fn callgraph_unknown_symbol_error() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"nonexistent"}}"#,
        crate::helpers::json_string(&format!("{}/main.ts", root))
    ));

    assert_eq!(
        resp["success"], false,
        "unknown symbol should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

/// `call_tree` resolves aliased imports (import { validate as checker }).
#[test]
fn callgraph_aliased_import_resolution() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"runCheck","depth":3}}"#,
        crate::helpers::json_string(&format!("{}/aliased.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "aliased call_tree should succeed: {:?}",
        resp
    );
    assert_eq!(resp["name"], "runCheck");

    // runCheck calls checker (alias for validate)
    let children = resp["children"].as_array().expect("children");

    // The callee should resolve to validate in helpers.ts
    let resolved_child = children
        .iter()
        .find(|c| c["resolved"] == true && c["file"].as_str().unwrap_or("").contains("helpers.ts"));

    assert!(
        resolved_child.is_some(),
        "checker alias should resolve to helpers.ts, children: {:?}",
        children
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// callers command
// ---------------------------------------------------------------------------

/// `callers` without prior `configure` returns not_configured error.
#[test]
fn callgraph_callers_without_configure() {
    let mut aft = AftProcess::spawn();

    let resp =
        aft.send(r#"{"id":"1","command":"callers","file":"helpers.ts","symbol":"validate"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `callers` for a known cross-file call chain returns grouped results.
///
/// helpers.ts:validate is called by utils.ts:processData
#[test]
fn callgraph_callers_cross_file() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    // Configure first
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true);

    // Get callers of validate in helpers.ts
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
    assert_eq!(resp["symbol"], "validate");
    assert!(
        resp["total_callers"].as_u64().unwrap() > 0,
        "validate should have callers"
    );
    assert!(
        resp["scanned_files"].as_u64().unwrap() > 0,
        "should report scanned files"
    );

    // Callers should include processData from utils.ts
    let callers = resp["callers"].as_array().expect("callers array");
    let utils_group = callers
        .iter()
        .find(|g| g["file"].as_str().unwrap_or("").contains("utils.ts"));
    assert!(
        utils_group.is_some(),
        "validate should be called from utils.ts, groups: {:?}",
        callers
    );

    let group = utils_group.unwrap();
    let entries = group["callers"].as_array().expect("callers entries");
    let process_data_caller = entries
        .iter()
        .find(|e| e["symbol"].as_str().unwrap_or("") == "processData");
    assert!(
        process_data_caller.is_some(),
        "validate should be called by processData, entries: {:?}",
        entries
    );
    assert_eq!(
        process_data_caller.unwrap()["line"],
        4,
        "call site line should be 1-based"
    );

    aft.shutdown();
}

/// `callers` for a symbol with no callers returns empty result.
#[test]
fn callgraph_callers_empty_result() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    // main is an entry point — nothing calls it
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"main"}}"#,
        crate::helpers::json_string(&format!("{}/main.ts", root))
    ));

    assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
    assert_eq!(resp["total_callers"], 0, "main should have no callers");
    let callers = resp["callers"].as_array().expect("callers array");
    assert!(
        callers.is_empty(),
        "callers should be empty for entry point"
    );

    aft.shutdown();
}

/// `callers` with recursive depth finds transitive callers.
#[test]
fn callgraph_callers_recursive() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    // checkFormat is called by validate (same file), validate called by processData (utils.ts)
    // With depth 2, we should see transitive callers
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"validate","depth":2}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "recursive callers should succeed: {:?}",
        resp
    );

    // With depth 2, should find transitive callers from main.ts
    // (main → processData → validate)
    let total = resp["total_callers"].as_u64().unwrap();
    assert!(
        total >= 2,
        "with depth 2, validate should have >= 2 callers (direct + transitive), got {}",
        total
    );

    aft.shutdown();
}

#[test]
fn callgraph_resolves_workspace_package_import_callers_and_tree() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();

    let pkg_a = root.join("packages/pkg-a");
    let pkg_b = root.join("packages/pkg-b");
    fs::create_dir_all(pkg_a.join("src")).unwrap();
    fs::create_dir_all(pkg_b.join("src")).unwrap();
    fs::write(
        pkg_a.join("package.json"),
        r#"{"name":"@scope/pkg-a","exports":{".":{"import":"./dist/index.js"}}}"#,
    )
    .unwrap();
    fs::write(
        pkg_a.join("src/index.ts"),
        r#"export { workspaceTarget } from "./target.js";
"#,
    )
    .unwrap();
    fs::write(
        pkg_a.join("src/target.ts"),
        r#"export function workspaceTarget(): string {
  return "ok";
}
"#,
    )
    .unwrap();
    fs::write(pkg_b.join("package.json"), r#"{"name":"pkg-b"}"#).unwrap();
    fs::write(
        pkg_b.join("src/main.ts"),
        r#"import { workspaceTarget } from "@scope/pkg-a";

export function runWorkspaceImport(): string {
  return workspaceTarget();
}
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let root_display = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_display)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"workspaceTarget","depth":1}}"#,
        crate::helpers::json_string(&pkg_a.join("src/target.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
    assert_eq!(
        resp["total_callers"], 1,
        "workspace import caller should resolve"
    );
    let callers = resp["callers"].as_array().expect("callers array");
    let pkg_b_group = callers.iter().find(|group| {
        group["file"]
            .as_str()
            .is_some_and(|path| path_text_ends_with(path, "packages/pkg-b/src/main.ts"))
    });
    assert!(
        pkg_b_group.is_some(),
        "caller should be pkg-b main.ts: {:?}",
        callers
    );

    let resp = aft.send(&format!(
        r#"{{"id":"3","command":"call_tree","file":{},"symbol":"runWorkspaceImport","depth":2}}"#,
        crate::helpers::json_string(&pkg_b.join("src/main.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "call_tree should succeed: {:?}",
        resp
    );
    let children = resp["children"].as_array().expect("children array");
    let target_child = children
        .iter()
        .find(|child| child["name"] == "workspaceTarget")
        .expect("runWorkspaceImport should call workspaceTarget");
    assert_eq!(target_child["resolved"], true);
    assert!(
        target_child["file"]
            .as_str()
            .is_some_and(|path| path_text_ends_with(path, "packages/pkg-a/src/target.ts")),
        "workspaceTarget should resolve through package export to source file: {:?}",
        target_child
    );

    aft.shutdown();
}

#[test]
fn callgraph_resolves_workspace_package_imports_past_nested_lockfile() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();

    let bridge = root.join("packages/aft-bridge");
    let opencode = root.join("packages/opencode-plugin");
    let pi = root.join("packages/pi-plugin");
    fs::create_dir_all(bridge.join("src")).unwrap();
    fs::create_dir_all(opencode.join("src/tools")).unwrap();
    fs::create_dir_all(pi.join("src/tools")).unwrap();

    fs::write(
        bridge.join("package.json"),
        r#"{"name":"@cortexkit/aft-bridge","exports":{".":{"import":"./dist/index.js"}}}"#,
    )
    .unwrap();
    fs::write(
        bridge.join("src/index.ts"),
        r#"export { fetchUrlToTempFile } from "./url-fetch.js";
export { formatZoomText } from "./zoom-format.js";
"#,
    )
    .unwrap();
    fs::write(
        bridge.join("src/url-fetch.ts"),
        r#"export function fetchUrlToTempFile(url: string): string {
  return url;
}
"#,
    )
    .unwrap();
    fs::write(
        bridge.join("src/zoom-format.ts"),
        r#"export function formatZoomText(text: string): string {
  return text;
}
"#,
    )
    .unwrap();

    fs::write(
        opencode.join("package.json"),
        r#"{"name":"@cortexkit/aft-opencode","dependencies":{"@cortexkit/aft-bridge":"0.0.0"}}"#,
    )
    .unwrap();
    fs::write(opencode.join("bun.lock"), "").unwrap();
    fs::write(
        opencode.join("src/tools/reading.ts"),
        r#"import { fetchUrlToTempFile, formatZoomText } from "@cortexkit/aft-bridge";

export function registerOpenCodeReadingTools(): string {
  const path = fetchUrlToTempFile("https://example.com/opencode");
  return formatZoomText(path);
}
"#,
    )
    .unwrap();

    fs::write(
        pi.join("package.json"),
        r#"{"name":"@cortexkit/aft-pi","dependencies":{"@cortexkit/aft-bridge":"0.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        pi.join("src/tools/reading.ts"),
        r#"import { fetchUrlToTempFile, formatZoomText } from "@cortexkit/aft-bridge";

export function registerPiReadingTools(): string {
  const path = fetchUrlToTempFile("https://example.com/pi");
  return formatZoomText(path);
}
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let root_display = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_display)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    for (id, file, symbol) in [
        ("2", bridge.join("src/url-fetch.ts"), "fetchUrlToTempFile"),
        ("3", bridge.join("src/zoom-format.ts"), "formatZoomText"),
    ] {
        let resp = aft.send(&format!(
            r#"{{"id":"{}","command":"callers","file":{},"symbol":"{}","depth":1}}"#,
            id,
            crate::helpers::json_string(&file.display()),
            symbol
        ));
        assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
        assert_eq!(
            resp["total_callers"], 2,
            "both workspace package consumers should call {symbol}: {resp:?}"
        );

        let callers = resp["callers"].as_array().expect("callers array");
        for expected_file in [
            "packages/opencode-plugin/src/tools/reading.ts",
            "packages/pi-plugin/src/tools/reading.ts",
        ] {
            assert!(
                callers.iter().any(|group| group["file"]
                    .as_str()
                    .is_some_and(|path| path_text_ends_with(path, expected_file))),
                "{symbol} caller should include {expected_file}: {:?}",
                callers
            );
        }
    }

    aft.shutdown();
}

#[test]
fn callgraph_prefers_workspace_package_source_over_existing_dist() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();

    let pkg_a = root.join("packages/pkg-a");
    let pkg_b = root.join("packages/pkg-b");
    fs::create_dir_all(pkg_a.join("dist")).unwrap();
    fs::create_dir_all(pkg_a.join("src")).unwrap();
    fs::create_dir_all(pkg_b.join("src")).unwrap();
    fs::write(
        pkg_a.join("package.json"),
        r#"{"name":"@scope/pkg-a","main":"dist/index.js"}"#,
    )
    .unwrap();
    fs::write(
        pkg_a.join("dist/index.js"),
        r#"export const bundledValue = 1;
"#,
    )
    .unwrap();
    fs::write(
        pkg_a.join("src/index.ts"),
        r#"export { workspaceTarget } from "./target.js";
"#,
    )
    .unwrap();
    fs::write(
        pkg_a.join("src/target.ts"),
        r#"export function workspaceTarget(): string {
  return "ok";
}
"#,
    )
    .unwrap();
    fs::write(pkg_b.join("package.json"), r#"{"name":"pkg-b"}"#).unwrap();
    fs::write(
        pkg_b.join("src/main.ts"),
        r#"import { workspaceTarget } from "@scope/pkg-a";

export function runWorkspaceImport(): string {
  return workspaceTarget();
}
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let root_display = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_display)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"workspaceTarget","depth":1}}"#,
        crate::helpers::json_string(&pkg_a.join("src/target.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
    assert_eq!(
        resp["total_callers"], 1,
        "workspace import caller should resolve through source even when dist exists: {:?}",
        resp
    );
    let callers = resp["callers"].as_array().expect("callers array");
    let pkg_b_group = callers.iter().find(|group| {
        group["file"]
            .as_str()
            .is_some_and(|path| path_text_ends_with(path, "packages/pkg-b/src/main.ts"))
    });
    assert!(
        pkg_b_group.is_some(),
        "caller should be pkg-b main.ts, not dist/index.js: {:?}",
        callers
    );

    let resp = aft.send(&format!(
        r#"{{"id":"3","command":"call_tree","file":{},"symbol":"runWorkspaceImport","depth":2}}"#,
        crate::helpers::json_string(&pkg_b.join("src/main.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "call_tree should succeed: {:?}",
        resp
    );
    let children = resp["children"].as_array().expect("children array");
    let target_child = children
        .iter()
        .find(|child| child["name"] == "workspaceTarget")
        .expect("runWorkspaceImport should call workspaceTarget");
    assert_eq!(target_child["resolved"], true);
    assert!(
        target_child["file"]
            .as_str()
            .is_some_and(|path| path_text_ends_with(path, "packages/pkg-a/src/target.ts")),
        "workspaceTarget should resolve to source target instead of dist bundle: {:?}",
        target_child
    );
    assert!(
        !target_child["file"]
            .as_str()
            .is_some_and(|path| path.replace('\\', "/").contains("/dist/")),
        "workspaceTarget should not resolve to dist bundle: {:?}",
        target_child
    );

    aft.shutdown();
}

#[test]
fn callgraph_indexes_relative_calls_inside_test_callbacks() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src/shared")).unwrap();
    fs::create_dir_all(root.join("src/__tests__")).unwrap();
    fs::write(
        root.join("src/shared/model.ts"),
        r#"export function testTarget(): number {
  return 1;
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/__tests__/model.test.ts"),
        r#"import { expect, test } from "bun:test";
import { testTarget } from "../shared/model.js";

test("calls target", () => {
  expect(testTarget()).toBe(1);
});
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let root_display = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_display)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"testTarget","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/shared/model.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {:?}", resp);
    assert_eq!(
        resp["total_callers"], 1,
        "test callback caller should be counted even when hidden"
    );
    let callers = resp["callers"].as_array().expect("callers array");
    assert!(
        callers.is_empty(),
        "test caller should be hidden by default while total remains honest: {:?}",
        callers
    );

    let resp = aft.send(&format!(
        r#"{{"id":"3","command":"callers","file":{},"symbol":"testTarget","depth":1,"includeTests":true}}"#,
        crate::helpers::json_string(&root.join("src/shared/model.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "callers with includeTests should succeed: {:?}",
        resp
    );
    let callers = resp["callers"].as_array().expect("callers array");
    let test_group = callers.iter().find(|group| {
        group["file"]
            .as_str()
            .is_some_and(|path| path_text_ends_with(path, "src/__tests__/model.test.ts"))
    });
    assert!(
        test_group.is_some(),
        "caller should be the test file: {:?}",
        callers
    );
    let entries = test_group.unwrap()["callers"]
        .as_array()
        .expect("caller entries");
    assert!(
        entries.iter().any(|entry| entry["line"] == 5),
        "testTarget call site should be line 5: {:?}",
        entries
    );

    aft.shutdown();
}

#[test]
fn callgraph_hides_tests_by_default_and_summarizes_hubs_honestly() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src/__tests__")).unwrap();
    fs::write(
        root.join("src/target.ts"),
        r#"export function target(): number {
  return 1;
}

export function smallTarget(): number {
  return 2;
}
"#,
    )
    .unwrap();

    for idx in 0..20 {
        fs::write(
            root.join(format!("src/caller{idx:02}.ts")),
            format!(
                r#"import {{ target }} from "./target";

export function caller{idx:02}(): number {{
  return target();
}}
"#
            ),
        )
        .unwrap();
    }
    for idx in 0..5 {
        fs::write(
            root.join(format!("src/__tests__/caller{idx:02}.test.ts")),
            format!(
                r#"import {{ target, smallTarget }} from "../target";

export function aaaTestCaller{idx:02}(): number {{
  return target() + smallTarget();
}}
"#
            ),
        )
        .unwrap();
    }
    for idx in 0..3 {
        fs::write(
            root.join(format!("src/small_caller{idx:02}.ts")),
            format!(
                r#"import {{ smallTarget }} from "./target";

export function smallCaller{idx:02}(): number {{
  return smallTarget();
}}
"#
            ),
        )
        .unwrap();
    }

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let target_path = root.join("src/target.ts");
    let callers = aft.send(&format!(
        r#"{{"id":"callers-hub","command":"callers","file":{},"symbol":"target","depth":1}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        callers["success"], true,
        "callers should succeed: {callers:?}"
    );
    assert_eq!(
        callers["total_callers"], 25,
        "summary count should include hidden tests"
    );
    let summary = callers["hub_summary"]["message"]
        .as_str()
        .expect("hub summary");
    assert!(
        summary.contains("Next: 25 callers (5 in tests, hidden — pass includeTests)"),
        "summary should state hidden tests and honest total: {summary}"
    );
    let visible_callers = flattened_caller_entries(&callers);
    assert_eq!(
        visible_callers.len(),
        15,
        "hub summary should show top 15 callers"
    );
    assert!(
        callers["callers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|group| !group["file"].as_str().unwrap().contains("__tests__")),
        "default callers should hide test files: {callers:?}"
    );

    let callers_with_tests = aft.send(&format!(
        r#"{{"id":"callers-hub-tests","command":"callers","file":{},"symbol":"target","depth":1,"includeTests":true}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        callers_with_tests["success"], true,
        "includeTests callers should succeed: {callers_with_tests:?}"
    );
    assert!(
        callers_with_tests["callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| group["file"].as_str().unwrap().contains("__tests__")),
        "includeTests should show test callers in hub top-k: {callers_with_tests:?}"
    );

    let impact = aft.send(&format!(
        r#"{{"id":"impact-hub","command":"impact","file":{},"symbol":"target","depth":1}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(impact["success"], true, "impact should succeed: {impact:?}");
    assert_eq!(
        impact["total_affected"], 25,
        "impact summary count should include hidden tests"
    );
    let impact_summary = impact["hub_summary"]["message"]
        .as_str()
        .expect("impact summary");
    assert!(
        impact_summary
            .contains("Next: 25 affected callers (5 in tests, hidden — pass includeTests)"),
        "impact summary should state hidden tests and honest total: {impact_summary}"
    );
    let impact_callers = impact["callers"].as_array().expect("impact callers");
    assert_eq!(
        impact_callers.len(),
        15,
        "impact hub summary should show top 15 callers"
    );
    assert!(
        impact_callers.iter().all(|caller| !caller["caller_file"]
            .as_str()
            .unwrap()
            .contains("__tests__")),
        "default impact should hide test callers: {impact:?}"
    );

    let impact_with_tests = aft.send(&format!(
        r#"{{"id":"impact-hub-tests","command":"impact","file":{},"symbol":"target","depth":1,"includeTests":true}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        impact_with_tests["success"], true,
        "includeTests impact should succeed: {impact_with_tests:?}"
    );
    assert!(
        impact_with_tests["callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["caller_file"]
                .as_str()
                .unwrap()
                .contains("__tests__")),
        "includeTests should show test impact callers in hub top-k: {impact_with_tests:?}"
    );

    let trace = aft.send(&format!(
        r#"{{"id":"trace-hub","command":"trace_to","file":{},"symbol":"target","depth":2}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(trace["success"], true, "trace_to should succeed: {trace:?}");
    assert_eq!(
        trace["total_paths"], 26,
        "trace summary count should include hidden tests and the target self-path"
    );
    let trace_summary = trace["hub_summary"]["message"]
        .as_str()
        .expect("trace summary");
    assert!(
        trace_summary.contains("Next: 26 paths (5 in tests, hidden — pass includeTests)"),
        "trace summary should state hidden tests and honest total: {trace_summary}"
    );
    let paths = trace["paths"].as_array().expect("trace paths");
    assert_eq!(
        paths.len(),
        15,
        "trace_to hub summary should show top 15 paths"
    );
    assert!(
        paths.iter().all(|path| !path["hops"][0]["file"]
            .as_str()
            .unwrap()
            .contains("__tests__")),
        "default trace paths should hide test entry points: {trace:?}"
    );

    let trace_with_tests = aft.send(&format!(
        r#"{{"id":"trace-hub-tests","command":"trace_to","file":{},"symbol":"target","depth":2,"includeTests":true}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        trace_with_tests["success"], true,
        "includeTests trace_to should succeed: {trace_with_tests:?}"
    );
    assert!(
        trace_with_tests["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["hops"][0]["file"]
                .as_str()
                .unwrap()
                .contains("__tests__")),
        "includeTests should show test trace paths in hub top-k: {trace_with_tests:?}"
    );

    let small = aft.send(&format!(
        r#"{{"id":"callers-small","command":"callers","file":{},"symbol":"smallTarget","depth":1}}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        small["success"], true,
        "small callers should succeed: {small:?}"
    );
    assert_eq!(
        small["total_callers"], 8,
        "small total should include hidden tests"
    );
    assert!(
        small.get("hub_summary").is_none(),
        "small result should not summarize: {small:?}"
    );
    assert_eq!(
        flattened_caller_entries(&small).len(),
        3,
        "small result should render the full non-test list after test-gating"
    );

    aft.shutdown();
}

#[test]
fn callgraph_leaves_non_workspace_package_imports_unresolved() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();
    let app = root.join("packages/app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        app.join("src/main.ts"),
        r#"import { useMemo } from "react";

export function render(): unknown {
  return useMemo(() => "ok", []);
}
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let root_display = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_display)
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"render","depth":1}}"#,
        crate::helpers::json_string(&app.join("src/main.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "call_tree should succeed: {:?}",
        resp
    );
    let children = resp["children"].as_array().expect("children array");
    let use_memo = children
        .iter()
        .find(|child| child["name"] == "useMemo")
        .expect("render should call useMemo");
    assert_eq!(
        use_memo["resolved"], false,
        "react import should not resolve as workspace"
    );

    aft.shutdown();
}

#[test]
fn callgraph_reexport_alias_resolves_to_source_symbol() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("real.ts"),
        "export function foo(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(
        root.join("barrel.ts"),
        "export { foo as bar } from './real.js';\n",
    )
    .unwrap();
    fs::write(
        root.join("app.ts"),
        "import { bar } from './barrel.js';\nexport function run(): string { return bar(); }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"foo","depth":1}}"#,
        crate::helpers::json_string(&root.join("real.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "alias re-export should point at foo: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_aliased_import_follows_reexport_barrel() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("real.ts"),
        "export function foo(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(root.join("barrel.ts"), "export { foo } from './real.js';\n").unwrap();
    fs::write(
        root.join("app.ts"),
        "import { foo as bar } from './barrel.js';\nexport function run(): string { return bar(); }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"foo","depth":1}}"#,
        crate::helpers::json_string(&root.join("real.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "aliased import should follow barrel: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_default_reexport_resolves_real_default_symbol() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("real.ts"),
        "export default function targetDefault(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(
        root.join("barrel.ts"),
        "export { default } from './real.js';\n",
    )
    .unwrap();
    fs::write(
        root.join("app.ts"),
        "import targetDefault from './barrel.js';\nexport function run(): string { return targetDefault(); }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"targetDefault","depth":1}}"#,
        crate::helpers::json_string(&root.join("real.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "default re-export should resolve to real default: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_call_tree_resolves_same_file_calls() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("local.ts"),
        "export function main(): string { return helper(); }\nfunction helper(): string { return leaf(); }\nfunction leaf(): string { return 'ok'; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"call_tree","file":{},"symbol":"main","depth":3}}"#,
        crate::helpers::json_string(&root.join("local.ts").display())
    ));
    assert_eq!(resp["success"], true, "call_tree should succeed: {resp:?}");
    let helper = resp["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "helper")
        .expect("main should resolve helper");
    assert_eq!(
        helper["resolved"], true,
        "helper should be a resolved local child: {helper:?}"
    );
    assert!(
        helper["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "leaf"),
        "helper should descend into leaf: {helper:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_workspace_package_cache_refreshes_after_reconfigure() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    )
    .unwrap();
    let pkg = root.join("packages/pkg");
    let app = root.join("packages/app");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(pkg.join("package.json"), r#"{"name":"@scope/old"}"#).unwrap();
    fs::write(
        pkg.join("src/index.ts"),
        "export function target(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(app.join("src/main.ts"), "import { target } from '@scope/new';\nexport function run(): string { return target(); }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"target","depth":1}}"#,
        crate::helpers::json_string(&pkg.join("src/index.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "first callers should succeed: {resp:?}"
    );
    assert_eq!(
        resp["total_callers"], 0,
        "new package name should miss before package.json changes"
    );

    fs::write(pkg.join("package.json"), r#"{"name":"@scope/new"}"#).unwrap();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"target","depth":1}}"#,
        crate::helpers::json_string(&pkg.join("src/index.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "second callers should succeed: {resp:?}"
    );
    assert_eq!(
        resp["total_callers"], 1,
        "workspace cache should not keep stale miss: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_indexes_tsx_jsx_components_and_new_expressions() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("component.tsx"),
        "export function Widget(): JSX.Element { return <span />; }\nexport class Service {}\nexport function App(): JSX.Element { const service = new Service(); return <Widget />; }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    for (id, symbol) in [("1", "Widget"), ("2", "Service")] {
        let resp = aft.send(&format!(
            r#"{{"id":"{}","command":"callers","file":{},"symbol":"{}","depth":1}}"#,
            id,
            crate::helpers::json_string(&root.join("component.tsx").display()),
            symbol
        ));
        assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
        assert_eq!(
            resp["total_callers"], 1,
            "{symbol} should be indexed from JSX/new: {resp:?}"
        );
    }
    aft.shutdown();
}

#[test]
fn callgraph_source_less_export_alias_resolves_local_symbol() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("module.ts"),
        "function foo(): string { return 'ok'; }\nexport { foo as bar };\n",
    )
    .unwrap();
    fs::write(
        root.join("app.ts"),
        "import { bar } from './module.js';\nexport function run(): string { return bar(); }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"foo","depth":1}}"#,
        crate::helpers::json_string(&root.join("module.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "source-less alias should resolve to local foo: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_commands_accept_private_leaf_symbols() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("leaf.ts"), "export function caller(): string { return leaf(); }\nfunction leaf(): string { return 'ok'; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let tree = aft.send(&format!(
        r#"{{"id":"1","command":"call_tree","file":{},"symbol":"leaf","depth":1}}"#,
        crate::helpers::json_string(&root.join("leaf.ts").display())
    ));
    assert_eq!(
        tree["success"], true,
        "call_tree should accept private leaf: {tree:?}"
    );
    let callers = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"leaf","depth":1}}"#,
        crate::helpers::json_string(&root.join("leaf.ts").display())
    ));
    assert_eq!(
        callers["success"], true,
        "callers should accept private leaf: {callers:?}"
    );
    assert_eq!(
        callers["total_callers"], 1,
        "leaf should have one caller: {callers:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_detects_pnpm_workspace_and_skips_empty_nested_workspaces() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("pnpm-workspace.yaml"),
        "packages:\n  - 'packages/*'\n",
    )
    .unwrap();
    let pkg = root.join("packages/lib");
    let app = root.join("packages/app");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(pkg.join("package.json"), r#"{"name":"@scope/lib"}"#).unwrap();
    fs::write(
        pkg.join("src/index.ts"),
        "export function pnpmTarget(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","workspaces":[]}"#,
    )
    .unwrap();
    fs::write(app.join("src/main.ts"), "import { pnpmTarget } from '@scope/lib';\nexport function run(): string { return pnpmTarget(); }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"pnpmTarget","depth":1}}"#,
        crate::helpers::json_string(&pkg.join("src/index.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "pnpm workspace root should resolve past empty nested workspaces: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_workspace_globs_support_recursive_patterns_and_negations() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("package.json"),
        r#"{"private":true,"workspaces":["packages/**","!packages/legacy"]}"#,
    )
    .unwrap();
    let recursive = root.join("packages/nested/pkg");
    let legacy = root.join("packages/legacy");
    let app = root.join("app");
    fs::create_dir_all(recursive.join("src")).unwrap();
    fs::create_dir_all(legacy.join("src")).unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        recursive.join("package.json"),
        r#"{"name":"@scope/recursive"}"#,
    )
    .unwrap();
    fs::write(
        recursive.join("src/index.ts"),
        "export function recursiveTarget(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(legacy.join("package.json"), r#"{"name":"@scope/legacy"}"#).unwrap();
    fs::write(
        legacy.join("src/index.ts"),
        "export function legacyTarget(): string { return 'legacy'; }\n",
    )
    .unwrap();
    fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(app.join("src/main.ts"), "import { recursiveTarget } from '@scope/recursive';\nimport { legacyTarget } from '@scope/legacy';\nexport function run(): string { recursiveTarget(); return legacyTarget(); }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"recursiveTarget","depth":1}}"#,
        crate::helpers::json_string(&recursive.join("src/index.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "recursive callers should succeed: {resp:?}"
    );
    assert_eq!(
        resp["total_callers"], 1,
        "recursive glob should include nested package: {resp:?}"
    );
    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"legacyTarget","depth":1}}"#,
        crate::helpers::json_string(&legacy.join("src/index.ts").display())
    ));
    assert_eq!(
        resp["success"], true,
        "legacy callers should succeed: {resp:?}"
    );
    assert_eq!(
        resp["total_callers"], 0,
        "negated workspace should not resolve: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_resolves_tsconfig_paths_aliases() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src/lib")).unwrap();
    fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("src/lib/target.ts"),
        "export function pathTarget(): string { return 'ok'; }\n",
    )
    .unwrap();
    fs::write(root.join("src/main.ts"), "import { pathTarget } from '@/lib/target';\nexport function run(): string { return pathTarget(); }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"pathTarget","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/lib/target.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "tsconfig paths alias should resolve: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_computed_member_call_uses_static_property_name() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("computed.ts"), "export function caller(obj: any): void { obj[\"method\"](); }\nfunction method(): void {}\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"method","depth":1}}"#,
        crate::helpers::json_string(&root.join("computed.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "computed member should be indexed as method: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn callgraph_keeps_external_member_call_with_same_short_name_as_caller() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("math.ts"),
        "export function add(): number { return 1; }\n",
    )
    .unwrap();
    fs::write(root.join("app.ts"), "import * as math from './math.js';\nexport function add(): number { return math.add(); }\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"add","depth":1}}"#,
        crate::helpers::json_string(&root.join("math.ts").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "external math.add call should not be filtered as self-recursion: {resp:?}"
    );
    aft.shutdown();
}

fn write_rust_manifest(root: &Path, package_name: &str) {
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
}

fn assert_single_caller(
    resp: &serde_json::Value,
    expected_file_suffix: &str,
    expected_symbol: &str,
) {
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 1,
        "expected exactly one caller: {resp:?}"
    );
    let callers = resp["callers"].as_array().expect("callers array");
    let group = callers
        .iter()
        .find(|group| {
            group["file"]
                .as_str()
                .is_some_and(|path| path_text_ends_with(path, expected_file_suffix))
        })
        .unwrap_or_else(|| panic!("caller should include {expected_file_suffix}: {callers:?}"));
    let entries = group["callers"].as_array().expect("caller entries");
    assert!(
        entries
            .iter()
            .any(|entry| entry["symbol"].as_str().unwrap_or("") == expected_symbol),
        "caller should include symbol {expected_symbol}: {entries:?}"
    );
}

#[test]
fn callgraph_rust_crate_qualified_callers_resolve() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    write_rust_manifest(root, "rust-qualified-fixture");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod caller;\npub mod target;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/target.rs"),
        "pub fn crate_qualified_target() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/caller.rs"),
        "pub fn run_crate_qualified() {\n    crate::target::crate_qualified_target();\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"crate_qualified_target","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/target.rs").display())
    ));
    assert_single_caller(&resp, "src/caller.rs", "run_crate_qualified");
    aft.shutdown();
}

#[test]
fn callgraph_rust_use_imported_short_callers_resolve() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    write_rust_manifest(root, "rust-use-fixture");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod caller;\npub mod target;\n",
    )
    .unwrap();
    fs::write(root.join("src/target.rs"), "pub fn imported_target() {}\n").unwrap();
    fs::write(
        root.join("src/caller.rs"),
        "use crate::target::imported_target;\n\npub fn run_imported() {\n    imported_target();\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"imported_target","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/target.rs").display())
    ));
    assert_single_caller(&resp, "src/caller.rs", "run_imported");
    aft.shutdown();
}

#[test]
fn callgraph_rust_self_and_super_callers_resolve() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    write_rust_manifest(root, "rust-relative-fixture");
    fs::create_dir_all(root.join("src/parent")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod current;\npub mod parent;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/current.rs"),
        "pub fn self_target() {}\n\npub fn run_self() {\n    self::self_target();\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/parent.rs"),
        "pub mod child;\n\npub fn super_target() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/parent/child.rs"),
        "pub fn run_super() {\n    super::super_target();\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let self_resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"self_target","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/current.rs").display())
    ));
    assert_single_caller(&self_resp, "src/current.rs", "run_self");

    let super_resp = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"super_target","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/parent.rs").display())
    ));
    assert_single_caller(&super_resp, "src/parent/child.rs", "run_super");
    aft.shutdown();
}

#[test]
fn callgraph_rust_workspace_lib_name_callers_resolve() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"libpkg\", \"app\"]\n",
    )
    .unwrap();

    let libpkg = root.join("libpkg");
    let app = root.join("app");
    fs::create_dir_all(libpkg.join("src")).unwrap();
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        libpkg.join("Cargo.toml"),
        "[package]\nname = \"lib-package\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"fixture_lib\"\n",
    )
    .unwrap();
    fs::write(libpkg.join("src/lib.rs"), "pub mod api;\n").unwrap();
    fs::write(libpkg.join("src/api.rs"), "pub fn workspace_target() {}\n").unwrap();
    write_rust_manifest(&app, "app");
    fs::write(
        app.join("src/main.rs"),
        "pub fn dispatch() {\n    fixture_lib::api::workspace_target();\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"workspace_target","depth":1}}"#,
        crate::helpers::json_string(&libpkg.join("src/api.rs").display())
    ));
    assert_single_caller(&resp, "app/src/main.rs", "dispatch");
    aft.shutdown();
}

#[test]
fn callgraph_rust_external_paths_stay_unresolved() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    write_rust_manifest(root, "rust-external-fixture");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod caller;\npub mod target;\n",
    )
    .unwrap();
    fs::write(root.join("src/target.rs"), "pub fn new() {}\n").unwrap();
    fs::write(
        root.join("src/caller.rs"),
        "use std::collections::HashMap;\n\npub fn run_external() {\n    let _map: HashMap<String, String> = HashMap::new();\n    unknown_crate::target::new();\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"callers","file":{},"symbol":"new","depth":1}}"#,
        crate::helpers::json_string(&root.join("src/target.rs").display())
    ));
    assert_eq!(resp["success"], true, "callers should succeed: {resp:?}");
    assert_eq!(
        resp["total_callers"], 0,
        "std and unknown external paths must not become callers: {resp:?}"
    );
    aft.shutdown();
}

// ---------------------------------------------------------------------------
// trace_to command
// ---------------------------------------------------------------------------

/// `trace_to` without prior `configure` returns not_configured error.
#[test]
fn callgraph_trace_to_not_configured() {
    let mut aft = AftProcess::spawn();

    let resp =
        aft.send(r#"{"id":"1","command":"trace_to","file":"helpers.ts","symbol":"checkFormat"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `trace_to` on a nonexistent symbol returns symbol_not_found error.
#[test]
fn callgraph_trace_to_symbol_not_found() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_to","file":{},"symbol":"nonexistent"}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(
        resp["success"], false,
        "unknown symbol should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

/// `trace_to` on a deeply-nested symbol returns a single path through the chain.
///
/// checkFormat is called by validate (helpers.ts), which is called by processData (utils.ts),
/// which is called by main (main.ts). Should return at least one path with main as entry point.
#[test]
fn callgraph_trace_to_single_path() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_to","file":{},"symbol":"checkFormat","depth":10}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(resp["success"], true, "trace_to should succeed: {:?}", resp);
    assert_eq!(resp["target_symbol"], "checkFormat");
    assert!(resp["target_file"].as_str().unwrap().contains("helpers.ts"));

    let paths = resp["paths"].as_array().expect("paths should be array");
    assert!(
        !paths.is_empty(),
        "checkFormat should have at least one path to an entry point"
    );

    // At least one path should start at main and end at checkFormat
    let main_path = paths.iter().find(|p| {
        let hops = p["hops"].as_array().unwrap();
        !hops.is_empty() && hops[0]["symbol"] == "main"
    });
    assert!(
        main_path.is_some(),
        "should have a path starting from main, paths: {:?}",
        paths
    );

    // That path should end at checkFormat (last hop)
    let hops = main_path.unwrap()["hops"].as_array().unwrap();
    let last = &hops[hops.len() - 1];
    assert_eq!(
        last["symbol"], "checkFormat",
        "path should end at checkFormat"
    );
    assert_eq!(hops[0]["line"], 3, "entry point line should be 1-based");
    assert_eq!(last["line"], 5, "target line should be 1-based");

    // Verify diagnostic fields exist
    assert!(resp["total_paths"].as_u64().unwrap() >= 1);
    assert!(resp["entry_points_found"].as_u64().unwrap() >= 1);

    aft.shutdown();
}

/// `trace_to` on validate (called from multiple paths) returns multiple paths.
///
/// validate is called by:
/// - main (main.ts) → processData (utils.ts) → validate
/// - handleRequest (service.ts) → processData (utils.ts) → validate
/// - testValidation (test_helpers.ts) → validate (directly)
#[test]
fn callgraph_trace_to_multi_path() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_to","file":{},"symbol":"validate","depth":10}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(resp["success"], true, "trace_to should succeed: {:?}", resp);
    assert_eq!(resp["target_symbol"], "validate");

    let total_paths = resp["total_paths"].as_u64().unwrap();
    assert!(
        total_paths >= 2,
        "validate should have multiple paths to entry points, got {}",
        total_paths
    );

    let entry_points_found = resp["entry_points_found"].as_u64().unwrap();
    assert!(
        entry_points_found >= 2,
        "validate should have multiple distinct entry points, got {}",
        entry_points_found
    );

    let paths = resp["paths"].as_array().expect("paths should be array");

    // Collect entry point names (first hop of each path)
    let entry_names: Vec<&str> = paths
        .iter()
        .filter_map(|p| {
            let hops = p["hops"].as_array()?;
            hops.first().and_then(|h| h["symbol"].as_str())
        })
        .collect();

    // Should include both main and testValidation (or handleRequest) as entry points
    assert!(
        entry_names.contains(&"main") || entry_names.contains(&"handleRequest"),
        "should have main or handleRequest as entry point, got: {:?}",
        entry_names
    );

    // Each path should end at validate (the target)
    for path in paths {
        let hops = path["hops"].as_array().unwrap();
        let last = &hops[hops.len() - 1];
        assert_eq!(
            last["symbol"], "validate",
            "every path should end at validate"
        );
    }

    aft.shutdown();
}

/// `trace_to` on an entry point itself returns gracefully (no paths or self-path).
///
/// main is an entry point — there's nothing calling it, so trace_to should return
/// an empty paths array or a minimal self-path.
#[test]
fn callgraph_trace_to_no_entry_points() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_to","file":{},"symbol":"main","depth":10}}"#,
        crate::helpers::json_string(&format!("{}/main.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "trace_to on entry point should succeed: {:?}",
        resp
    );
    assert_eq!(resp["target_symbol"], "main");

    // main IS an entry point, so the result should handle it gracefully.
    // It may return a self-path (just [main]) or empty paths.
    let paths = resp["paths"].as_array().expect("paths should be array");

    // If there are paths, each should contain main
    for path in paths {
        let hops = path["hops"].as_array().unwrap();
        let has_main = hops.iter().any(|h| h["symbol"] == "main");
        assert!(has_main, "any path for main should include main");
    }

    // Diagnostic fields should be present and valid
    assert!(resp.get("total_paths").is_some());
    assert!(resp.get("entry_points_found").is_some());
    assert!(resp.get("truncated_paths").is_some());

    aft.shutdown();
}

#[test]
fn callgraph_default_import_targets_real_default_export_name() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path();
    let main = root.join("main.ts");
    let helper = root.join("helper.ts");

    fs::write(
        &main,
        "import helper from './helper';\n\nexport function main() {\n  return helper();\n}\n",
    )
    .expect("write main");
    fs::write(
        &helper,
        "export default function realName() {\n  return 1;\n}\n",
    )
    .expect("write helper");

    let mut aft = AftProcess::spawn();
    let root_str = root.display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root_str)
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"call_tree","file":{},"symbol":"main","depth":2}}"#,
        crate::helpers::json_string(&main.display())
    ));
    assert_eq!(resp["success"], true, "call_tree should succeed: {resp:?}");

    let children = resp["children"].as_array().expect("children array");
    let default_child = children
        .iter()
        .find(|child| {
            child["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("helper.ts"))
        })
        .expect("default import call should resolve into helper.ts");

    assert_eq!(default_child["resolved"], true);
    assert_eq!(default_child["name"], "realName");
    assert_ne!(default_child["name"], "default");

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// impact command
// ---------------------------------------------------------------------------

/// `impact` without prior `configure` returns not_configured error.
#[test]
fn callgraph_impact_not_configured() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(r#"{"id":"1","command":"impact","file":"helpers.ts","symbol":"validate"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `impact` on a nonexistent symbol returns symbol_not_found error.
#[test]
fn callgraph_impact_symbol_not_found() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"impact","file":{},"symbol":"nonexistent"}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(
        resp["success"], false,
        "unknown symbol should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

/// `impact` on validate (called from multiple files) returns enriched callers.
///
/// validate is called from:
/// - utils.ts (processData)
/// - test_helpers.ts (testValidation)
///
/// Should return callers with signatures, entry point flags, and call expressions.
#[test]
fn callgraph_impact_multi_caller() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"impact","file":{},"symbol":"validate","depth":5}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));

    assert_eq!(resp["success"], true, "impact should succeed: {:?}", resp);
    assert_eq!(resp["symbol"], "validate");
    assert!(resp["file"].as_str().unwrap().contains("helpers.ts"));

    // Should have at least 2 affected callers (processData in utils.ts + testValidation in test_helpers.ts)
    let total_affected = resp["total_affected"].as_u64().unwrap();
    assert!(
        total_affected >= 2,
        "validate should have at least 2 affected callers, got {}",
        total_affected
    );

    // Should have at least 2 affected files
    let affected_files = resp["affected_files"].as_u64().unwrap();
    assert!(
        affected_files >= 2,
        "validate should affect at least 2 files, got {}",
        affected_files
    );

    // Callers array should exist and be non-empty
    let callers = resp["callers"].as_array().expect("callers array");
    assert!(!callers.is_empty(), "callers should not be empty");

    // Each caller should have required fields
    for caller in callers {
        assert!(
            caller.get("caller_symbol").is_some(),
            "caller should have caller_symbol: {:?}",
            caller
        );
        assert!(
            caller.get("caller_file").is_some(),
            "caller should have caller_file: {:?}",
            caller
        );
        assert!(
            caller.get("line").is_some(),
            "caller should have line: {:?}",
            caller
        );
        assert!(
            caller["line"].as_u64().unwrap_or(0) >= 1,
            "caller line should be 1-based: {:?}",
            caller
        );
        assert!(
            caller.get("is_entry_point").is_some(),
            "caller should have is_entry_point: {:?}",
            caller
        );
        assert!(
            caller.get("parameters").is_some(),
            "caller should have parameters: {:?}",
            caller
        );
    }

    // At least one caller should have is_entry_point set
    // (testValidation starts with "test", processData is called by main which is an entry point)
    // With depth 5, we get transitive callers — main should be an entry point
    let has_entry_point = callers
        .iter()
        .any(|c| c["is_entry_point"].as_bool() == Some(true));
    assert!(
        has_entry_point,
        "at least one caller should be an entry point, callers: {:?}",
        callers
    );

    // Target signature should be present
    assert!(
        resp.get("signature").is_some(),
        "target should have a signature"
    );

    // Parameters should be present (validate takes `input: string`)
    let params = resp["parameters"].as_array().expect("parameters array");
    assert!(
        params.iter().any(|p| p.as_str() == Some("input")),
        "validate parameters should include 'input', got: {:?}",
        params
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// trace_data tests
// ---------------------------------------------------------------------------

/// `trace_data` without configure returns not_configured error.
#[test]
fn callgraph_trace_data_not_configured() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(
        r#"{"id":"1","command":"trace_data","file":"data_flow.ts","symbol":"transformData","expression":"rawInput"}"#,
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `trace_data` on a nonexistent symbol returns symbol_not_found error.
#[test]
fn callgraph_trace_data_symbol_not_found() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_data","file":{},"symbol":"nonexistent","expression":"x"}}"#,
        crate::helpers::json_string(&format!("{}/data_flow.ts", root))
    ));

    assert_eq!(
        resp["success"], false,
        "unknown symbol should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

/// `trace_data` tracks an expression through a local assignment within a function.
///
/// In data_flow.ts:
///   export function transformData(rawInput: string): string {
///       const cleaned = rawInput;   // assignment hop: rawInput → cleaned
///       const result = processInput(cleaned);  // parameter hop: cleaned → input in processInput
///       return result;
///   }
#[test]
fn callgraph_trace_data_assignment_tracking() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_data","file":{},"symbol":"transformData","expression":"rawInput","depth":5}}"#,
        crate::helpers::json_string(&format!("{}/data_flow.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "trace_data should succeed: {:?}",
        resp
    );
    assert_eq!(resp["expression"], "rawInput");
    assert!(
        resp["origin_file"]
            .as_str()
            .unwrap()
            .contains("data_flow.ts"),
        "origin_file should reference data_flow.ts"
    );
    assert_eq!(resp["origin_symbol"], "transformData");

    let hops = resp["hops"].as_array().expect("hops array");
    assert!(
        !hops.is_empty(),
        "should have at least one hop (assignment rawInput → cleaned)"
    );

    // First hop should be an assignment: rawInput → cleaned
    let first = &hops[0];
    assert_eq!(
        first["flow_type"], "assignment",
        "first hop should be assignment"
    );
    assert_eq!(first["variable"], "cleaned", "should track to 'cleaned'");
    assert_eq!(
        first["approximate"], false,
        "direct assignment is not approximate"
    );
    assert_eq!(first["line"], 4, "assignment line should be 1-based");

    aft.shutdown();
}

fn trace_data_hops(aft: &mut AftProcess, root: &str, request_id: &str, symbol: &str) -> Vec<Value> {
    let resp = aft.send(&format!(
        r#"{{"id":{},"command":"trace_data","file":{},"symbol":{},"expression":"raw","depth":5}}"#,
        crate::helpers::json_string(&request_id),
        crate::helpers::json_string(&format!("{root}/data_flow_kills.ts")),
        crate::helpers::json_string(&symbol),
    ));
    assert_eq!(resp["success"], true, "trace_data should succeed: {resp:?}");
    resp["hops"].as_array().expect("hops array").clone()
}

fn sink_parameter_hop(hops: &[Value]) -> Option<&Value> {
    hops.iter().find(|hop| {
        hop["symbol"] == "sink" && hop["variable"] == "input" && hop["flow_type"] == "parameter"
    })
}

#[test]
fn callgraph_trace_data_kills_only_dominating_straight_line_overwrites() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let overwritten = trace_data_hops(&mut aft, &root, "2", "overwritten");
    assert!(
        sink_parameter_hop(&overwritten).is_none(),
        "a straight-line overwrite must kill the stale flow: {overwritten:?}"
    );

    let derived = trace_data_hops(&mut aft, &root, "3", "derived");
    assert_eq!(
        sink_parameter_hop(&derived).map(|hop| &hop["approximate"]),
        Some(&Value::Bool(false)),
        "an assignment derived from raw must remain tracked: {derived:?}"
    );

    let before_overwrite = trace_data_hops(&mut aft, &root, "4", "usedBeforeOverwrite");
    assert_eq!(
        sink_parameter_hop(&before_overwrite).map(|hop| &hop["approximate"]),
        Some(&Value::Bool(false)),
        "a use before the overwrite must remain an exact flow: {before_overwrite:?}"
    );

    let conditional = trace_data_hops(&mut aft, &root, "5", "conditionallyOverwritten");
    assert_eq!(
        sink_parameter_hop(&conditional).map(|hop| &hop["approximate"]),
        Some(&Value::Bool(true)),
        "a possible branch overwrite must retain but qualify the flow: {conditional:?}"
    );

    let augmented = trace_data_hops(&mut aft, &root, "6", "augmented");
    assert_eq!(
        sink_parameter_hop(&augmented).map(|hop| &hop["approximate"]),
        Some(&Value::Bool(false)),
        "an augmented assignment derives from its tracked left side: {augmented:?}"
    );

    aft.shutdown();
}

/// `trace_data` tracks across file boundaries via argument-to-parameter matching.
///
/// In data_flow.ts, `transformData` calls `processInput(cleaned)`.
/// `processInput` is defined in data_processor.ts with parameter `input`.
/// So the flow should be: rawInput → cleaned (assignment) → input (parameter in processInput).
#[test]
fn callgraph_trace_data_cross_file() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_data","file":{},"symbol":"transformData","expression":"rawInput","depth":5}}"#,
        crate::helpers::json_string(&format!("{}/data_flow.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "trace_data should succeed: {:?}",
        resp
    );

    let hops = resp["hops"].as_array().expect("hops array");
    assert!(
        hops.len() >= 2,
        "should have at least 2 hops (assignment + cross-file parameter), got {}: {:?}",
        hops.len(),
        hops
    );

    // Should have a parameter hop pointing to data_processor.ts
    let has_param_hop = hops.iter().any(|h| {
        h["flow_type"] == "parameter"
            && h["file"]
                .as_str()
                .map(|f| f.contains("data_processor.ts"))
                .unwrap_or(false)
    });
    assert!(
        has_param_hop,
        "should have a parameter hop into data_processor.ts, hops: {:?}",
        hops
    );

    // Parameter hop should map to 'input' (processInput's first parameter)
    let param_hop = hops.iter().find(|h| {
        h["flow_type"] == "parameter"
            && h["file"]
                .as_str()
                .map(|f| f.contains("data_processor.ts"))
                .unwrap_or(false)
    });
    if let Some(ph) = param_hop {
        assert_eq!(
            ph["variable"], "input",
            "parameter should be 'input' (processInput's parameter)"
        );
        assert_eq!(ph["approximate"], false);
        assert_eq!(ph["line"], 1, "parameter line should be 1-based");
    }

    aft.shutdown();
}

/// `trace_data` marks destructuring as an approximate hop.
///
/// In data_flow.ts:
///   export function complexFlow(data: string): void {
///       const { name, value } = JSON.parse(data);  // destructuring — approximate
///   }
#[test]
fn callgraph_trace_data_approximation() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));

    let resp = aft.send(&format!(
        r#"{{"id":"2","command":"trace_data","file":{},"symbol":"complexFlow","expression":"data","depth":5}}"#,
        crate::helpers::json_string(&format!("{}/data_flow.ts", root))
    ));

    assert_eq!(
        resp["success"], true,
        "trace_data should succeed: {:?}",
        resp
    );

    let hops = resp["hops"].as_array().expect("hops array");

    // Should have at least one approximate hop (the destructuring)
    let has_approximate = hops.iter().any(|h| h["approximate"] == true);
    assert!(
        has_approximate,
        "destructuring should produce an approximate hop, hops: {:?}",
        hops
    );

    aft.shutdown();
}

/// Callgraph-backed commands reject absolute paths outside the configured
/// project_root instead of reporting an honest-looking empty graph. These files
/// are not indexed, so returning `success: true` with zero callers would be a
/// tri-state contract violation.
#[test]
fn callgraph_navigation_rejects_paths_outside_project_root() {
    let mut aft = AftProcess::spawn();
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    let outside = tempfile::tempdir().expect("create outside temp dir");
    let outside_file = outside.path().join("outside.ts");
    fs::write(
        &outside_file,
        r#"export function outside(value: string): string {
    const copied = value;
    return copied;
}
"#,
    )
    .expect("write outside file");
    let outside_path = outside_file.display().to_string();

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");

    let requests = [
        format!(
            r#"{{"id":"2","command":"callers","file":{},"symbol":"outside"}}"#,
            crate::helpers::json_string(&outside_path)
        ),
        format!(
            r#"{{"id":"3","command":"impact","file":{},"symbol":"outside"}}"#,
            crate::helpers::json_string(&outside_path)
        ),
        format!(
            r#"{{"id":"4","command":"trace_to","file":{},"symbol":"outside"}}"#,
            crate::helpers::json_string(&outside_path)
        ),
        format!(
            r#"{{"id":"5","command":"trace_data","file":{},"symbol":"outside","expression":"value"}}"#,
            crate::helpers::json_string(&outside_path)
        ),
    ];

    for request in requests {
        let resp = aft.send(&request);
        assert_eq!(
            resp["success"], false,
            "outside project_root request should fail: {request} -> {resp:?}"
        );
        assert_eq!(
            resp["code"], "path_outside_project_root",
            "outside project_root request should use path_outside_project_root: {request} -> {resp:?}"
        );
    }

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Async cold-build lifecycle (A1): the five ops never block the request thread
// on a cold build. With the inline-wait window disabled (`= 0`), the first op
// after configure returns `callgraph_building`; the store is installed via the
// drain loop and a subsequent op succeeds.
// ---------------------------------------------------------------------------

/// With pure-async cold build (wait window 0), the first callgraph op returns
/// `callgraph_building`, then retries succeed once the background build lands.
#[test]
fn callgraph_ops_return_building_then_ready_async() {
    // Disable the inline-wait window so the cold build is fully asynchronous.
    let mut aft = AftProcess::spawn_with_env(&[("AFT_CALLGRAPH_BUILD_WAIT_MS", OsStr::new("0"))]);
    let (_fixture, fixtures) = callgraph_fixture();
    let root = fixtures.display().to_string();

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");

    // Configure eagerly warms the store in the background. The first op must
    // never block on that build: it either returns the transient
    // `callgraph_building` signal (build still in flight) or a correct
    // synchronous result (eager build already landed — common on fast runners
    // with this small fixture). Both are valid; what is forbidden is a silent
    // empty/clean result while the build is in flight.
    let first = aft.send(&format!(
        r#"{{"id":"2","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    ));
    if first["success"] == true {
        assert_eq!(first["symbol"], "validate");
        assert!(
            first["total_callers"].as_u64().unwrap() > 0,
            "eagerly-built store should serve real callers: {first:?}"
        );
    } else {
        assert_eq!(
            first["code"], "callgraph_building",
            "first async op should report callgraph_building or succeed: {first:?}"
        );
    }

    // Retry until the background build lands and the store is installed via the
    // drain loop. Deadline-bounded so a genuine regression still fails the test,
    // but slow CI runners (Windows debug builds under parallel load) get the
    // headroom a fixed 5s retry count denied them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut succeeded = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let resp = aft.send(&format!(
            r#"{{"id":"3","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
            crate::helpers::json_string(&format!("{}/helpers.ts", root))
        ));
        if resp["success"] == true {
            assert_eq!(resp["symbol"], "validate");
            assert!(
                resp["total_callers"].as_u64().unwrap() > 0,
                "validate should have callers once the store is ready: {resp:?}"
            );
            succeeded = true;
            break;
        }
        // While building, the only acceptable non-success code is the transient
        // building signal — never a silent empty/clean result.
        assert_eq!(
            resp["code"], "callgraph_building",
            "op should be either ready or building, got: {resp:?}"
        );
    }
    assert!(
        succeeded,
        "callgraph store should finish building and serve callers within the retry window"
    );

    aft.shutdown();
}
