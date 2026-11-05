#![cfg(debug_assertions)]

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness::{
    reset_verify_file_strict_count_for_debug, verify_file_strict_count_for_debug,
};
use aft::search_index::SearchIndex;
use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

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

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn normalize_display_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn configure_search_index(aft: &mut AftProcess, root: &Path, id: &str) -> Value {
    send(
        aft,
        json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": true, "semantic": false }
            })),
        }),
    )
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status-search-index-warm-restart",
            "command": "status",
        }),
    )
}

fn wait_for_search_index_ready(aft: &mut AftProcess, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last_response = None;

    while Instant::now() < deadline {
        let response = status(aft);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if response["search_index"]["status"] == "ready" {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!(
        "search index should become ready within {:?}; last response: {:?}",
        timeout, last_response
    );
}

/// Sends one grep, which binds the root's data-layer checkout and starts its
/// build; status alone never does (see
/// `standalone_configure_builds_the_search_index_without_a_query`).
fn bind_search_index(aft: &mut AftProcess) {
    let grep = send(
        aft,
        json!({"id": "bind-search-index", "command": "grep", "pattern": "fn"}),
    );
    assert_eq!(grep["success"], true, "binding grep failed: {grep:?}");
}

/// The first query of a root binds its data-layer checkout, which builds the
/// search index; status never binds one. Configure asks for that build as it
/// binds the root, so status reports the search index building and then
/// ready with no query sent. A standalone AFT never asks: status reports the
/// index off (`not_bound`) until a query arrives.
#[test]
fn standalone_configure_builds_the_search_index_without_a_query() {
    let project = setup_project(&[("src/lib.rs", "pub fn searchable_symbol() -> usize { 7 }\n")]);
    let mut aft = AftProcess::spawn();

    let configure = configure_search_index(&mut aft, project.path(), "cfg-no-query");
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(60));
    assert_eq!(ready["search_index"]["status"], "ready");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn verify_file_mtimes_checks_each_cached_file_once() {
    let project = setup_project(&[
        ("src/lib.rs", "pub fn alpha() {}\n"),
        ("src/main.rs", "fn main() { alpha(); }\n"),
    ]);
    let mut index = SearchIndex::build(project.path());
    let cached_files = index
        .files
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .count();

    reset_verify_file_strict_count_for_debug();
    index.verify_against_disk_for_debug(None);

    assert_eq!(verify_file_strict_count_for_debug(), cached_files);
}

#[test]
fn aft_search_excludes_tests_before_the_visible_result_cap() {
    let mut production_content = "// filler filler filler filler\n".repeat(300_000);
    production_content.push_str("pub fn production_needle_cap_bug() -> bool { true }\n");
    let project = setup_project(&[
        ("zzz/production.rs", production_content.as_str()),
        (
            "src/small.rs",
            "pub fn normal_small_needle() -> bool { true }\n",
        ),
    ]);
    fs::create_dir_all(project.path().join("tests")).expect("create tests directory");
    for index in 0..400 {
        fs::write(
            project.path().join(format!("tests/case_{index:03}.rs")),
            "const SATURATION_SENTINEL: &str = \"needle_cap_bug\";\n",
        )
        .expect("write test fixture");
    }

    let mut aft = AftProcess::spawn_with_env(&[("RAYON_NUM_THREADS", OsStr::new("4"))]);
    let configure = configure_search_index(&mut aft, project.path(), "cfg-visible-cap");
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    bind_search_index(&mut aft);
    // How long the build takes is the runner's business; the claims below
    // are about the answers.
    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(120));
    assert_eq!(ready["search_index"]["status"], "ready");

    let production_only = send(
        &mut aft,
        json!({
            "id": "search-production-only",
            "command": "semantic_search",
            "query": "needle_.*cap_bug",
            "hint": "literal",
            "top_k": 10,
            "include_tests": false,
        }),
    );
    assert_eq!(
        production_only["success"], true,
        "aft_search failed: {production_only:?}"
    );
    let production_results = production_only["results"]
        .as_array()
        .expect("production results");
    assert_eq!(
        production_results.len(),
        1,
        "hidden test matches must not consume the visible cap: {production_only:?}"
    );
    assert!(production_results[0]["file"]
        .as_str()
        .is_some_and(|file| normalize_display_path(file).ends_with("/zzz/production.rs")));
    assert_eq!(production_only["engine_capped"], false);
    assert_eq!(production_only["more_available"], false);

    let including_tests = send(
        &mut aft,
        json!({
            "id": "search-including-tests",
            "command": "semantic_search",
            "query": "needle_.*cap_bug",
            "hint": "literal",
            "top_k": 10,
            "include_tests": true,
        }),
    );
    assert_eq!(including_tests["success"], true);
    // The regex route examines all 401 candidate files (well inside its file
    // bound) and ranks them before paging, so nothing is cut by a budget; the
    // rest are simply on later pages.
    assert_eq!(including_tests["engine_capped"], false);
    assert_eq!(including_tests["more_available"], true);
    assert!(including_tests["results"]
        .as_array()
        .expect("results including tests")
        .iter()
        .any(|result| result["file"]
            .as_str()
            .is_some_and(|file| normalize_display_path(file).contains("/tests/"))));

    let generic_grep = send(
        &mut aft,
        json!({
            "id": "generic-grep-tests",
            "command": "grep",
            "pattern": "needle_cap_bug",
            "max_results": 10,
        }),
    );
    assert_eq!(generic_grep["success"], true);
    assert!(generic_grep["matches"]
        .as_array()
        .expect("generic grep matches")
        .iter()
        .any(|result| result["file"]
            .as_str()
            .is_some_and(|file| normalize_display_path(file).contains("/tests/"))));

    let normal_search = send(
        &mut aft,
        json!({
            "id": "normal-small-search",
            "command": "semantic_search",
            "query": "normal_.*needle",
            "hint": "literal",
            "top_k": 10,
            "include_tests": false,
        }),
    );
    assert_eq!(normal_search["success"], true);
    assert!(normal_search["results"]
        .as_array()
        .expect("normal search results")
        .iter()
        .any(|result| result["file"]
            .as_str()
            .is_some_and(|file| normalize_display_path(file).ends_with("/src/small.rs"))));

    let status = aft.shutdown();
    assert!(status.success());
}
