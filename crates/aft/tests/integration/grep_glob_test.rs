use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

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

fn configure_with_index(aft: &mut AftProcess, root: &Path) {
    let resp = aft.send(&format!(
        r#"{{"id":"cfg-index","command":"configure","harness":"opencode","project_root":"{}","search_index":true}}"#,
        root.display()
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn wait_for_index_ready<F>(aft: &mut AftProcess, mut request: F) -> Value
where
    F: FnMut() -> Value,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;

    while Instant::now() < deadline {
        let response = send(aft, request());
        if response["index_status"] == "Ready" {
            return response;
        }

        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!(
        "search index should become ready within 10s; last response: {:?}",
        last_response
    );
}

#[test]
fn grep_rejects_invalid_regex_with_pattern_data() {
    let project = setup_project(&[("src/main.rs", "fn main() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-invalid-regex",
            "command": "grep",
            "pattern": "[",
        }),
    );

    assert_eq!(response["success"], false, "grep should fail: {response:?}");
    assert_eq!(response["code"], "invalid_pattern");
    assert_eq!(response["pattern"], "[");
    assert!(response["message"]
        .as_str()
        .expect("message")
        .contains("invalid regex"));

    let status = aft.shutdown();
    assert!(status.success());
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize path")
        .display()
        .to_string()
}

#[test]
fn grep_fallback_returns_relative_paths_and_counts() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
        ("notes.txt", "alpha beta gamma\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-fallback",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(response["total_matches"], 2);
    assert_eq!(response["files_with_matches"], 2);
    assert_eq!(response["files_searched"], 2);

    let matches = response["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0]["line"], 1);
    assert!(matches[0]["column"].as_u64().unwrap_or(0) >= 1);
    // Files are returned as absolute paths
    let file_path = matches[0]["file"].as_str().expect("file path");
    assert!(file_path.contains("src/one.rs") || file_path.contains("src/two.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-empty-scope",
            "command": "grep",
            "pattern": "anything",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_fallback_respects_gitignore_and_returns_absolute_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/skip.ts", "const skip = true;\n"),
        ("ignored.log", "secret\n"),
        (".gitignore", "*.log\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-fallback",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 1);
    let files = response["files"].as_array().expect("files array");
    assert_eq!(
        files,
        &vec![Value::String(canonical_path_string(
            &project.path().join("src/keep.rs")
        ))]
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-empty-scope",
            "command": "glob",
            "pattern": "**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_uses_index_when_configured() {
    let project = setup_project(&[
        ("src/search.rs", "fn search() { println!(\"needle\"); }\n"),
        ("src/other.rs", "fn other() { println!(\"haystack\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-indexed",
            "command": "grep",
            "pattern": "needle",
            "include": ["src/**/*.rs"],
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 1);
    assert_eq!(response["files_with_matches"], 1);
    assert_eq!(response["files_searched"], 1);
    // Files are returned as absolute paths
    let expected_path = canonical_path_string(&project.path().join("src/search.rs"));
    assert_eq!(response["matches"][0]["file"], expected_path);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_text_uses_relative_paths_and_compact_format() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-format",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    let text = response["text"].as_str().expect("grep text");
    // New format: relative paths, no decorators, line:text format
    assert!(text.contains("src/one.rs\n"));
    assert!(text.contains("src/two.rs\n"));
    // No decorators
    assert!(!text.contains("──"));
    // No "Line" prefix, no indentation
    assert!(text.contains("1: fn alpha()"));
    assert!(text.contains("1: fn beta()"));
    assert!(text.ends_with("Found 2 match(es) across 2 file(s). [index: fallback]"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_text_uses_relative_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/other.rs", "fn other() {}\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-format",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    let text = response["text"].as_str().expect("glob text");
    // Relative paths in text
    assert!(text.contains("src/keep.rs") || text.contains("src/other.rs"));
    // No absolute paths
    assert!(!text.contains("/private/"));
    assert!(text.starts_with("2 files matching src/**/*.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_line_anchors() {
    let project = setup_project(&[
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-anchor-fallback",
            "command": "grep",
            "pattern": "^## ",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    // README has two `## ` headings at line start; `src/lib.rs` has one but
    // on a non-first line, so multi_line anchors must match it too.
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_indexed_supports_line_anchors() {
    let project = setup_project(&[
        (".fixture-id", "grep_indexed_supports_line_anchors\n"),
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-anchor-indexed",
            "command": "grep",
            "pattern": "^## ",
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_treats_leading_dash_pattern_as_literal() {
    let project = setup_project(&[("notes.txt", "-foo\nbar\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-leading-dash",
            "command": "grep",
            "pattern": "-foo",
            "path": project.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1, "response: {response:?}");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_end_of_line_anchor() {
    let project = setup_project(&[("src/a.rs", "fn foo() {}\nfn bar() {}\nlet x = 1;\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Match lines ending with `{}` — requires `$` to act as line-end anchor.
    let response = send(
        &mut aft,
        json!({
            "id": "grep-eol",
            "command": "grep",
            "pattern": r"\{\}$",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}
