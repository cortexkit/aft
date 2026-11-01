#![cfg(debug_assertions)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness::{
    reset_verify_file_strict_count_for_debug, verify_file_strict_count_for_debug,
};
use aft::search_index::SearchIndex;
use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = crate::test_helpers::project::project_tempdir();
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }
    temp_dir
}

fn find_cache_bin(root: &Path) -> PathBuf {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read cache directory") {
            let entry = entry.expect("read cache entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name() == Some(OsStr::new("cache.bin")) {
                return path;
            }
        }
    }
    panic!("search cache.bin should exist under {}", root.display());
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn normalize_display_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn configure_search_index(aft: &mut AftProcess, root: &Path, id: &str) -> Value {
    let response = send(
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
    );
    if response["success"] == true {
        aft.wait_for_folder_admission();
    }
    response
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

#[test]
fn unchanged_head_warm_configure_reuses_verified_cache_without_rebuild_thread() {
    let project = setup_project(&[("src/lib.rs", "pub fn needle() -> usize { 1 }\n")]);
    let marker_dir = tempfile::tempdir().expect("create marker dir");
    let marker = marker_dir.path().join("rebuild-spawned");
    // Both processes must share one cache dir: the harness default mints a
    // fresh AFT_CACHE_DIR per spawn, which would leave the restarted process
    // with no disk cache to warm-load.
    let shared_cache = tempfile::tempdir().expect("create shared cache dir");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_SEARCH_REBUILD_THREAD_MARKER", marker.as_os_str()),
        ("AFT_CACHE_DIR", shared_cache.path().as_os_str()),
    ]);

    let first = configure_search_index(&mut aft, project.path(), "cfg-first");
    assert_eq!(first["success"], true, "configure failed: {first:?}");
    assert!(
        marker.exists(),
        "cold configure should exercise the marker hook"
    );
    wait_for_search_index_ready(&mut aft, Duration::from_secs(5));
    fs::remove_file(&marker).expect("remove cold-build marker");

    // An equivalent reconfigure in the SAME process is the zero-work rebind
    // path: the live, watcher-maintained index keeps serving, no verify or
    // rebuild thread spawns (re-verifying on every rebind was configure-storm
    // fuel: one long-lived session can rebind hundreds of times).
    let second = configure_search_index(&mut aft, project.path(), "cfg-second");
    assert_eq!(second["success"], true, "configure failed: {second:?}");
    assert_eq!(second["search_index_cache_reused"], true);
    assert!(
        !marker.exists(),
        "equivalent same-process rebind must not spawn index work"
    );
    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(5));
    assert_eq!(ready["search_index"]["status"], "ready");
    let status = aft.shutdown();
    assert!(status.success());
    let cache_bin = find_cache_bin(shared_cache.path());
    let cache_before_restart = fs::metadata(&cache_bin).expect("stat warm search cache");
    let cache_len_before_restart = cache_before_restart.len();
    let cache_mtime_before_restart = cache_before_restart.modified().expect("cache mtime");

    // A REAL warm restart (fresh process, unchanged HEAD) loads the disk cache
    // and verifies it on a background thread instead of inline on the dispatch
    // thread (verify_against_disk content-hashes every cached file, O(repo) —
    // blocking configure past the 30s transport timeout on a large repo).
    let mut restarted = AftProcess::spawn_with_env(&[
        ("AFT_TEST_SEARCH_REBUILD_THREAD_MARKER", marker.as_os_str()),
        ("AFT_CACHE_DIR", shared_cache.path().as_os_str()),
    ]);
    let warm = configure_search_index(&mut restarted, project.path(), "cfg-warm-restart");
    assert_eq!(warm["success"], true, "configure failed: {warm:?}");
    assert!(
        marker.exists(),
        "warm restart should verify the cached index on a background thread"
    );
    let ready = wait_for_search_index_ready(&mut restarted, Duration::from_secs(5));
    assert_eq!(ready["search_index"]["status"], "ready");
    let cache_after_restart = fs::metadata(&cache_bin).expect("stat reused search cache");
    assert_eq!(
        cache_after_restart.len(),
        cache_len_before_restart,
        "warm restart must reuse the persisted index, not rebuild it"
    );
    assert_eq!(
        cache_after_restart.modified().expect("cache mtime"),
        cache_mtime_before_restart,
        "warm restart must reuse the persisted index, not rewrite it"
    );
    let status = restarted.shutdown();
    assert!(status.success());
}

#[test]
fn search_index_ready_status_does_not_wait_for_symbol_prewarm() {
    let project = setup_project(&[("src/lib.rs", "pub fn searchable_symbol() -> usize { 7 }\n")]);
    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_SYMBOL_PREWARM_DELAY_MS", OsStr::new("5000"))]);

    let configure = configure_search_index(&mut aft, project.path(), "cfg-prewarm-delay");
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(3));
    assert_eq!(ready["search_index"]["status"], "ready");
    // Polling readiness while a 5s symbol prewarm delay is configured proves the
    // ready status is not gated on prewarm, without comparing wall-clock elapsed
    // time against the delay under shared CPU contention.

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
    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(10));
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
