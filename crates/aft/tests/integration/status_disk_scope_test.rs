//! Tests for the per-project scoping of `disk.trigram_disk_bytes` and
//! `disk.semantic_disk_bytes` in the `status` command response.
//!
//! Regression: before this fix, status reported `dir_size("<storage>/index")`
//! and `dir_size("<storage>/semantic")` recursively, which summed disk usage
//! across **every** project the user had ever opened. The TUI sidebar then
//! displayed that cross-project total as if it were the current project's
//! footprint (e.g. a 4 MB project showed 16 GB because a sibling project's
//! cache was huge). Status must scope to the current project's slice using
//! `project_cache_key(project_root)`.

use std::fs;
use std::path::PathBuf;

use aft::search_index::project_cache_key;
use serde_json::json;
use tempfile::tempdir;

use crate::test_helpers::AftProcess;

/// Write a fake index/semantic blob under the project's cache key so we can
/// observe a non-zero per-project slice without running real indexing.
fn write_fake_cache_for_project(
    storage_root: &std::path::Path,
    project_root: &std::path::Path,
    trigram_bytes: usize,
    semantic_bytes: usize,
) {
    let key = project_cache_key(project_root);

    let trigram_dir = storage_root.join("index").join(&key);
    fs::create_dir_all(&trigram_dir).expect("create trigram dir");
    fs::write(trigram_dir.join("postings.bin"), vec![0u8; trigram_bytes])
        .expect("write fake postings");

    let semantic_dir = storage_root.join("semantic").join(&key);
    fs::create_dir_all(&semantic_dir).expect("create semantic dir");
    fs::write(semantic_dir.join("semantic.bin"), vec![0u8; semantic_bytes])
        .expect("write fake semantic");
}

#[test]
fn status_disk_bytes_only_count_current_project() {
    // Set up two fake project caches under one shared storage_dir, where
    // project A is what we'll configure aft against and project B is a
    // sibling that should NOT contribute to A's disk numbers.
    let storage_root_dir = tempdir().expect("storage root");
    let storage_root = storage_root_dir.path().to_path_buf();

    let project_a_dir = tempdir().expect("project a");
    let project_a = project_a_dir.path().to_path_buf();
    fs::create_dir_all(project_a.join("src")).expect("project a src");
    fs::write(project_a.join("src/lib.rs"), "pub fn a() {}\n").expect("project a file");

    let project_b_dir = tempdir().expect("project b");
    let project_b = project_b_dir.path().to_path_buf();

    // Ensure the two projects have distinct cache keys (they should, because
    // they have distinct canonical paths). Sanity check this — if it ever
    // fails the test logic falls apart.
    let key_a = project_cache_key(&project_a);
    let key_b = project_cache_key(&project_b);
    assert_ne!(
        key_a, key_b,
        "projects {project_a:?} and {project_b:?} unexpectedly share cache key {key_a}"
    );

    // Project A: small slice (1 KB trigram + 512 B semantic).
    write_fake_cache_for_project(&storage_root, &project_a, 1024, 512);
    // Project B: enormous slice (10 MB trigram + 5 MB semantic). Must NOT
    // appear in A's status response.
    write_fake_cache_for_project(&storage_root, &project_b, 10 * 1024 * 1024, 5 * 1024 * 1024);

    let mut aft = AftProcess::spawn();
    let configure = json!({
        "id": "1",
        "command": "configure",
            "harness": "opencode",
        "project_root": project_a.to_str().expect("project a utf-8"),
        "storage_dir": storage_root.to_str().expect("storage utf-8"),
    });
    let response = aft.send(&configure.to_string());
    assert_eq!(response["success"], true, "configure failed: {response}");

    // Drain the configure_warnings push frame.
    let _ = aft.try_read_next_timeout(std::time::Duration::from_secs(2));

    let status = aft.send(r#"{"id":"2","command":"status"}"#);
    assert_eq!(status["success"], true, "status failed: {status}");

    let trigram = status["disk"]["trigram_disk_bytes"]
        .as_u64()
        .expect("trigram_disk_bytes is u64");
    let semantic = status["disk"]["semantic_disk_bytes"]
        .as_u64()
        .expect("semantic_disk_bytes is u64");

    // Project A's slice only.
    assert_eq!(
        trigram, 1024,
        "trigram_disk_bytes should reflect only project A's slice; \
         got {trigram} (sibling B has 10 MB which would be visible without scoping)"
    );
    assert_eq!(
        semantic, 512,
        "semantic_disk_bytes should reflect only project A's slice; \
         got {semantic} (sibling B has 5 MB which would be visible without scoping)"
    );

    // The status response must also expose the project_cache_key so the
    // host can correlate disk numbers with cache directories.
    assert_eq!(
        status["disk"]["project_cache_key"].as_str(),
        Some(key_a.as_str()),
        "status should include project_cache_key for the configured project"
    );
}

#[test]
fn status_disk_bytes_zero_when_no_cache_for_project() {
    // A project with no entries under either index or semantic caches should
    // report zero bytes — not the cross-project total.
    let storage_root_dir = tempdir().expect("storage root");
    let storage_root = storage_root_dir.path().to_path_buf();

    let project_dir = tempdir().expect("project");
    let project_root = project_dir.path().to_path_buf();
    fs::create_dir_all(project_root.join("src")).expect("project src");
    fs::write(project_root.join("src/lib.rs"), "pub fn x() {}\n").expect("project file");

    // Populate a sibling cache under storage_root that we should NOT see.
    let sibling_dir = tempdir().expect("sibling project");
    write_fake_cache_for_project(&storage_root, sibling_dir.path(), 4096, 2048);

    let mut aft = AftProcess::spawn();
    let configure = json!({
        "id": "1",
        "command": "configure",
            "harness": "opencode",
        "project_root": project_root.to_str().expect("utf-8"),
        "storage_dir": storage_root.to_str().expect("utf-8"),
    });
    let response = aft.send(&configure.to_string());
    assert_eq!(response["success"], true);

    let _ = aft.try_read_next_timeout(std::time::Duration::from_secs(2));

    let status = aft.send(r#"{"id":"2","command":"status"}"#);
    assert_eq!(status["disk"]["trigram_disk_bytes"], 0);
    assert_eq!(status["disk"]["semantic_disk_bytes"], 0);
}

/// Type-only sanity check that the public `project_cache_key` import path
/// stays stable. If this stops compiling, callers in `commands/status.rs`
/// (and downstream callers) need to update.
#[allow(dead_code)]
fn _api_compile_check() -> String {
    project_cache_key(&PathBuf::from("/tmp/whatever"))
}
