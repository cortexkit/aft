use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness;
use aft::callgraph_store::CallGraphStore;
use aft::commands::configure::handle_configure;
use aft::commands::inspect::{
    handle_inspect, handle_inspect_tier2_run, handle_inspect_tool_call,
    handle_inspect_warm_for_test,
};
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::inspect::{
    inspect_phase_log_for_request, FileContribution, InspectCache, InspectCategory, InspectManager,
    InspectPhaseId, InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope,
};
use aft::lsp::client::LspEvent;
use aft::lsp::registry::ServerKind;
use aft::parser::{SymbolCache, TreeSitterProvider};
use aft::protocol::RawRequest;
use serde_json::{json, Value};

fn fixture_project() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");
    (temp_dir, root)
}

fn fake_server_path() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_fake_lsp_server")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_fake-lsp-server"))
        .map(PathBuf::from)
        .or_else(|| {
            option_env!("CARGO_BIN_EXE_fake-lsp-server")
                .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
                .map(PathBuf::from)
        })
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn write_file(root: &Path, relative_path: &str, contents: &str) -> PathBuf {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(&path, contents).expect("write fixture file");
    path
}

fn file_uri(path: &Path) -> String {
    let canonical = crate::helpers::canonicalize_like_product(path);
    url::Url::from_file_path(canonical)
        .expect("file URL")
        .to_string()
}

fn collect_lsp_notifications(ctx: &AppContext, method: &str, expected: usize) -> Vec<Value> {
    // Notification count is the assertion; this only catches a wedged fake LSP.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut notifications = Vec::new();

    while Instant::now() < deadline && notifications.len() < expected {
        let events = ctx.lsp().drain_events().events;
        for event in events {
            if let LspEvent::Notification {
                method: event_method,
                params: Some(params),
                ..
            } = event
            {
                if event_method == method {
                    notifications.push(params);
                }
            }
        }
        if notifications.len() < expected {
            thread::sleep(Duration::from_millis(20));
        }
    }

    assert_eq!(
        notifications.len(),
        expected,
        "expected {expected} {method} notifications"
    );
    notifications
}

fn request(payload: Value) -> RawRequest {
    serde_json::from_value(payload).expect("request parses")
}

fn wait_for_path_event(path: &Path, event: &str) {
    let hang_deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < hang_deadline,
            "timed out waiting for {event}: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn env_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            unsafe { std::env::set_var(self.key, value) };
        } else {
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

fn configured_context(root: &Path) -> AppContext {
    configured_context_with_callgraph_store(root, false)
}

fn configured_context_with_callgraph_store(root: &Path, callgraph_store: bool) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        },
    );
    // Libtest runs these independent contexts in one process, whereas nextest
    // gives each test a process and therefore a separate production limiter.
    // Preserve the production cap within each context without cross-test denial.
    ctx.isolate_cold_build_limiter_for_test(2);
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": callgraph_store
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

/// `configured_context` with an explicit whole-inspect deadline. The server
/// reserves terminal-egress time inside this value, and timeout detail still
/// carries the configured budget.
fn configured_context_with_diagnostics_timeout(root: &Path, timeout_ms: u64) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        },
    );
    ctx.isolate_cold_build_limiter_for_test(2);
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": false,
            "inspect": { "diagnostics_timeout_ms": timeout_ms }
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

fn drain_callgraph_store_for_test(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };
        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(store) => latest = Some(store),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    if let Some(store) = latest {
        drop(store);
        if let Some(project_root) = ctx.callgraph_project_root() {
            let store = CallGraphStore::open_readonly(ctx.callgraph_store_dir(), project_root)
                .expect("open read-only callgraph store")
                .expect("ready callgraph store");
            *ctx.callgraph_store()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(std::sync::Arc::new(store));
        }
        *ctx.callgraph_store_rx().lock() = None;
    } else if disconnected {
        *ctx.callgraph_store_rx().lock() = None;
    }
}

fn ensure_callgraph_store_ready(ctx: &AppContext) {
    // Generous hang-catch deadline: this only guards against a wedged cold
    // build, it is NOT a correctness assertion (those come after readiness).
    // The callgraph cold build is a heavy tree-sitter parse that can take far
    // longer than the happy-path <1s on a loaded Windows CI runner during a
    // release with parallel jobs — 10s flaked there. 90s matches the
    // callgraph_test.rs precedent.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match ctx.callgraph_store_for_ops() {
            CallgraphStoreAccess::Ready(_) => return,
            CallgraphStoreAccess::Building => {
                drain_callgraph_store_for_test(ctx);
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for callgraph store cold build"
                );
                thread::sleep(Duration::from_millis(10));
            }
            CallgraphStoreAccess::Suspended(suspension) => {
                panic!("callgraph store unexpectedly suspended in test: {suspension:?}")
            }
            CallgraphStoreAccess::Unavailable => {
                panic!("callgraph store unexpectedly unavailable in test")
            }
            CallgraphStoreAccess::Error(error) => {
                panic!("callgraph store failed in test: {error}")
            }
        }
    }
}

fn inspect(ctx: &AppContext, payload: Value) -> Value {
    let diagnostics_requested = payload
        .get("sections")
        .is_some_and(|sections| match sections {
            Value::String(section) => section == "diagnostics" || section == "all",
            Value::Array(sections) => sections
                .iter()
                .any(|section| matches!(section.as_str(), Some("diagnostics" | "all"))),
            _ => false,
        });
    if !diagnostics_requested {
        if payload.get("scope").is_none() {
            // These scanner-focused fixtures are not assertions about LSP
            // startup. Seed an empty checked-clean report so they exercise a
            // complete diagnostics prerequisite rather than the new terminal
            // non-fresh branch owned by the diagnostics fixtures below.
            let root = ctx
                .config()
                .project_root
                .clone()
                .expect("configured project root");
            ctx.lsp()
                .diagnostics_store_mut_for_test()
                .publish_with_kind(
                    ServerKind::Rust,
                    root.join(".aft-test-authoritative-diagnostics"),
                    Vec::new(),
                );
        } else {
            // Scoped scanner fixtures need their real scope preserved. The
            // diagnostics prerequisite for an unanalyzed scope comes from
            // per-file coverage gaps (complete: false), so these fixtures
            // still reach a fresh diagnostics payload without warming LSP.
            ctx.lsp()
                .override_binary(ServerKind::Rust, fake_server_path());
            ctx.lsp()
                .override_binary(ServerKind::TypeScript, fake_server_path());
            ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
        }
    }
    // The harness runs the nonblocking path, whose Tier-1 scans carry a 1 s
    // soft deadline; under runner contention a two-line fixture can miss it
    // and the product answers `inspect_not_fresh` with a Tier-1 "did not
    // complete" message. That is the path's contract (the caller re-asks), not
    // a scanner defect, so the harness re-asks under a liveness bound. Other
    // not-fresh causes (diagnostics prerequisites) are returned unchanged:
    // fixtures below assert on them.
    let liveness_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = handle_inspect(&request(payload.clone()), ctx);
        let value = serde_json::to_value(response).expect("inspect response serializes");
        let tier1_deadline_miss = value["code"] == "inspect_not_fresh"
            && value["message"].as_str().is_some_and(|message| {
                (message.starts_with("metrics did not complete")
                    || message.starts_with("todos did not complete"))
                    && message.contains("deadline elapsed")
            });
        if !tier1_deadline_miss || Instant::now() >= liveness_deadline {
            return value;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn inspect_warm_event_driven(ctx: &AppContext, payload: Value) -> Value {
    let response = handle_inspect_warm_for_test(&request(payload), ctx);
    serde_json::to_value(response).expect("event-driven warm inspect response serializes")
}

fn enqueue_tier2_run(ctx: &AppContext, categories: &[&str]) -> Value {
    let response = handle_inspect_tier2_run(
        &request(json!({
            "id": "tier2-run",
            "command": "inspect_tier2_run",
            "categories": categories,
        })),
        ctx,
    );
    let value = serde_json::to_value(response).expect("tier2_run response serializes");
    assert_eq!(value["success"], true, "tier2_run failed: {value:#}");
    value
}

fn tier2_run(ctx: &AppContext, categories: &[&str]) {
    if categories.contains(&"dead_code") {
        ensure_callgraph_store_ready(ctx);
    }
    let submission = enqueue_tier2_run(ctx, categories);
    let in_flight = submission["in_flight_categories"]
        .as_array()
        .unwrap_or_else(|| panic!("Tier-2 submission has no in-flight list: {submission:#}"));
    for category in categories {
        assert!(
            in_flight.iter().any(|queued| queued == category),
            "Tier-2 submission did not queue {category}: {submission:#}"
        );
    }
    assert!(
        submission["errors"].as_array().is_some_and(Vec::is_empty),
        "Tier-2 submission failed: {submission:#}"
    );
    wait_for_tier2(ctx, categories);
}

fn wait_for_tier2(ctx: &AppContext, categories: &[&str]) {
    let manager = ctx.inspect_manager();
    // The in-flight registry is installed before the worker starts and removed
    // only after its terminal outcome is published. Wait on that lifecycle
    // event instead of using an unrelated inspect request's one-second Tier-1
    // soft deadline as a proxy for Tier-2 completion.
    let hang_deadline = Instant::now() + Duration::from_secs(90);
    while manager.tier2_any_in_flight() {
        assert!(
            Instant::now() < hang_deadline,
            "timed out waiting for Tier-2 completion event for {categories:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let config = ctx.config();
    let root = config
        .project_root
        .clone()
        .expect("configured project root");
    let snapshot = InspectSnapshot::new_with_capabilities(
        root.clone(),
        ctx.inspect_dir(),
        config,
        ctx.symbol_cache(),
        ctx.inspect_writer(),
        ctx.callgraph_writer(),
    );
    let scope = JobScope::for_project(root);
    for category in categories {
        let category = category
            .parse::<InspectCategory>()
            .unwrap_or_else(|error| panic!("invalid Tier-2 category {category}: {error}"));
        let outcome = manager.tier2_read_cached(snapshot.clone(), category, scope.clone());
        assert!(
            matches!(outcome, JobOutcome::Fresh { .. }),
            "completed Tier-2 category {category} must be fresh: {outcome:?}"
        );
    }
}

fn assert_summary_count(response: &Value, category: &str, count: u64) {
    let summary = response["summary"][category]
        .as_object()
        .unwrap_or_else(|| panic!("{category} summary object: {response:#}"));
    assert_eq!(
        summary.get("count").and_then(Value::as_u64),
        Some(count),
        "{category} summary should carry count={count}: {response:#}"
    );
    assert!(
        !summary.contains_key("status"),
        "{category} computed summary should not carry a status sentinel: {response:#}"
    );
}

#[test]
fn inspect_command_todos_summary_uses_production_dispatch() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/app.ts",
        "// TODO: assert production dispatch reaches todos scanner\nexport function app() { return 1; }\n",
    );
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-todos",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["todos"]["count"]
        .as_u64()
        .expect("todos count");
    assert!(count > 0, "todos scanner should be reachable: {response:#}");
}

#[test]
fn inspect_command_metrics_summary_uses_production_dispatch() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.rs",
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() }\n",
    );
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-metrics",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let files = response["summary"]["metrics"]["files"]
        .as_u64()
        .expect("metrics files");
    assert!(
        files > 0,
        "metrics scanner should count files: {response:#}"
    );
    let metrics = response["summary"]["metrics"]
        .as_object()
        .expect("metrics summary object");
    assert!(
        !metrics.contains_key("status"),
        "Tier-1 metrics should be computed, not status-only: {response:#}"
    );
    assert_summary_count(&response, "todos", 0);
}

#[cfg(debug_assertions)]
#[test]
fn inspect_command_tier1_reuses_file_memo_for_unchanged_files() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/app.ts",
        "// TODO: keep cached\nexport function app() { return 1; }\n",
    );
    write_file(&root, "src/lib.ts", "export function lib() { return 2; }\n");
    let ctx = configured_context(&root);

    let first = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-cold",
            "command": "inspect",
        }),
    );
    assert_eq!(first["success"], true, "inspect failed: {first:#}");

    aft::inspect::scanners::metrics::reset_file_read_count_for_debug(&root);
    aft::inspect::scanners::todos::reset_file_read_count_for_debug(&root);

    let second = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-warm",
            "command": "inspect",
        }),
    );

    assert_eq!(second["success"], true, "inspect failed: {second:#}");
    assert_eq!(
        aft::inspect::scanners::metrics::file_read_count_for_debug(&root),
        0,
        "warm metrics scan should reuse unchanged per-file memo entries: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::todos::file_read_count_for_debug(&root),
        0,
        "warm todos scan should reuse unchanged per-file memo entries: {second:#}"
    );
    assert_eq!(first["summary"]["metrics"], second["summary"]["metrics"]);
    assert_eq!(first["summary"]["todos"], second["summary"]["todos"]);
}

#[cfg(debug_assertions)]
#[test]
fn inspect_command_tier1_changed_file_invalidates_only_that_file() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/unchanged.ts",
        "// TODO: already counted\nexport function unchanged() { return 1; }\n",
    );
    write_file(
        &root,
        "src/changed.ts",
        "export function changed() { return 2; }\n",
    );
    let ctx = configured_context(&root);

    let first = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-before-change",
            "command": "inspect",
        }),
    );
    assert_eq!(first["success"], true, "inspect failed: {first:#}");
    assert_eq!(first["summary"]["todos"]["count"], 1);

    aft::inspect::scanners::metrics::reset_file_read_count_for_debug(&root);
    aft::inspect::scanners::todos::reset_file_read_count_for_debug(&root);

    write_file(
        &root,
        "src/changed.ts",
        "// TODO: newly counted after memo invalidation\nexport function changed() { return 2; }\n",
    );

    let second = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-after-change",
            "command": "inspect",
        }),
    );

    assert_eq!(second["success"], true, "inspect failed: {second:#}");
    assert_eq!(
        second["summary"]["todos"]["count"], 2,
        "changed file's TODO should update the Tier 1 summary: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::metrics::file_read_count_for_debug(&root),
        1,
        "metrics should rescan only the changed file: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::todos::file_read_count_for_debug(&root),
        1,
        "todos should rescan only the changed file: {second:#}"
    );
}

#[test]
fn inspect_command_dead_code_uses_callgraph_snapshot_and_details() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "import { used } from './lib';\nused();\n",
    );
    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    // aft_inspect never scans Tier 2 categories synchronously. Tier 2 scans run
    // via aft_inspect_tier2_run on session.idle in production. Simulate that
    // here so the cached aggregate is populated before the read-only inspect
    // call.
    tier2_run(&ctx, &["dead_code"]);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["dead_code"]["count"]
        .as_u64()
        .expect("dead_code count");
    assert!(
        count > 0,
        "dead_code should report fixture's intentionally dead export: {response:#}"
    );

    let details = response["details"]["dead_code"]
        .as_array()
        .expect("dead_code details array");
    assert!(
        details.iter().any(|item| item["symbol"] == "unused"),
        "dead_code details should include unused export: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_cold_direct_computes_before_deadline() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context(&root);

    // No tier2_run call: an explicit inspect now waits for a direct Tier-2
    // reuse pass and returns the fresh result when the scan finishes before the
    // direct-inspect deadline.
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-tier2-cold",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_summary_count(&response, "unused_exports", 2);
    assert_summary_count(&response, "duplicates", 0);
    assert_summary_count(&response, "cycles", 0);
}

#[test]
fn inspect_tier2_run_returns_promptly_with_background_in_flight() {
    let (_temp_dir, root) = fixture_project();
    for index in 0..40 {
        write_file(
            &root,
            &format!("src/file_{index:03}.ts"),
            &format!(
                "export function unused_{index}() {{ return {index}; }}
"
            ),
        );
    }
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);

    let response = enqueue_tier2_run(&ctx, &["dead_code"]);

    // Queue/in-flight state is the load-resistant promptness contract: if the
    // command scanned inline, the category would not still be marked in flight.
    // A wall-clock bound here flaked under shared CPU contention.
    assert!(
        response["queued_categories"]
            .as_array()
            .expect("queued_categories array")
            .iter()
            .any(|category| category.as_str() == Some("dead_code")),
        "dead_code should be queued: {response:#}"
    );
    assert!(
        response["in_flight_categories"]
            .as_array()
            .expect("in_flight_categories array")
            .iter()
            .any(|category| category.as_str() == Some("dead_code")),
        "dead_code should be marked in-flight: {response:#}"
    );

    wait_for_tier2(&ctx, &["dead_code"]);
}

fn duplicate_fixture_source() -> &'static str {
    r#"
export function calculate(input: number) {
  const first = input + 1;
  const second = first + 2;
  const third = second + first;
  const fourth = third + 3;
  const fifth = fourth + third;
  const sixth = fifth + second;
  const seventh = sixth + fifth;
  return seventh + third;
}
"#
}

fn tier2_snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

fn dead_code_tier2_snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        callgraph_store: true,
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

fn artifact_cache_key_for_test(project_root: &std::path::Path) -> String {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    aft::search_index::artifact_cache_key(project_root)
}

#[test]
fn inspect_dead_code_reuse_reports_unavailable_when_store_not_ready() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let project_key = artifact_cache_key_for_test(&root);
    let callgraph_dir = inspect_dir
        .parent()
        .expect("storage dir")
        .join("callgraph")
        .join(&project_key);
    aft::root_cache::configure_artifact_access(&root, &project_key, false);
    let _not_ready_store =
        CallGraphStore::open(callgraph_dir, root.clone()).expect("open non-ready callgraph store");

    let manager = InspectManager::new();
    let success = manager
        .tier2_run_with_reuse_result(
            dead_code_tier2_snapshot(&root, &inspect_dir),
            InspectCategory::DeadCode,
            None,
        )
        .outcome
        .expect("dead_code unavailable aggregate succeeds");

    assert!(
        success.contributions.is_empty(),
        "unavailable callgraph must not fabricate per-file dead_code contributions"
    );
    assert_eq!(success.aggregate["callgraph_available"], false);
    assert_eq!(success.aggregate["notes"], json!(["callgraph_unavailable"]));
    assert!(
        success.aggregate.get("count").is_none(),
        "unavailable callgraph must not be represented as zero dead code"
    );
}

#[test]
fn inspect_dead_code_fresh_result_has_no_unavailable_status() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/main.ts",
        "export function live() { return 1; }\n",
    );
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-fresh",
            "command": "inspect",
            "sections": ["dead_code"],
        }),
    );

    assert_eq!(response["success"], true, "response: {response:#}");
    assert!(response["summary"]["dead_code"].get("status").is_none());
    assert!(response["summary"]["dead_code"].get("stale").is_none());
}

fn run_duplicates_reuse(
    manager: &InspectManager,
    project_root: &Path,
    inspect_dir: &Path,
) -> InspectScanSuccess {
    manager
        .tier2_run_with_reuse_result(
            tier2_snapshot(project_root, inspect_dir),
            InspectCategory::Duplicates,
            None,
        )
        .outcome
        .expect("duplicates tier2 reuse run succeeds")
}

fn relative_scanned_paths(project_root: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| {
            file.strip_prefix(project_root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn duplicate_aggregate_mentions_file(success: &InspectScanSuccess, file_prefix: &str) -> bool {
    success.aggregate["items"].as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            group["files"].as_array().is_some_and(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|file| file.starts_with(file_prefix))
            })
        })
    })
}

#[test]
fn inspect_command_tier2_quick_reuse_is_path_aware_after_rename() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let first = run_duplicates_reuse(&first_manager, &root, &inspect_dir);
    assert_eq!(first.scanned_files.len(), 2);
    assert!(duplicate_aggregate_mentions_file(&first, "src/foo.ts:"));
    assert!(duplicate_aggregate_mentions_file(&first, "src/bar.ts:"));

    fs::rename(root.join("src/foo.ts"), root.join("src/baz.ts")).expect("rename fixture file");

    let second_manager = InspectManager::new();
    let second = run_duplicates_reuse(&second_manager, &root, &inspect_dir);

    assert_eq!(
        relative_scanned_paths(&root, &second.scanned_files),
        vec!["src/baz.ts"],
        "rename should invalidate quick reuse and rescan the new path"
    );
    assert!(duplicate_aggregate_mentions_file(&second, "src/baz.ts:"));
    assert!(duplicate_aggregate_mentions_file(&second, "src/bar.ts:"));
    assert!(
        !duplicate_aggregate_mentions_file(&second, "src/foo.ts:"),
        "renamed path must not leak from the stale aggregate"
    );
}

#[test]
fn inspect_command_tier2_quick_reuse_skips_rescan_for_unchanged_file_set() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let manager = Arc::new(InspectManager::new());

    let first = run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir);
    assert_eq!(first.scanned_files.len(), 2);

    let second = run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir);
    assert!(
        second.scanned_files.is_empty(),
        "unchanged file identity set should use quick reuse without rescanning"
    );
    assert_eq!(second.aggregate, first.aggregate);

    let handles = (0..4)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let root = root.clone();
            let inspect_dir = inspect_dir.clone();
            thread::spawn(move || run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir))
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let success = handle.join().expect("concurrent quick reuse joins");
        assert!(
            success.scanned_files.is_empty(),
            "concurrent freshness/fingerprint reads should reuse without rescanning"
        );
        assert_eq!(success.aggregate, first.aggregate);
    }
}

#[test]
fn inspect_command_computed_tier2_zero_count_stays_count_zero() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/unique.ts",
        "export function unique(input: number) { return input + 1; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-zero",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_summary_count(&response, "duplicates", 0);
    assert_eq!(
        response["summary"]["duplicates"]["total_groups"].as_u64(),
        Some(0),
        "computed zero duplicate summary should keep total_groups=0: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_warm_cache_hit_is_not_stale() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-warm-cache",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        response["summary"]["duplicates"]["total_groups"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "duplicates aggregate should be available from cache: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_changed_file_returns_fresh_without_scheduler_wait() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    let snapshot = InspectSnapshot::new(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
    );
    let initial = ctx.inspect_manager().tier2_run_with_reuse_result(
        snapshot,
        InspectCategory::Duplicates,
        None,
    );
    assert!(
        initial.outcome.is_ok(),
        "initial duplicates scan: {initial:?}"
    );

    write_file(
        &root,
        "src/foo.ts",
        "export function changed(input: number) { return input + 42; }\n",
    );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-direct-fresh",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_summary_count(&response, "duplicates", 0);
    let top = response["summary"]["unused_exports"]["top"]
        .as_array()
        .expect("unused export preview");
    assert!(
        top.iter().any(|item| item["symbol"] == "changed"),
        "direct inspect should reflect the edited file in Tier-2 output: {response:#}"
    );
}

#[test]
fn inspect_blocking_reuse_attaches_to_in_flight_background_category() {
    let _env_lock = env_serial_lock();
    let (_temp_dir, root) = fixture_project();
    let _wait_for_attach_root = EnvVarGuard::set(
        "AFT_TEST_TIER2_REUSE_WAIT_FOR_WAITER_ROOT",
        &root.to_string_lossy(),
    );
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);
    let manager = ctx.inspect_manager();
    let snapshot = InspectSnapshot::new(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
    );

    let starts_before_submit = manager.reuse_start_count_for_test();
    manager
        .submit_tier2_run_with_reuse_background(snapshot.clone(), InspectCategory::Duplicates)
        .expect("queue background duplicate scan");
    let start_deadline = Instant::now() + Duration::from_secs(20);
    while manager.reuse_start_count_for_test() == starts_before_submit {
        assert!(
            Instant::now() < start_deadline,
            "background duplicate scan never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let outcome = manager.tier2_run_with_reuse_blocking(
        snapshot,
        InspectCategory::Duplicates,
        aft::inspect::JobScope::for_project(root),
    );

    assert!(
        matches!(outcome, JobOutcome::Fresh { .. }),
        "blocking inspect should attach to and receive the background result: {outcome:?}"
    );
    assert_eq!(
        manager.reuse_completion_count(),
        1,
        "blocking inspect must not start a competing same-category reuse scan"
    );
}

#[test]
fn inspect_blocking_reuse_waits_for_slow_category_completion() {
    let _env_lock = env_serial_lock();
    let (_temp_dir, root) = fixture_project();
    let gate_ready = root.join("tier2-reuse-gate-ready");
    let gate_release = root.join("tier2-reuse-gate-release");
    let _gate_root = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_GATE_ROOT", &root.to_string_lossy());
    let _gate_ready = EnvVarGuard::set(
        "AFT_TEST_TIER2_REUSE_GATE_READY",
        &gate_ready.to_string_lossy(),
    );
    let _gate_release = EnvVarGuard::set(
        "AFT_TEST_TIER2_REUSE_GATE_RELEASE",
        &gate_release.to_string_lossy(),
    );
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);
    let manager = ctx.inspect_manager();
    let snapshot = InspectSnapshot::new(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
    );
    let scope = aft::inspect::JobScope::for_project(root);
    let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);

    thread::scope(|scope_thread| {
        scope_thread.spawn(move || {
            let outcome =
                manager.tier2_run_with_reuse_blocking(snapshot, InspectCategory::Duplicates, scope);
            outcome_tx.send(outcome).expect("publish Tier-2 outcome");
        });

        wait_for_path_event(&gate_ready, "Tier-2 worker gate admission");
        assert!(
            matches!(
                outcome_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "blocking reuse returned before the worker release event"
        );
        fs::write(&gate_release, b"release").expect("release Tier-2 worker");
        let outcome = outcome_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("Tier-2 outcome did not arrive before the outer hang catch");
        assert!(
            matches!(outcome, JobOutcome::Fresh { .. }),
            "gated Tier-2 work must complete instead of becoming pending: {outcome:?}"
        );
    });
}

#[test]
fn inspect_blocking_reuse_panic_cleans_in_flight_key() {
    let _env_lock = env_serial_lock();
    let (_temp_dir, root) = fixture_project();
    let _panic_root = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_PANIC_ROOT", &root.to_string_lossy());
    let _panic_category = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY", "duplicates");
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);
    let manager = ctx.inspect_manager();
    let snapshot = InspectSnapshot::new(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
    );

    let outcome = manager.tier2_run_with_reuse_blocking(
        snapshot,
        InspectCategory::Duplicates,
        aft::inspect::JobScope::for_project(root),
    );

    assert!(
        matches!(outcome, JobOutcome::Failed { .. }),
        "panic should be surfaced to waiters as a failed outcome: {outcome:?}"
    );
    assert!(
        !manager.tier2_any_in_flight(),
        "panic cleanup must remove the single-flight key"
    );
}

#[test]
fn inspect_command_ignores_retired_tier2_deadline_overrides() {
    let _env_lock = env_serial_lock();
    let (_temp_dir, root) = fixture_project();
    let _deadline_root = EnvVarGuard::set(
        "AFT_INSPECT_DIRECT_TIER2_DEADLINE_ROOT",
        &root.to_string_lossy(),
    );
    let _deadline = EnvVarGuard::set("AFT_INSPECT_DIRECT_TIER2_DEADLINE_MS", "10");
    let _delay_root = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_DELAY_ROOT", &root.to_string_lossy());
    let _delay = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_DELAY_MS", "200");
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-retired-direct-deadline",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "response: {response:#}");
    assert!(response.get("complete").is_none());
    assert!(response["summary"]["duplicates"].get("status").is_none());
}

#[test]
#[ignore = "watcher force-path bookkeeping is retired from the blocking inspect path"]
fn inspect_command_direct_forced_path_catches_mtime_preserved_same_size_edit() {
    let (_temp_dir, root) = fixture_project();
    let source = write_file(&root, "src/export.ts", "export function one() {}\n");
    let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&source, fixed_mtime).expect("set fixed mtime");
    let ctx = configured_context(&root);
    let snapshot = InspectSnapshot::new(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
    );
    let initial = ctx.inspect_manager().tier2_run_with_reuse_result(
        snapshot,
        InspectCategory::UnusedExports,
        None,
    );
    assert!(
        initial.outcome.is_ok(),
        "initial unused_exports scan: {initial:?}"
    );

    fs::write(&source, "export function two() {}\n").expect("same-size mutate");
    filetime::set_file_mtime(&source, fixed_mtime).expect("restore mtime");
    ctx.add_pending_tier2_paths([source.clone()]);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-mtime-preserved-direct-fresh",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let top = response["summary"]["unused_exports"]["top"]
        .as_array()
        .expect("unused exports top");
    assert!(
        top.iter().any(|item| item["symbol"] == "two"),
        "direct inspect must reflect the mtime-preserved content edit: {response:#}"
    );
    assert!(
        !top.iter().any(|item| item["symbol"] == "one"),
        "direct inspect must not reuse the stat-fresh stale contribution: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_hash_miss_after_restart_serves_stale_dead_code_results() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "import { used } from './lib';\nused();\n",
    );
    let lib = write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let before = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-before-hash-miss",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );
    assert_eq!(before["success"], true, "inspect failed: {before:#}");
    let before_count = before["summary"]["dead_code"]["count"]
        .as_u64()
        .expect("dead_code count");
    assert!(
        before_count > 0,
        "dead_code should have cached results: {before:#}"
    );
    assert!(
        dead_code_items(&before).contains(&("src/lib.ts".to_string(), "unused".to_string())),
        "dead_code fixture should report the intentionally unused export: {before:#}"
    );

    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 10; }\nexport function unused() { return 20; }\n",
    );

    // Simulate the restarted-process hash-miss case: a changed source file has
    // fresh per-file contribution metadata in SQLite, while the aggregate row is
    // still the previous contribution_set_hash. Old behavior returned Pending
    // here because get_aggregated() misses the exact hash and ignored the
    // persisted aggregate row.
    let cache = InspectCache::open(ctx.inspect_dir(), root.clone()).expect("open inspect cache");
    let changed_freshness = cache_freshness::collect(&lib).expect("collect changed freshness");
    cache
        .update_content_fresh_metadata(
            InspectCategory::DeadCode,
            Path::new("src").join("lib.ts").as_path(),
            &changed_freshness,
        )
        .expect("update contribution metadata to force aggregate hash miss");
    assert!(
        cache
            .get_aggregated(&JobKey::for_project_category(InspectCategory::DeadCode))
            .expect("hash-aware aggregate lookup")
            .is_none(),
        "test setup must force the exact-hash aggregate lookup to miss"
    );

    let restarted_ctx = configured_context_with_callgraph_store(&root, true);
    let after = inspect(
        &restarted_ctx,
        json!({
            "id": "inspect-dead-code-after-restart-hash-miss",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(after["success"], true, "inspect failed: {after:#}");
    assert_summary_count(&after, "dead_code", before_count);
    assert!(
        dead_code_items(&after).contains(&("src/lib.ts".to_string(), "unused".to_string())),
        "fresh hash-miss response should retain the expected unused export: {after:#}"
    );
}

#[test]
fn inspect_command_tier2_aggregate_hash_mismatch_is_cache_miss() {
    let (_temp_dir, root) = fixture_project();
    let file = write_file(&root, "src/foo.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
    let key = JobKey::for_project_category(InspectCategory::Duplicates);
    let freshness = cache_freshness::collect(&file).expect("collect freshness");
    let contribution = FileContribution::new(
        InspectCategory::Duplicates,
        file.clone(),
        freshness,
        json!({"file": "src/foo.ts", "fragments": []}),
    );

    cache
        .store_tier2_result(
            key.clone(),
            std::slice::from_ref(&file),
            &[contribution],
            json!({"count": 1, "items": [{"file": "src/foo.ts"}]}),
        )
        .expect("store tier2 aggregate");
    assert!(
        cache
            .get_aggregated(&key)
            .expect("warm aggregate")
            .is_some(),
        "freshly stored aggregate should be readable"
    );

    write_file(
        &root,
        "src/foo.ts",
        "export function changed(input: number) { return input + 42; }\n",
    );
    let changed_freshness = cache_freshness::collect(&file).expect("collect changed freshness");
    cache
        .update_content_fresh_metadata(
            InspectCategory::Duplicates,
            Path::new("src/foo.ts"),
            &changed_freshness,
        )
        .expect("update cached contribution metadata without aggregate recompute");

    assert!(
        cache
            .get_aggregated(&key)
            .expect("hash-aware memory aggregate read")
            .is_none(),
        "in-memory aggregate must miss after contribution_set_hash changes"
    );
    let reopened = InspectCache::open(inspect_dir, root).expect("reopen cache");
    assert!(
        reopened
            .get_aggregated(&key)
            .expect("hash-aware sqlite aggregate read")
            .is_none(),
        "persisted aggregate must miss when its stored contribution_set_hash is stale"
    );
}

fn dead_code_items(response: &Value) -> Vec<(String, String)> {
    response["details"]["dead_code"]
        .as_array()
        .expect("dead_code details array")
        .iter()
        .map(|item| {
            (
                item["file"].as_str().expect("dead file").to_string(),
                item["symbol"].as_str().expect("dead symbol").to_string(),
            )
        })
        .collect()
}

fn unused_export_items(response: &Value) -> Vec<(String, String)> {
    response["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details array")
        .iter()
        .map(|item| {
            (
                item["file"]
                    .as_str()
                    .expect("unused export file")
                    .to_string(),
                item["symbol"]
                    .as_str()
                    .expect("unused export symbol")
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn inspect_command_oxc_unused_exports_workspace_reports_dead_export_despite_dynamic_import() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    );
    write_file(
        &root,
        "packages/lib/package.json",
        r#"{"name":"@scope/lib","exports":"./src/index.ts"}"#,
    );
    write_file(
        &root,
        "packages/lib/src/index.ts",
        "export { consumed } from './api';\n",
    );
    write_file(
        &root,
        "packages/lib/src/api.ts",
        "export function consumed() { return 1; }\nexport function genuinelyDead() { return 2; }\n",
    );
    write_file(&root, "packages/app/package.json", r#"{"name":"app"}"#);
    write_file(
        &root,
        "packages/app/src/consumer.ts",
        "import { consumed } from '@scope/lib';\nconsole.log(consumed());\n",
    );
    write_file(
        &root,
        "packages/app/src/dynamic.ts",
        "const name = './optional-plugin';\nexport async function loadOptional() { return import(name); }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["unused_exports"]);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-oxc-workspace",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 20,
        }),
    );
    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let items = unused_export_items(&response);

    assert!(
        !items.contains(&(
            "packages/lib/src/api.ts".to_string(),
            "consumed".to_string()
        )),
        "barrel-export imported through a workspace package should be live: {response:#}",
    );
    assert!(
        items.contains(&(
            "packages/lib/src/api.ts".to_string(),
            "genuinelyDead".to_string()
        )),
        "genuinely dead export should still be reported: {response:#}",
    );
    let dead_item = response["details"]["unused_exports"]
        .as_array()
        .expect("unused export details")
        .iter()
        .find(|item| item["file"] == "packages/lib/src/api.ts" && item["symbol"] == "genuinelyDead")
        .expect("dead export detail");
    assert_eq!(dead_item["provenance"], "oxc");
}

#[test]
fn inspect_command_oxc_dead_code_keeps_same_file_schema_composition_live() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "import { UserSchema } from './schema';\nconsole.log(UserSchema);\n",
    );
    write_file(
        &root,
        "src/schema.ts",
        "const z = { object: () => ({ extend: () => ({}) }) };\nexport const BaseSchema = z.object({});\nexport const UserSchema = BaseSchema.extend({});\nexport const TrulyDeadSchema = z.object({});\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-oxc-same-file",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 20,
        }),
    );
    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let items = dead_code_items(&response);

    assert!(
        !items.contains(&("src/schema.ts".to_string(), "BaseSchema".to_string())),
        "schema composed via same-file value reference should not be dead: {response:#}",
    );
    assert!(
        items.contains(&("src/schema.ts".to_string(), "TrulyDeadSchema".to_string())),
        "genuinely dead schema export should still be reported: {response:#}",
    );
}

#[test]
fn inspect_command_dead_code_uses_cargo_manifest_targets_not_nested_main_files() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
autobins = false

[[bin]]
name = "fixture-cli"
path = "src/bin/app.rs"
"#,
    );
    write_file(
        &root,
        "src/bin/app.rs",
        "pub fn declared_bin_entry() -> u32 { 1 }\npub fn unused_bin_helper() -> u32 { 0 }\nfn main() { declared_bin_entry(); }\n",
    );
    write_file(
        &root,
        "tools/main.rs",
        "pub fn nested_only() -> u32 { 2 }\npub fn nested_main() -> u32 { nested_only() }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-cargo-manifest-entry-points",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let items = dead_code_items(&response);
    assert!(
        items.contains(&("tools/main.rs".to_string(), "nested_only".to_string())),
        "nested main.rs must not be treated as a Cargo entry point: {response:#}"
    );
    assert!(
        !items.contains(&(
            "src/bin/app.rs".to_string(),
            "declared_bin_entry".to_string()
        )),
        "declared Cargo bin export called from main should be live: {response:#}"
    );
    assert!(
        items.contains(&(
            "src/bin/app.rs".to_string(),
            "unused_bin_helper".to_string()
        )),
        "binary exports are not public API and should remain eligible: {response:#}"
    );
}

#[test]
fn inspect_command_unused_exports_uses_package_exports_as_public_api_but_not_bin() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        r#"{
  "name": "fixture",
  "exports": {
    ".": "./src/index.ts",
    "./feature": { "import": "./src/feature.ts" }
  },
  "bin": { "fixture": "./src/cli.ts" }
}
"#,
    );
    write_file(
        &root,
        "src/index.ts",
        "export function publicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/feature.ts",
        "export function publicFeature() { return 2; }\n",
    );
    write_file(
        &root,
        "src/cli.ts",
        "export function cliEntry() { return 3; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function nonPublicUncalled() { return 4; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-package-public-api",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        unused_export_items(&response),
        vec![
            ("src/cli.ts".to_string(), "cliEntry".to_string()),
            (
                "src/internal.ts".to_string(),
                "nonPublicUncalled".to_string()
            ),
        ],
        "package exports should be public API while bin/non-public exports are reported: {response:#}"
    );
}

#[test]
fn inspect_command_dead_code_and_unused_exports_share_workspace_public_api_resolution() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        r#"{"private":true,"workspaces":["apps/*"]}"#,
    );
    write_file(
        &root,
        "apps/service/package.json",
        r#"{"name":"service","exports":"./src/index.ts"}"#,
    );
    write_file(
        &root,
        "apps/service/src/index.ts",
        "export function serviceApi() { return 1; }\n",
    );
    write_file(
        &root,
        "apps/service/src/internal.ts",
        "export function serviceInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-shared-public-api-resolution",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "apps/service/src/internal.ts".to_string(),
            "serviceInternal".to_string()
        )],
        "dead_code should use the workspace package public API: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "apps/service/src/internal.ts".to_string(),
            "serviceInternal".to_string()
        )],
        "unused_exports should match dead_code without a packages/* assumption: {response:#}"
    );
}

#[test]
fn inspect_command_manifestless_projects_keep_conventional_entry_point_fallback() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "export function fallbackPublicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function fallbackInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-manifestless-entry-point-fallback",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest-less conventional index.ts should remain an entry/public API file: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest-less fallback should be shared by unused_exports: {response:#}"
    );
}

#[test]
fn inspect_command_manifest_without_declared_entries_uses_conventional_fallback() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"private":true}"#);
    write_file(
        &root,
        "src/index.ts",
        "export function fallbackPublicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function fallbackInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-manifest-no-entry-fallback",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest presence without declared roots should still use conventional index.ts fallback for dead_code: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest presence without declared roots should still use conventional index.ts fallback for unused_exports: {response:#}"
    );
}

#[test]
fn inspect_command_manifest_edit_invalidates_unused_exports_aggregate() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"main":"./src/index.ts"}"#);
    write_file(
        &root,
        "src/index.ts",
        "export function indexApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/public.ts",
        "export function publicApi() { return 2; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function internalOnly() { return 3; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let before = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-before-manifest-edit",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );
    assert_eq!(before["success"], true, "inspect failed: {before:#}");
    assert_eq!(
        unused_export_items(&before),
        vec![
            ("src/internal.ts".to_string(), "internalOnly".to_string()),
            ("src/public.ts".to_string(), "publicApi".to_string()),
        ],
        "initial package main should suppress only index.ts: {before:#}"
    );

    write_file(&root, "package.json", r#"{"main":"./src/public.ts"}"#);
    tier2_run(&ctx, &["unused_exports"]);
    let after = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-after-manifest-edit",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );

    assert_eq!(after["success"], true, "inspect failed: {after:#}");
    assert_eq!(
        unused_export_items(&after),
        vec![
            ("src/index.ts".to_string(), "indexApi".to_string()),
            ("src/internal.ts".to_string(), "internalOnly".to_string()),
        ],
        "manifest edit should change the contribution_set_hash and force aggregate recompute: {after:#}"
    );
}

#[test]
fn inspect_command_dead_code_keeps_same_name_exports_distinct_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/main.ts",
        "import { foo } from './alive';\nexport function main() { return foo(); }\n",
    );
    write_file(
        &root,
        "src/alive.ts",
        "export function foo() { return 1; }\n",
    );
    write_file(
        &root,
        "src/dead.ts",
        "export function foo() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-same-name",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["count"], 1);
    assert_eq!(
        dead_code_items(&response),
        vec![("src/dead.ts".to_string(), "foo".to_string())]
    );
}

#[test]
fn inspect_command_dead_code_does_not_headline_product_referenced_cycle_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/a.ts",
        "import { b } from './b';\nexport function a() { return b(); }\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { a } from './a';\nexport function b() { return a(); }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-cycle",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["count"], 0);
    assert_eq!(dead_code_items(&response), Vec::<(String, String)>::new());
    assert_eq!(response["summary"]["dead_code"]["test_only_count"], 0);
}

#[test]
fn inspect_command_cycles_render_chain_and_import_edges_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/a.ts",
        "import { b } from './b';\nexport const a = b;\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { a } from './a';\nexport const b = a;\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["cycles"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-cycles",
            "command": "inspect",
            "sections": "cycles",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_summary_count(&response, "cycles", 1);
    assert_eq!(response["summary"]["cycles"]["largest"], 2);
    let details = response["details"]["cycles"]
        .as_array()
        .expect("cycles details");
    assert_eq!(
        details.len(),
        1,
        "cycle should be reported once: {response:#}"
    );
    assert_eq!(
        details[0]["cycle"].as_str(),
        Some("src/a.ts -> src/b.ts -> src/a.ts")
    );
    let text = response["text"].as_str().expect("inspect text");
    assert!(
        text.contains("Import cycles: 1 import cycle (largest: 2 files)"),
        "cycles summary line missing: {text}"
    );
    assert!(
        text.contains("src/a.ts -> src/b.ts via import::Named './b' line 1"),
        "cycle import edge missing: {text}"
    );
}

#[test]
fn inspect_command_dead_code_keeps_multi_hop_entry_reachability_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/main.ts",
        "import { b } from './b';\nexport function main() { return b(); }\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { c } from './c';\nexport function b() { return c(); }\n",
    );
    write_file(&root, "src/c.ts", "export function c() { return 3; }\n");
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-multihop",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["count"], 0);
    assert!(
        dead_code_items(&response).is_empty(),
        "response: {response:#}"
    );
}

#[test]
fn inspect_command_dead_code_resolves_extensionless_package_module_entry_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{\"module\":\"src/index\"}\n");
    write_file(
        &root,
        "src/index.mts",
        "export function publicApi() { return 1; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-package-entry",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        response["summary"]["dead_code"]["count"], 0,
        "extensionless package module entry should be public API: {response:#}"
    );
}

#[test]
fn inspect_command_duplicates_summary_count_uses_production_payload() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-count",
            "command": "inspect",
            "sections": "duplicates",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    let total_groups = response["summary"]["duplicates"]["total_groups"]
        .as_u64()
        .expect("duplicates total_groups");
    assert!(
        count > 0,
        "duplicates count should be non-zero: {response:#}"
    );
    assert_eq!(
        count, total_groups,
        "summary should mirror scanner contract: {response:#}"
    );
    assert!(
        !response["details"]["duplicates"]
            .as_array()
            .expect("duplicates details")
            .is_empty(),
        "duplicates details should include groups: {response:#}"
    );
}

#[test]
#[ignore = "requires the deferred inspect slice's authoritative scoped LSP completion"]
fn inspect_command_duplicates_file_scope_matches_occurrence_labels() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/scoped/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/scoped/baz.ts", duplicate_fixture_source());
    write_file(&root, "src/outside/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-scoped",
            "command": "inspect",
            "sections": "duplicates",
            "scope": "src/scoped",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    assert!(
        count > 0,
        "scoped duplicates should retain groups duplicated within scope: {response:#}"
    );
    let details = response["details"]["duplicates"]
        .as_array()
        .expect("duplicates details");
    assert!(
        !details.is_empty(),
        "expected scoped duplicate details: {response:#}"
    );
    for group in details {
        let files = group["files"].as_array().expect("group files");
        assert!(
            files.len() >= 2,
            "duplicate groups with fewer than two in-scope files should be dropped: {response:#}"
        );
        assert!(
            files
                .iter()
                .filter_map(Value::as_str)
                .all(|file| file.starts_with("src/scoped/")),
            "scoped duplicate groups must prune out-of-scope files: {response:#}"
        );
    }
}

#[test]
#[ignore = "requires the deferred inspect slice's authoritative scoped LSP completion"]
fn inspect_command_unused_exports_scope_filters_full_contributions_before_cap() {
    let (_temp_dir, root) = fixture_project();
    for index in 0..120 {
        write_file(
            &root,
            &format!("aaa_global/file_{index:03}.ts"),
            &format!("export function global_{index:03}() {{ return {index}; }}\n"),
        );
    }
    for index in 0..3 {
        write_file(
            &root,
            &format!("zzz_scoped/file_{index:03}.ts"),
            &format!("export function scoped_{index:03}() {{ return {index}; }}\n"),
        );
    }
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let scoped = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-scoped-after-cap",
            "command": "inspect",
            "sections": "unused_exports",
            "scope": "zzz_scoped",
            "topK": 100,
        }),
    );

    assert_eq!(scoped["success"], true, "inspect failed: {scoped:#}");
    assert_eq!(
        scoped["summary"]["unused_exports"]["count"], 3,
        "scoped count should come from full contributions, not the capped project aggregate: {scoped:#}"
    );
    let scoped_details = scoped["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details");
    assert_eq!(
        scoped_details.len(),
        3,
        "scoped details should include all scoped items beyond the project cap: {scoped:#}"
    );
    assert!(
        scoped_details.iter().all(|item| item["file"]
            .as_str()
            .is_some_and(|file| file.starts_with("zzz_scoped/"))),
        "scoped details should only include scoped files: {scoped:#}"
    );

    let project = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-project-cap",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 100,
        }),
    );

    assert_eq!(project["success"], true, "inspect failed: {project:#}");
    assert_eq!(
        project["summary"]["unused_exports"]["count"], 123,
        "project-wide count should keep the full aggregate count: {project:#}"
    );
    let project_details = project["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details");
    assert_eq!(
        project_details.len(),
        100,
        "project-wide details should still be capped at 100: {project:#}"
    );
    assert!(
        project_details
            .iter()
            .all(|item| item["file"].as_str().is_some_and(|file| file.starts_with("aaa_global/"))),
        "project-wide cap should be applied before later zzz_scoped files appear in details: {project:#}"
    );
}

#[test]
#[ignore = "requires the deferred inspect slice's authoritative scoped LSP completion"]
fn inspect_command_duplicates_scope_filters_full_contributions_before_cap() {
    let (_temp_dir, root) = fixture_project();
    // Distinct per-file markers so the whole-file (program) node is not itself a
    // cross-file duplicate (which would correctly subsume the 130 inner groups);
    // the 130 functions remain byte-identical across files and so stay duplicated.
    write_file(
        &root,
        "aaa_global/left.ts",
        &many_duplicate_groups_source(2),
    );
    write_file(
        &root,
        "aaa_global/right.ts",
        &many_duplicate_groups_source(3),
    );
    write_file(
        &root,
        "zzz_scoped/left.ts",
        &many_duplicate_groups_source(4),
    );
    write_file(
        &root,
        "zzz_scoped/right.ts",
        &many_duplicate_groups_source(5),
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-scoped-after-cap",
            "command": "inspect",
            "sections": "duplicates",
            "scope": "zzz_scoped",
            "topK": 100,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    assert!(
        count > 100,
        "scoped duplicate count should come from full contributions, not the capped project aggregate: {response:#}"
    );
    let details = response["details"]["duplicates"]
        .as_array()
        .expect("duplicates details");
    assert_eq!(
        details.len(),
        100,
        "scoped duplicate details should be capped after filtering the full rollup: {response:#}"
    );
    assert!(
        details.iter().all(|group| group["files"]
            .as_array()
            .expect("group files")
            .iter()
            .filter_map(Value::as_str)
            .all(|file| file.starts_with("zzz_scoped/"))),
        "scoped duplicate details should only include scoped files: {response:#}"
    );
}

/// 130 literal-distinct functions shared across files (the real cross-file
/// duplicate groups) plus a trailing marker function whose statement count is
/// unique per `unique_stmts`. The unique marker makes each file's top-level
/// (program) node structurally distinct, so the WHOLE FILE is not itself a
/// cross-file duplicate that would (correctly) subsume the 130 inner groups
/// under the nested-overlap collapse. Each caller passes a distinct
/// `unique_stmts` so every file's program node appears exactly once.
fn many_duplicate_groups_source(unique_stmts: usize) -> String {
    let mut source = String::new();
    for index in 0..130 {
        source.push_str(&format!(
            r#"export function duplicate_group_{index:03}(input: number) {{
  const first = input + {index};
  const second = first * {};
  const third = second - {};
  const label = "group_{index:03}";
  if (third > {}) {{
    return label + third.toString();
  }}
  return label + first.toString();
}}
"#,
            index + 3,
            index + 7,
            index + 11
        ));
    }
    source.push_str("function file_unique_marker() {\n");
    for n in 0..unique_stmts {
        source.push_str(&format!("  const marker_{n} = {n} * 2 + 1;\n"));
    }
    source.push_str("  return 0;\n}\n");
    source
}

#[test]
fn inspect_command_duplicates_project_wide_cap_preserves_total_groups() {
    let (_temp_dir, root) = fixture_project();
    // Distinct per-file markers (see scope test): keep the 130 functions
    // duplicated across files without the whole file becoming one big duplicate.
    write_file(&root, "src/left.ts", &many_duplicate_groups_source(2));
    write_file(&root, "src/right.ts", &many_duplicate_groups_source(3));
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-project-cap",
            "command": "inspect",
            "sections": "duplicates",
            "topK": 100,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    let total_groups = response["summary"]["duplicates"]["total_groups"]
        .as_u64()
        .expect("duplicates total_groups");
    assert!(
        count > 100,
        "fixture should produce more groups than the drill-down cap: {response:#}"
    );
    assert_eq!(
        total_groups, count,
        "project-wide total_groups should retain the full group count: {response:#}"
    );
    assert_eq!(
        response["details"]["duplicates"]
            .as_array()
            .expect("duplicates details")
            .len(),
        100,
        "project-wide duplicate details should still be capped at 100: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_last_run_updates_on_hash_match_reuse() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);
    let manager = ctx.inspect_manager();
    let snapshot = InspectSnapshot::new_with_capabilities(
        root.clone(),
        ctx.inspect_dir(),
        ctx.config(),
        ctx.symbol_cache(),
        ctx.inspect_writer(),
        ctx.callgraph_writer(),
    );

    let first = manager
        .tier2_run_with_reuse_result(snapshot.clone(), InspectCategory::Duplicates, None)
        .outcome
        .expect("initial duplicates publish succeeds");
    assert_eq!(
        first.scanned_files.len(),
        2,
        "initial run must seed the cache"
    );
    let first_last_run = InspectCache::open_readonly(ctx.inspect_dir(), root.clone())
        .expect("open inspect cache after initial publish")
        .expect("inspect cache exists after initial publish")
        .last_full_run(InspectCategory::Duplicates)
        .expect("read initial duplicates last_full_run")
        .expect("initial duplicates last_full_run exists");

    // A zero-capacity isolated limiter gates the full-rescan arm. Hash-match
    // reuse publishes before cold-build admission, so only a missing reuse
    // decision can reach the 30-second outer hang catch.
    ctx.isolate_cold_build_limiter_for_test(0);
    let completions_before = manager.reuse_completion_count();
    let reuse_manager = Arc::clone(&manager);
    let reuse_root = root.clone();
    let (reuse_result_tx, reuse_result_rx) = std::sync::mpsc::sync_channel(1);
    let reuse_driver = thread::spawn(move || {
        let outcome = reuse_manager.tier2_run_with_reuse_blocking(
            snapshot,
            InspectCategory::Duplicates,
            JobScope::for_project(reuse_root),
        );
        reuse_result_tx
            .send(outcome)
            .expect("publish hash-match reuse outcome");
    });
    let outcome = reuse_result_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("hash-match reuse did not publish before the outer hang catch");
    reuse_driver
        .join()
        .expect("hash-match reuse driver completes");

    assert!(
        matches!(outcome, JobOutcome::Fresh { .. }),
        "hash-match reuse must publish a fresh outcome: {outcome:?}"
    );
    assert_eq!(
        manager.reuse_completion_count(),
        completions_before + 1,
        "the reuse completion event must be recorded before the waiter wakes"
    );
    let second_last_run = InspectCache::open_readonly(ctx.inspect_dir(), root.clone())
        .expect("open inspect cache after reuse publish")
        .expect("inspect cache exists after reuse publish")
        .last_full_run(InspectCategory::Duplicates)
        .expect("read reused duplicates last_full_run")
        .expect("reused duplicates last_full_run exists");
    assert!(
        second_last_run > first_last_run,
        "hash-match reuse should advance tier2_last_run: first={first_last_run} second={second_last_run}"
    );
}

fn configure_fake_rust_lsp(ctx: &AppContext) {
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
}

fn open_with_lsp(ctx: &AppContext, file: &Path, content: &str) {
    let config = ctx.config().clone();
    ctx.lsp()
        .notify_file_changed(file, content, &config)
        .expect("notify file changed");
    let diagnostics = ctx
        .lsp()
        .wait_for_diagnostics(file, &config, Duration::from_secs(30));
    assert!(
        !diagnostics.is_empty(),
        "fake LSP should publish diagnostics for {file:?}"
    );
}

fn close_with_lsp(ctx: &AppContext, file: &Path) {
    let config = ctx.config().clone();
    ctx.lsp().notify_file_closed(file).expect("close file");
    let diagnostics = ctx
        .lsp()
        .wait_for_diagnostics(file, &config, Duration::from_secs(30));
    assert!(
        diagnostics.is_empty(),
        "fake LSP close should publish checked-clean diagnostics"
    );
    assert!(
        ctx.lsp().has_diagnostic_report_for_file(file),
        "empty publish should remain as checked-clean proof"
    );
}

fn open_with_server_status_mode(ctx: &AppContext, file: &Path, mode: &str) {
    configure_fake_rust_lsp(ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", mode);
    let config = ctx.config().clone();
    ctx.lsp()
        .notify_file_changed(file, "fn main() {}\n", &config)
        .expect("start fake rust-analyzer");
}

fn wait_for_inspect_phase_start(request_id: &str, phase: InspectPhaseId) {
    let hang_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(snapshot) = inspect_phase_log_for_request(request_id) {
            if let Some(record) = snapshot
                .records
                .iter()
                .find(|record| record.entry.id == phase)
            {
                assert!(
                    !record.is_completed() && record.terminal_error().is_none(),
                    "inspect phase {phase:?} terminated before its release event: {snapshot:?}"
                );
                return;
            }
        }
        assert!(
            Instant::now() < hang_deadline,
            "timed out waiting for inspect phase {phase:?} to start for {request_id}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_lsp_report_state(ctx: &AppContext, file: &Path, provisional: bool) {
    // Report state is authoritative; this deadline only catches a wedged server.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ready = {
            let mut lsp = ctx.lsp();
            lsp.drain_events();
            lsp.has_diagnostic_report_for_file(file)
                && (!lsp.provisional_server_keys().is_empty()) == provisional
        };
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for provisional={provisional} LSP report for {}",
            file.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn rust_quiescence_promotes_latest_publish_for_warm_inspect_and_status_bar() {
    const REQUEST_ID: &str = "inspect-diagnostics-after-settle";

    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-settle\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = Arc::new(configured_context_with_callgraph_store(&root, true));
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "publish_then_quiescent");

    let inspect_ctx = Arc::clone(&ctx);
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
    let inspector = thread::spawn(move || {
        let response = serde_json::to_value(handle_inspect_tool_call(
            &request(json!({
                "id": REQUEST_ID,
                "command": "inspect",
                "sections": ["diagnostics"],
                "topK": 10,
            })),
            &inspect_ctx,
        ))
        .expect("inspect response serializes");
        response_tx
            .send(response)
            .expect("publish blocking inspect response");
    });

    wait_for_inspect_phase_start(REQUEST_ID, InspectPhaseId::LspQuiescence);
    // didOpen is the deterministic release event: the fake producer publishes
    // diagnostics and then declares quiescence in that message's handler.
    let config = ctx.config();
    ctx.lsp()
        .notify_file_changed(&file, "fn main() {}\n", &config)
        .expect("release the warming producer with didOpen");

    let response = response_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("inspect did not observe the quiescence publish before the outer hang catch");
    inspector.join().expect("blocking inspect thread completes");
    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["inspect_terminal"], "fresh");

    let phase_log = inspect_phase_log_for_request(REQUEST_ID).expect("retained inspect phase log");
    let quiescence = phase_log
        .records
        .iter()
        .find(|record| record.entry.id == InspectPhaseId::LspQuiescence)
        .expect("LSP quiescence phase record");
    assert!(
        quiescence.is_completed() && quiescence.terminal_error().is_none(),
        "the producer quiescence event must complete successfully: {phase_log:?}"
    );

    let summary = response["summary"]["diagnostics"]
        .as_object()
        .expect("diagnostics summary");
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(1));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(1));
    assert!(
        !summary.contains_key("status"),
        "quiescent diagnostics must be complete: {response:#}"
    );
    assert_eq!(
        response["details"]["diagnostics"]
            .as_array()
            .expect("diagnostics details")
            .len(),
        2
    );

    ctx.update_status_bar_tier2(Some(0), Some(0), Some(0), Some(0), false);
    let counts = ctx.status_bar_counts().expect("status bar populated");
    assert_eq!((counts.errors, counts.warnings), (1, 1));
}

#[test]
fn rust_quiescence_promotes_empty_latest_publish_as_checked_clean() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-clean-settle\"\n",
    );
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    open_with_server_status_mode(&ctx, &file, "empty_then_quiescent");
    wait_for_lsp_report_state(&ctx, &file, false);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-empty-diagnostics-after-settle",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );
    let summary = response["summary"]["diagnostics"]
        .as_object()
        .expect("diagnostics summary");
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "latest empty publish must prove checked-clean: {response:#}"
    );
    assert!(response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details")
        .is_empty());
}

#[test]
fn rust_pre_quiescence_publish_is_not_returned_as_a_partial_summary() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-warming\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    open_with_server_status_mode(&ctx, &file, "1");
    wait_for_lsp_report_state(&ctx, &file, true);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-before-settle",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["code"], "inspect_not_fresh");
    assert!(response.get("summary").is_none());
}

#[test]
fn inspect_command_diagnostics_default_reports_warm_counts_and_details() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-warm\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &file, "fn main() {}\n");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-warm",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(1));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(1));
    assert_eq!(summary.get("info").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("hints").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "warm diagnostics should be computed, not pending: {response:#}"
    );

    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert_eq!(details.len(), 2, "response: {response:#}");
    assert!(details.iter().all(|item| item["file"] == "src/main.rs"));
    assert!(details.iter().any(|item| item["severity"] == "error"));
    assert!(details.iter().any(|item| item["severity"] == "warning"));
}

#[test]
fn inspect_command_diagnostics_without_a_report_is_not_returned_as_a_zero_result() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-pending\"\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-no-server-ran",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["code"], "inspect_not_fresh");
    assert!(response.get("summary").is_none());
}

#[test]
fn inspect_command_diagnostics_clean_zero_after_empty_publish() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-clean\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &file, "fn main() {}\n");
    close_with_lsp(&ctx, &file);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-clean",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "checked-clean diagnostics should be distinct from pending: {response:#}"
    );
    assert!(response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details")
        .is_empty());
}

/// Scope must not change collection work: a scoped request may not spawn
/// servers, open documents, or pull diagnostics beyond what the warm path
/// already did. Assertions target the producer/LSP call surface (server
/// roster, open-document store, diagnostic reports), not timing.
#[test]
fn scoped_diagnostics_perform_no_lsp_work_beyond_the_warm_path() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-scope-cost\"\n",
    );
    let main_rs = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    // Pull-capable server: the old scoped path would have opened and pulled
    // the scoped file, leaving a report and a running server behind.
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    assert!(ctx.lsp().active_server_keys().is_empty());

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-no-work",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/main.rs",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        ctx.lsp().active_server_keys().is_empty(),
        "scoped inspect must not spawn servers"
    );
    assert!(
        !ctx.lsp().document_is_open_for_test(&main_rs),
        "scoped inspect must not open documents"
    );
    assert!(
        !ctx.lsp().has_diagnostic_report_for_file(&main_rs),
        "scoped inspect must not pull diagnostics into the store"
    );
    // The honest consequence of doing no collection work: the never-analyzed
    // file is named as a gap instead of rendering as clean.
    assert_eq!(response["complete"], false, "response: {response:#}");
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
        .unwrap_or_else(|| panic!("uncovered_file gap missing: {response:#}"));
    assert_eq!(gap["file"], "src/main.rs");
}

/// Producer discovery may see nested Cargo workspaces, but a scoped blocking
/// inspect starts rust-analyzer only for the workspace that owns the scope.
#[test]
fn scoped_blocking_inspect_starts_only_the_owning_rust_workspace() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/aft\"]\nresolver = \"2\"\n",
    );
    write_file(
        &root,
        "crates/aft/Cargo.toml",
        "[package]\nname = \"scoped-owner\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "crates/aft/src/lib.rs", "pub fn owned() {}\n");
    write_file(
        &root,
        "spikes/foreign/Cargo.toml",
        "[workspace]\n[package]\nname = \"foreign\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "spikes/foreign/src/lib.rs", "pub fn foreign() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-scoped-owning-workspace",
            "command": "inspect",
            "scope": "crates/aft",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    let active = ctx.lsp().active_server_keys();
    assert_eq!(active.len(), 1, "response: {response:#}");
    assert_eq!(active[0].kind, ServerKind::Rust);
    assert_eq!(
        active[0].root,
        crate::helpers::canonicalize_like_product(&root)
    );

    let unscoped = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-unscoped-all-workspaces",
            "command": "inspect",
        })),
        &ctx,
    ))
    .expect("unscoped inspect response serializes");
    let active = ctx.lsp().active_server_keys();
    assert_eq!(active.len(), 2, "response: {unscoped:#}");
    assert!(active.iter().any(|key| {
        key.root == crate::helpers::canonicalize_like_product(&root.join("spikes/foreign"))
    }));
}

/// A scoped call over a warm root returns only scope-matching findings, and
/// the same findings appear in the unscoped call.
#[test]
fn scoped_diagnostics_filter_warm_findings_to_the_scope() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-scope-filter\"\n",
    );
    let main_rs = write_file(&root, "src/main.rs", "fn main() {}\n");
    let lib_rs = write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &main_rs, "fn main() {}\n");
    open_with_lsp(&ctx, &lib_rs, "pub fn lib() {}\n");

    let scoped = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-filter",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/main.rs",
            "topK": 10,
        }),
    );
    assert_eq!(scoped["success"], true, "inspect failed: {scoped:#}");
    assert!(
        scoped.get("complete").is_none(),
        "covered scope must be complete: {scoped:#}"
    );
    assert_eq!(scoped["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(scoped["summary"]["diagnostics"]["warnings"], 1);
    let scoped_details = scoped["details"]["diagnostics"]
        .as_array()
        .expect("scoped diagnostics details");
    assert_eq!(scoped_details.len(), 2, "response: {scoped:#}");
    assert!(
        scoped_details
            .iter()
            .all(|item| item["file"] == "src/main.rs"),
        "scoped details must only contain scope findings: {scoped:#}"
    );

    let unscoped = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-unscoped-filter",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 10,
        }),
    );
    assert_eq!(unscoped["success"], true, "inspect failed: {unscoped:#}");
    assert_eq!(unscoped["summary"]["diagnostics"]["errors"], 2);
    assert_eq!(unscoped["summary"]["diagnostics"]["warnings"], 2);
    let unscoped_details = unscoped["details"]["diagnostics"]
        .as_array()
        .expect("unscoped diagnostics details");
    assert_eq!(unscoped_details.len(), 4, "response: {unscoped:#}");
    // The scoped findings are a subset of the unscoped findings.
    assert!(
        scoped_details
            .iter()
            .all(|item| unscoped_details.contains(item)),
        "scoped findings must survive in the unscoped payload: scoped={scoped_details:#?} unscoped={unscoped_details:#?}"
    );
}

/// The reporter's scenario: the server reported on file Y only, so a scoped
/// call for file X must return `complete: false` naming X — never an
/// empty-clean result. The second half is the mutation control: forcing the
/// coverage check to "covered" reproduces exactly the confident-empty answer
/// the assertions above reject, proving they are sensitive to the check.
#[test]
fn scoped_diagnostics_name_uncovered_files_instead_of_rendering_clean_empty() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-scope-gap\"\n",
    );
    let reported = write_file(&root, "src/main.rs", "fn main() {}\n");
    write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    // The server only ever analyzes src/main.rs; src/lib.rs is never touched.
    open_with_lsp(&ctx, &reported, "fn main() {}\n");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-uncovered",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/lib.rs",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["complete"], false, "response: {response:#}");
    let summary = response["summary"]["diagnostics"]
        .as_object()
        .expect("diagnostics summary");
    assert_eq!(
        summary.get("complete").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(0));
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
        .unwrap_or_else(|| panic!("uncovered_file gap missing: {response:#}"));
    assert_eq!(gap["file"], "src/lib.rs");
    assert!(
        gap["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "coverage gap must carry a reason: {response:#}"
    );
    assert!(
        response["details"]
            .get("diagnostics")
            .and_then(Value::as_array)
            .is_none_or(|items| items.is_empty()),
        "uncovered scope must not render findings: {response:#}"
    );

    // Mutation control: force every scoped file to read as covered. The gap
    // disappears and the payload becomes the confident empty answer that the
    // coverage check exists to prevent — i.e. every assertion above would
    // fail under this mutation.
    aft::inspect::force_scoped_diagnostic_coverage_for_test(true);
    let mutated = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-uncovered-mutated",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/lib.rs",
            "topK": 10,
        }),
    );
    aft::inspect::force_scoped_diagnostic_coverage_for_test(false);

    assert_eq!(mutated["success"], true, "inspect failed: {mutated:#}");
    assert!(
        mutated.get("complete").is_none(),
        "forced coverage must remove the gap: {mutated:#}"
    );
    assert!(
        mutated["summary"]["diagnostics"].get("complete").is_none(),
        "forced coverage must remove the category gap: {mutated:#}"
    );
    assert_eq!(mutated["summary"]["diagnostics"]["errors"], 0);
}

/// Negative control: a scoped file WITH an authoritative report and zero
/// findings is legitimately clean — coverage gaps must not fire for it.
#[test]
fn scoped_diagnostics_render_clean_empty_for_covered_file_without_findings() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-scope-clean\"\n",
    );
    let lib_rs = write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    // Authoritative checked-clean report: a producer analyzed the file and
    // found nothing. Published directly so the test does not depend on any
    // fake-server behavior. Store keys use the canonical product spelling, so
    // publish with the same form the lookup normalizes to.
    ctx.lsp()
        .diagnostics_store_mut_for_test()
        .publish_with_kind(
            ServerKind::Rust,
            crate::helpers::canonicalize_like_product(&lib_rs),
            Vec::new(),
        );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-clean",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/lib.rs",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        response.get("complete").is_none(),
        "covered clean scope must not be marked incomplete: {response:#}"
    );
    assert!(
        response["summary"]["diagnostics"].get("complete").is_none(),
        "covered clean scope must not carry a category gap: {response:#}"
    );
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    assert!(
        response["details"]
            .get("diagnostics")
            .and_then(Value::as_array)
            .is_none_or(|items| items.is_empty()),
        "clean scope must not render findings: {response:#}"
    );
}

/// A blocking inspect facing a warming producer must wait for the producer to
/// settle instead of rendering the warming store: the payload asserted below
/// can only be built from a publish that arrives after the call started.
#[test]
fn blocking_inspect_waits_for_a_warming_producer_to_settle() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-blocking-wait\"\n",
    );
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    // Blocking tool calls run every active category, so the Tier-2
    // prerequisites (callgraph store, aggregates) must be ready first.
    let ctx = Arc::new(configured_context_with_callgraph_store(&root, true));
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    configure_fake_rust_lsp(&ctx);
    // Warming mode: didOpen publishes provisional diagnostics, and only a
    // later didChange declares quiescence and publishes the settled set.
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "1");
    let config = ctx.config().clone();
    ctx.lsp()
        .notify_file_changed(&file, "fn main() {}\n", &config)
        .expect("open document");
    wait_for_lsp_report_state(&ctx, &file, true);

    // Start inspect first, then release the producer only after its quiescence
    // phase is observably waiting. This makes the post-start publish ordering
    // deterministic without charging a fixed sleep to correctness.
    const REQUEST_ID: &str = "inspect-blocking-wait";
    let inspect_ctx = Arc::clone(&ctx);
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
    let inspector = thread::spawn(move || {
        let response = serde_json::to_value(handle_inspect_tool_call(
            &request(json!({
                "id": REQUEST_ID,
                "command": "inspect",
            })),
            &inspect_ctx,
        ))
        .expect("inspect response serializes");
        response_tx
            .send(response)
            .expect("publish blocking inspect response");
    });

    wait_for_inspect_phase_start(REQUEST_ID, InspectPhaseId::LspQuiescence);
    assert!(
        matches!(
            response_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "blocking inspect returned before producer settlement"
    );
    ctx.lsp()
        .notify_file_changed(&file, "fn main() { let _settled = 1; }\n", &config)
        .expect("settle the producer via an edit");
    let response = response_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("inspect did not settle before the outer hang catch");
    inspector.join().expect("blocking inspector completes");

    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["inspect_terminal"], "fresh");
    // Event-ordering proof that the wait blocked: this message exists only
    // after the post-start publish was drained.
    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert!(
        details
            .iter()
            .any(|item| item["message"] == "test diagnostic after change"),
        "the settled publish must be part of the payload: {response:#}"
    );
}

/// A producer that never settles is cut off by the shared request deadline.
/// The terminal reserve keeps the PHASE-FAILED response inside the configured
/// budget and attributes it to the unfinished quiescence phase.
#[test]
fn blocking_inspect_returns_before_the_configured_request_deadline() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-blocking-deadline\"\n",
    );
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    configure_fake_rust_lsp(&ctx);
    // Warming mode without any later edit: the server declares warming at
    // startup and nothing ever declares quiescence.
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "1");

    let started = Instant::now();
    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-blocking-deadline",
            "command": "inspect",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["inspect_terminal"], "phase_failed");
    assert_eq!(response["failure_reason"], "inspect_request_timeout");
    assert_eq!(response["failed_phase"], "lsp_quiescence");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the terminal reserve must return well before the 10s request budget: {:?}",
        started.elapsed()
    );
}

/// The deadline error text carries the configured millisecond budget end to
/// end: user config, clamped deadline, blocking wait, terminal failure detail.
#[test]
fn blocking_inspect_deadline_error_names_the_configured_budget() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-blocking-budget\"\n",
    );
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context_with_diagnostics_timeout(&root, 12_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "1");

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-blocking-budget",
            "command": "inspect",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["failure_reason"], "inspect_request_timeout");
    let detail = response["failure_detail"]
        .as_str()
        .unwrap_or_else(|| panic!("failure_detail missing: {response:#}"));
    assert!(
        detail.contains("12000ms"),
        "the configured budget must reach the error text: {detail}"
    );
}

/// A producer that declares quiescence without publishing any report is
/// complete. The wait already treated that producer as settled; the freshness
/// gate must use the same predicate rather than requiring a published report.
#[test]
fn blocking_inspect_is_fresh_when_producers_quiesce_without_reports() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-blocking-quiesce-empty\"\n",
    );
    write_file(&root, "src/main.rs", "fn main() {}\n");
    // Blocking tool calls run every active category, so the Tier-2
    // prerequisites (callgraph store, aggregates) must be ready first.
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    configure_fake_rust_lsp(&ctx);
    // Default fake rust-analyzer declares quiescence on `initialized` and
    // only publishes on didOpen. Blocking inspect starts producers without
    // opening documents, so the store stays empty after settlement.

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-blocking-quiesce-empty",
            "command": "inspect",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["inspect_terminal"], "fresh");
    let phases = response["wait_stamp"]["phases"]
        .as_array()
        .unwrap_or_else(|| panic!("wait_stamp.phases missing: {response:#}"));
    assert!(
        phases.iter().any(|phase| phase["id"] == "lsp_quiescence"),
        "quiescence must complete before the fresh terminal: {response:#}"
    );
    assert!(
        phases
            .iter()
            .any(|phase| { phase["id"] == "callgraph_ready" && phase["category"] == "dead_code" }),
        "fresh dead_code must record the ready callgraph phase: {response:#}"
    );
    assert!(
        phases.iter().any(|phase| phase["id"] == "tier2_rescan"),
        "fresh Tier-2 work must appear in the completed phases: {response:#}"
    );
    let summary = response["summary"]["diagnostics"]
        .as_object()
        .expect("diagnostics summary");
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "settled empty diagnostics must be complete, not pending: {response:#}"
    );
}

#[test]
fn scoped_diagnostics_drain_events_before_the_warm_collection() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-drain\"\n");
    for name in ["a.rs", "b.rs", "c.rs"] {
        write_file(&root, &format!("src/{name}"), "fn main() {}\n");
    }
    let ctx = configured_context(&root);
    ctx.lsp().override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/rust-analyzer"),
    );

    const SEEDED_EVENTS: usize = 64;
    for index in 0..SEEDED_EVENTS {
        ctx.lsp().enqueue_event_for_test(LspEvent::Notification {
            server_kind: ServerKind::Rust,
            root: root.clone(),
            method: format!("custom/seeded/{index}"),
            params: None,
        });
    }
    assert_eq!(ctx.lsp().pending_event_count_for_test(), SEEDED_EVENTS);

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-drains-events",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src",
        }),
    );

    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["complete"], false);
    assert_eq!(
        ctx.lsp().pending_event_count_for_test(),
        0,
        "the warm collection must drain queued events before reading the store"
    );
    // No producer ever reported on the scoped files, so each one is named as
    // a coverage gap instead of rendering a confident empty summary.
    let gaps = response["gaps"].as_array().expect("coverage gaps");
    let mut uncovered = gaps
        .iter()
        .filter(|gap| gap["kind"] == "uncovered_file")
        .filter_map(|gap| gap["file"].as_str())
        .collect::<Vec<_>>();
    uncovered.sort();
    assert_eq!(
        uncovered,
        vec!["src/a.rs", "src/b.rs", "src/c.rs"],
        "every unanalyzed scoped file must be named: {response:#}"
    );
}

/// Scoped diagnostics no longer open documents in order to pull them, so the
/// old open/close bookkeeping has nothing to manage: a scoped request must
/// leave the document store exactly as the warm path found it.
#[test]
fn scoped_diagnostics_open_no_documents_and_close_none() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-close\"\n");
    let pre_opened = write_file(&root, "src/a.rs", "fn a() {}\n");
    let never_opened = [
        write_file(&root, "src/b.rs", "fn b() {}\n"),
        write_file(&root, "src/c.rs", "fn c() {}\n"),
    ];
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");

    let config = ctx.config().clone();
    let open_result = ctx
        .lsp()
        .ensure_file_open(&pre_opened, &config)
        .expect("pre-open document");
    assert_eq!(open_result.newly_opened.len(), 1);
    let pre_open_events = collect_lsp_notifications(&ctx, "custom/documentOpened", 1);
    assert_eq!(pre_open_events[0]["uri"], file_uri(&pre_opened));

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-opens-nothing",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src",
        }),
    );
    assert_eq!(response["success"], true, "inspect failed: {response:#}");

    assert!(ctx.lsp().document_is_open_for_test(&pre_opened));
    for file in &never_opened {
        assert!(
            !ctx.lsp().document_is_open_for_test(file),
            "scoped inspect must not open cold documents"
        );
        assert!(
            !ctx.lsp().has_diagnostic_report_for_file(file),
            "scoped inspect must not pull cold documents into the store"
        );
    }
    // The fake server emits one custom/documentOpened per didOpen and one
    // custom/documentClosed per didClose: after the warmup above, a scoped
    // inspect that touched documents would leave more of both in the queue.
    let leftover = ctx.lsp().drain_events().events;
    assert!(
        leftover.iter().all(|event| !matches!(
            event,
            LspEvent::Notification { method, .. }
                if method == "custom/documentOpened" || method == "custom/documentClosed"
        )),
        "scoped inspect must neither open nor close documents: {leftover:#?}"
    );
}

#[test]
fn inspect_command_diagnostics_missing_server_is_a_named_partial_gap() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-missing\"\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    ctx.lsp().override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server"),
    );

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-missing-server",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/main.rs",
        }),
    );

    // The warm path never attempts a spawn, so a missing binary surfaces as
    // per-file coverage gaps (the file has a registered producer but no
    // report) rather than a failed-producer entry.
    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["complete"], false);
    assert_eq!(response["summary"]["diagnostics"]["complete"], false);
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
        .unwrap_or_else(|| panic!("uncovered_file gap missing: {response:#}"));
    assert_eq!(gap["file"], "src/main.rs");
    assert_eq!(gap["categories"], json!(["diagnostics"]));
    assert!(gap["reason"]
        .as_str()
        .is_some_and(|reason| !reason.is_empty()));
}

#[test]
fn inspect_command_diagnostics_unsupported_file_is_not_returned_as_a_zero_result() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-no-server\"\n",
    );
    write_file(&root, "docs/readme.md", "# Title\n\nsome prose\n");
    let ctx = configured_context(&root);

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-no-server",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "docs/readme.md",
        }),
    );

    // A file no producer applies to is a named coverage gap: incomplete,
    // never a confident zero result.
    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["complete"], false);
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
        .unwrap_or_else(|| panic!("uncovered_file gap missing: {response:#}"));
    assert_eq!(gap["file"], "docs/readme.md");
    assert!(gap["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("no LSP producer")));
}

#[test]
fn inspect_command_inapplicable_server_is_not_returned_as_a_zero_result() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let storage_dir = root.join(".aft-test-storage");
    fs::create_dir_all(&root).expect("create project root");
    write_file(&root, "src/app.customts", "export const value = 1;\n");

    let server_id = "needs-marker-ls";
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            lsp_servers: vec![aft::config::UserServerDef {
                id: server_id.to_string(),
                extensions: vec!["customts".to_string()],
                binary: "needs-marker-ls".to_string(),
                args: Vec::new(),
                root_markers: vec!["needs-this-marker.json".to_string()],
                env: Default::default(),
                initialization_options: None,
                disabled: false,
            }],
            ..Config::default()
        },
    );
    crate::helpers::disable_in_process_file_watcher();
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false
        })),
    }));
    let configure_response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(
        configure_response["success"], true,
        "configure failed: {configure_response:#}"
    );
    ctx.lsp().override_binary(
        ServerKind::Custom(std::sync::Arc::from(server_id)),
        fake_server_path(),
    );

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-inapplicable-marker",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/app.customts",
        }),
    );

    // The producer is registered for the file type but its root marker is
    // absent, so it never runs and never reports: the scoped file must be a
    // named coverage gap, never a confident zero result.
    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["complete"], false);
    assert_eq!(response["summary"]["diagnostics"]["errors"], 0);
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
        .unwrap_or_else(|| panic!("uncovered_file gap missing: {response:#}"));
    assert_eq!(gap["file"], "src/app.customts");
    assert!(gap["reason"]
        .as_str()
        .is_some_and(|reason| !reason.is_empty()));
}

#[test]
fn inspect_command_diagnostics_details_honor_top_k() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-top-k\"\n");
    let main_rs = write_file(&root, "src/main.rs", "fn main() {}\n");
    let lib_rs = write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &main_rs, "fn main() {}\n");
    open_with_lsp(&ctx, &lib_rs, "pub fn lib() {}\n");

    let response = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-diagnostics-top-k",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 3,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 2);
    assert_eq!(response["summary"]["diagnostics"]["warnings"], 2);
    assert_eq!(
        response["details"]["diagnostics"]
            .as_array()
            .expect("diagnostics details")
            .len(),
        3,
        "diagnostics details should honor topK: {response:#}"
    );
}

#[test]
fn inspect_reports_one_failed_lsp_producer_without_hiding_other_results() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"inspect-gap\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "src/lib.rs", "pub fn rust_value() -> u8 { 1 }\n");
    write_file(
        &root,
        "web/package.json",
        "{\"name\":\"inspect-gap-web\"}\n",
    );
    write_file(&root, ".aftignore", "package.json\n");
    write_file(
        &root,
        "web/src/app.ts",
        "export function tsValue(): number { return 1; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );

    let rust_root_uri = file_uri(&root);
    let mut lsp = ctx.lsp();
    lsp.override_binary(ServerKind::Rust, fake_server_path());
    lsp.override_binary(ServerKind::TypeScript, fake_server_path());
    lsp.set_extra_env("AFT_FAKE_LSP_PULL", "1");
    lsp.set_extra_env("AFT_FAKE_LSP_INIT_CRASH_ROOT_URI", &rust_root_uri);
    drop(lsp);

    // The blocking tool call reads the warm working set instead of pulling
    // per-file, so the TypeScript findings must be warmed through the normal
    // edit path before the inspection runs.
    let app_ts = root.join("web/src/app.ts");
    open_with_lsp(
        &ctx,
        &app_ts,
        "export function tsValue(): number { return 1; }\n",
    );

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-producer-gap",
            "command": "inspect",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["inspect_terminal"], "fresh");
    assert_eq!(response["complete"], false);
    assert!(
        response["summary"]["diagnostics"]["errors"]
            .as_u64()
            .is_some_and(|errors| errors > 0),
        "the working TypeScript producer's diagnostics should survive: {response:#}"
    );
    let rust_gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["producer"] == "rust"))
        .unwrap_or_else(|| panic!("Rust producer gap missing: {response:#}"));
    assert_eq!(rust_gap["kind"], "failed_producer");
    assert_eq!(rust_gap["categories"], json!(["diagnostics"]));
    assert!(
        rust_gap["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "failed producer reason missing: {response:#}"
    );
}
