use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = crate::test_helpers::tempdir();
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }
    temp_dir
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn configure_restricted(aft: &mut AftProcess, root: &Path) {
    let response = send(
        aft,
        json!({
            "id": "cfg-restricted",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "config": crate::test_helpers::project_detection_user_config(serde_json::json!({ "restrict_to_project_root": true })),
        }),
    );
    assert_eq!(
        response["success"], true,
        "configure should succeed: {response:?}"
    );
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize path")
        .display()
        .to_string()
        .replace('\\', "/")
}

fn normalize_path_text(path: &str) -> String {
    path.replace('\\', "/")
}

fn match_files(response: &Value) -> HashSet<String> {
    response["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|entry| {
            entry["file"]
                .as_str()
                .expect("file path")
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn grep_multi_path_happy_path() {
    let project = setup_project(&[
        ("a/one.ts", "const value = 'needle';\n"),
        ("b/two.ts", "const value = 'needle';\n"),
        ("c/three.ts", "const value = 'needle';\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-multi-path",
            "command": "grep",
            "pattern": "needle",
            "path": "a b c",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 3);
    let files = match_files(&response);
    for relative in ["a/one.ts", "b/two.ts", "c/three.ts"] {
        let expected = canonical_path_string(&project.path().join(relative));
        assert!(
            files.contains(&expected),
            "missing {relative}: {response:?}"
        );
        let text = response["text"].as_str().expect("text").replace('\\', "/");
        assert!(
            text.contains(relative),
            "text should mention {relative}: {response:?}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_with_overlap_deduplicates_files() {
    let project = setup_project(&[
        ("src/root.ts", "const value = 'needle';\n"),
        ("src/features/feature.ts", "const value = 'needle';\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-overlap",
            "command": "grep",
            "pattern": "needle",
            "path": "src src/features",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 2);
    let matches = response["matches"].as_array().expect("matches array");
    let feature_path = canonical_path_string(&project.path().join("src/features/feature.ts"));
    assert_eq!(
        matches
            .iter()
            .filter(|entry| {
                entry["file"]
                    .as_str()
                    .is_some_and(|path| normalize_path_text(path) == feature_path)
            })
            .count(),
        1,
        "feature file should not be duplicated: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_keeps_sibling_dirs_with_shared_prefix() {
    let project = setup_project(&[
        (
            "packages/app/main.ts",
            "const value = 'shared-prefix-needle';\n",
        ),
        (
            "packages/app-old/main.ts",
            "const value = 'shared-prefix-needle';\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-sibling-prefix",
            "command": "grep",
            "pattern": "shared-prefix-needle",
            "path": "packages/app packages/app-old",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 2,
        "sibling directory with shared lexical prefix must not be dropped: {response:?}"
    );
    let files = match_files(&response);
    assert!(files.contains(&canonical_path_string(
        &project.path().join("packages/app/main.ts")
    )));
    assert!(files.contains(&canonical_path_string(
        &project.path().join("packages/app-old/main.ts")
    )));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_keeps_explicit_file_under_aftignored_parent() {
    let project = setup_project(&[
        (
            "vendored/sub.rs",
            "pub fn vendored() { let _ = \"needle\"; }\n",
        ),
        (".aftignore", "vendored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-under-aftignored-parent",
            "command": "grep",
            "pattern": "needle",
            "path": [".", "vendored/sub.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        "explicit file nested under an .aftignored directory must still be searched: {response:?}"
    );
    assert_eq!(
        normalize_path_text(response["matches"][0]["file"].as_str().expect("file path")),
        canonical_path_string(&project.path().join("vendored/sub.rs"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_falls_through_to_path_not_found_when_fragment_missing() {
    let project = setup_project(&[
        ("a/one.ts", "const value = 'needle';\n"),
        ("c/three.ts", "const value = 'needle';\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-missing-fragment",
            "command": "grep",
            "pattern": "needle",
            "path": "a b c",
        }),
    );

    assert_eq!(response["success"], false, "grep should fail: {response:?}");
    assert_eq!(response["code"], "path_not_found");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_legitimate_single_path_with_space_is_not_split() {
    let project = setup_project(&[("with space/file.ts", "const value = 'needle';\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-path-with-space",
            "command": "grep",
            "pattern": "needle",
            "path": "with space",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1);
    assert_eq!(
        normalize_path_text(response["matches"][0]["file"].as_str().expect("file path")),
        canonical_path_string(&project.path().join("with space/file.ts"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_rejects_outside_fragment_when_restricted() {
    let project = setup_project(&[("a/one.ts", "const value = 'needle';\n")]);
    let outside = crate::test_helpers::tempdir();
    fs::write(
        outside.path().join("outside.ts"),
        "const value = 'needle';\n",
    )
    .expect("write outside file");
    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-restricted-outside-fragment",
            "command": "grep",
            "pattern": "needle",
            "path": format!("{} {}", project.path().join("a").display(), outside.path().display()),
        }),
    );

    assert_eq!(response["success"], false, "grep should fail: {response:?}");
    assert_eq!(response["code"], "path_outside_root");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_multi_path_happy_path() {
    let project = setup_project(&[
        ("a/one.ts", "const one = true;\n"),
        ("b/two.ts", "const two = true;\n"),
        ("c/three.ts", "const three = true;\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-multi-path",
            "command": "glob",
            "pattern": "**/*.ts",
            "path": "a b c",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 3);
    let files: HashSet<String> = response["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|entry| normalize_path_text(entry.as_str().expect("file path")))
        .collect();
    for relative in ["a/one.ts", "b/two.ts", "c/three.ts"] {
        assert!(
            files.contains(&canonical_path_string(&project.path().join(relative))),
            "missing {relative}: {response:?}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_expands_space_separated_path_element() {
    let project = setup_project(&[
        ("src/a/one.ts", "console.log(one);\n"),
        ("src/b/two.ts", "console.log(two);\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "ast-search-multi-path-element",
            "command": "ast_search",
            "pattern": "console.log($ARG)",
            "lang": "typescript",
            "paths": ["src/a src/b"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "ast_search should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 2);
    let files = match_files(&response);
    assert!(files.contains(&canonical_path_string(&project.path().join("src/a/one.ts"))));
    assert!(files.contains(&canonical_path_string(&project.path().join("src/b/two.ts"))));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_multi_path_enforces_max_results_globally() {
    let mut fixtures = Vec::new();
    for dir in ["a", "b", "c"] {
        for index in 0..50 {
            fixtures.push((
                format!("{dir}/file_{index}.ts"),
                "const value = 'needle';\n".to_string(),
            ));
        }
    }
    let fixture_refs = fixtures
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let project = setup_project(&fixture_refs);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-global-max-results",
            "command": "grep",
            "pattern": "needle",
            "path": "a b c",
            "max_results": 100,
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 150);
    assert_eq!(
        response["matches"].as_array().expect("matches array").len(),
        100
    );
    // Every match is counted before the page is cut, so 150 matches are
    // complete, not truncated; the next page starts where this one ended.
    assert_eq!(response["truncated"], false);
    assert_eq!(response["next_offset"], 100);

    let status = aft.shutdown();
    assert!(status.success());
}
