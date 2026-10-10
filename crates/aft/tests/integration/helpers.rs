// Re-exported for both the `integration` and `watcher_integration` binaries,
// which share this module via `#[path]`. Each binary uses a different subset,
// so some re-exports are unused in one or the other.
#[allow(unused_imports)]
pub use crate::test_helpers::{
    canonicalize_like_product, cargo_manifest_dir, disable_in_process_file_watcher, fixture_path,
    real_rust_analyzer_available, thread_scratch_dir, user_config, user_config_tier,
    warm_executable, AftProcess, ReleaseOnDrop,
};

pub fn json_string(value: &impl std::fmt::Display) -> String {
    serde_json::to_string(&value.to_string()).unwrap()
}

/// Serializes the real-watcher tests within the `watcher_integration` binary so
/// at most one live `AftProcess` watcher exists at a time.
///
/// These tests live in their own test binary (cargo runs test binaries
/// sequentially, so it runs alone, with no concurrent `aft`-process load from
/// the ~1150-test `integration` binary). Under that concurrent load the macOS
/// `fseventsd` daemon was swamped and the watcher `watch()` call probabilistically
/// hung for ~1-in-3 watcher processes, so events were never delivered and the
/// tests timed out. Binary isolation removes that load; this lock is the
/// belt-and-suspenders within the isolated binary. Acquire it at the top of every
/// real-watcher test.
///
/// `#[allow(dead_code)]` because this module is `#[path]`-shared with the
/// `integration` binary, which no longer contains any watcher test.
#[allow(dead_code)]
pub fn watcher_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run a nonblocking `inspect` request, re-asking on the one refusal that is
/// the path's own contract rather than a finding.
///
/// The nonblocking inspect path gives Tier-1 scans a 1 s soft deadline; on a
/// contended runner even a two-line fixture can miss it and the product
/// answers `inspect_not_fresh` with `<category> did not complete (deadline
/// elapsed …)`, telling the caller to ask again. Three suites hit that shape
/// as a CI flake (specimens #13 and #16), so the re-ask lives here once. Every
/// other refusal (diagnostics prerequisites, producer failures) is returned
/// unchanged for the fixtures to assert on, and the loop is bounded by a
/// liveness deadline so a real stall still fails.
///
/// `#[allow(dead_code)]` because this module is `#[path]`-shared with the
/// `watcher_integration` binary, which has no inspect tests.
#[allow(dead_code)]
pub fn inspect_reasking_tier1_deadline(
    ctx: &aft::context::AppContext,
    payload: serde_json::Value,
) -> serde_json::Value {
    use std::time::{Duration, Instant};
    let liveness_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let request: aft::protocol::RawRequest =
            serde_json::from_value(payload.clone()).expect("inspect request parses");
        let response = aft::commands::inspect::handle_inspect(&request, ctx);
        let value = serde_json::to_value(response).expect("inspect response serializes");
        let tier1_deadline_miss = value["code"] == "inspect_not_fresh"
            && value["message"].as_str().is_some_and(|message| {
                message.contains(" did not complete") && message.contains("deadline elapsed")
            });
        if !tier1_deadline_miss || Instant::now() >= liveness_deadline {
            return value;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Retry a callgraph request only while its persisted reader is being installed.
/// Other responses are returned unchanged so each fixture can assert its own result.
#[allow(dead_code)] // Shared with watcher_integration, which has no callgraph tests.
pub fn callgraph_when_ready(
    operation: &str,
    mut query: impl FnMut() -> aft::protocol::Response,
) -> aft::protocol::Response {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = query();
        if response.data["code"] != "callgraph_building" {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "{operation}: callgraph did not become ready within 30s: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Observe readiness without delivering the result to a protocol session.
/// A terminal bash_status reply consumes a completion, so replay/reminder
/// fixtures must instead read the task's atomically published metadata.
#[cfg(unix)]
#[allow(dead_code)] // Shared with watcher_integration, which does not use bash fixtures.
pub fn wait_for_task_metadata(
    storage: &std::path::Path,
    harness: &str,
    task_id: &str,
    expected: &str,
) -> serde_json::Value {
    let root = storage.join(harness).join("bash-tasks");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(sessions) = std::fs::read_dir(&root) {
            for session in sessions.flatten() {
                let path = session.path().join(task_id).join("control/metadata.json");
                if let Ok(text) = std::fs::read_to_string(path) {
                    if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&text) {
                        if metadata["status"] == expected {
                            return metadata;
                        }
                    }
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task {task_id} never persisted status {expected}"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
