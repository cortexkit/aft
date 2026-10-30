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
use aft::commands::tool_call::handle_with_dispatch;
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::inspect::{
    inspect_phase_log_for_request, FileContribution, InspectCache, InspectCategory, InspectManager,
    InspectPhaseId, InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope,
};
use aft::lsp::client::LspEvent;
use aft::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use aft::lsp::registry::ServerKind;
use aft::lsp::roots::ServerKey;
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
    crate::test_helpers::fake_lsp::fake_server_binary()
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
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
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
            "indexes": {
                "trigram": false,
                "semantic": false,
                "callgraph": callgraph_store
            }
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
fn configured_restricted_context(root: &Path) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    ctx.isolate_cold_build_limiter_for_test(2);
    let configure = request(json!({
        "id": "configure-restricted",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "indexes": { "trigram": false, "semantic": false },
            "restrict_to_project_root": true
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

fn configured_context_with_diagnostics_timeout(root: &Path, timeout_ms: u64) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    ctx.isolate_cold_build_limiter_for_test(2);
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
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
            CallgraphStoreAccess::Off => panic!("callgraph index unexpectedly off in test"),
            CallgraphStoreAccess::Unavailable => {
                panic!("callgraph store unexpectedly unavailable in test")
            }
            CallgraphStoreAccess::Error(error) => {
                panic!("callgraph store failed in test: {error}")
            }
        }
    }
}

fn inspect_tool_call(ctx: &AppContext, payload: Value) -> Value {
    serde_json::to_value(handle_inspect_tool_call(&request(payload), ctx))
        .expect("inspect tool_call response serializes")
}

/// Run an inspect the way the native server dispatches it for this
/// OpenCode-configured root: under the engine's plugin route. Called without
/// that route, the engine treats the call as a plugin request that lost its
/// route and refuses outside paths with `access_route_missing`.
fn inspect_as_opencode(
    ctx: &AppContext,
    payload: Value,
    run: impl Fn(&AppContext, Value) -> Value,
) -> Value {
    aft::access_policy::with_native_route(request(payload.clone()), ctx, |_, ctx| {
        let value = run(ctx, payload.clone());
        aft::protocol::Response {
            id: "inspect".into(),
            success: value["success"] == true,
            data: value,
        }
    })
    .data
}

fn configure_fake_scope_lsp(ctx: &AppContext) {
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
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
    // The nonblocking path's Tier-1 soft-deadline refusal is re-asked by the
    // shared helper; every other refusal comes back for the fixtures below.
    crate::helpers::inspect_reasking_tier1_deadline(ctx, payload)
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
    let ctx = configured_restricted_context(&root);

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
fn inspect_partial_parse_keeps_dead_findings_and_names_the_parse_reason() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"main":"src/index.ts"}"#);
    write_file(&root, "src/index.ts", "console.log('entry');\n");
    write_file(&root, "src/dead.ts", "export function trulyDead() {}\n");
    write_file(&root, "src/broken.ts", "export function broken( {\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    let response = inspect(
        &ctx,
        json!({"id": "partial-parse", "command": "inspect", "sections": ["dead_code", "unused_exports"]}),
    );

    assert_eq!(response["success"], true, "{response:#}");
    for category in ["dead_code", "unused_exports"] {
        let summary = &response["summary"][category];
        assert_eq!(summary["complete"], false, "{response:#}");
        assert_eq!(summary["count"], 1, "{response:#}");
        assert!(
            response["details"][category]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| { item["file"] == "src/dead.ts" && item["symbol"] == "trulyDead" }),
            "{response:#}"
        );
        let error = summary["parse_errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|error| error["file"] == "src/broken.ts")
            .expect("broken file named");
        let reason = error["message"].as_str().expect("parse reason");
        assert!(!reason.is_empty());
        let text = response["text"].as_str().unwrap();
        assert!(
            text.contains(&format!("src/broken.ts ({reason})")),
            "parse reason missing: {text}"
        );
    }
    let counts = ctx.status_bar_count_values();
    assert_eq!(counts.dead_code, Some(1));
    assert_eq!(counts.unused_exports, Some(1));
    assert!(
        counts.tier2_stale,
        "partial counts must use the existing ~ marker"
    );
}

#[test]
fn inspect_partial_parse_reports_symbols_used_only_from_broken_files_with_caveat() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"main":"src/index.ts"}"#);
    write_file(&root, "src/index.ts", "console.log('entry');\n");
    write_file(
        &root,
        "src/nearby.ts",
        "export function onlyBrokenUser() {}\n",
    );
    write_file(&root, "src/broken.ts", "import { onlyBrokenUser } from './nearby';\nonlyBrokenUser();\nexport function broken( {\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    let response = inspect(
        &ctx,
        json!({"id": "partial-parse-reference", "command": "inspect", "sections": ["dead_code", "unused_exports"]}),
    );
    assert_eq!(response["success"], true, "{response:#}");
    for category in ["dead_code", "unused_exports"] {
        assert_eq!(
            response["summary"][category]["complete"], false,
            "{response:#}"
        );
        assert!(
            response["details"][category]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| {
                    item["file"] == "src/nearby.ts" && item["symbol"] == "onlyBrokenUser"
                }),
            "{response:#}"
        );
    }
    assert!(
        response["text"].as_str().unwrap().contains(
            "A symbol used only from files that could not be parsed may appear in this list."
        ),
        "caveat missing: {response:#}"
    );
}

#[test]
fn borrow_only_dead_code_is_a_named_gap_never_a_zero() {
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
    // A borrow-only root (a worktree) keeps its own inspect store but may only
    // read, never build, the shared call graph; no main-checkout store exists.
    ctx.set_cache_writer_capabilities(false, true);
    // An earlier count (from before the root became borrow-only) must not
    // survive as if it were current.
    ctx.update_status_bar_tier2(Some(7), None, None, None, false);
    assert_eq!(ctx.status_bar_count_values().dead_code, Some(7));

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-borrow-only",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports", "duplicates"],
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let dead_code = &response["summary"]["dead_code"];
    assert_eq!(dead_code["unavailable"], true, "{response:#}");
    assert_eq!(dead_code["complete"], false, "{response:#}");
    assert!(
        dead_code.get("count").is_none() && dead_code.get("by_language").is_none(),
        "an unavailable analysis must carry no count: {response:#}"
    );
    assert_eq!(
        dead_code["gaps"][0]["reason"],
        "dead code: unavailable in this worktree (call graph is borrow-only); run aft_inspect in the main checkout",
        "{response:#}"
    );
    let text = response["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("dead code: unavailable in this worktree (call graph is borrow-only)"),
        "the named gap must be rendered: {text}"
    );
    assert!(
        !text.lines().any(|line| line.starts_with("Dead code: ")),
        "no dead-code count may be rendered: {text}"
    );
    assert_eq!(
        ctx.status_bar_count_values().dead_code,
        None,
        "the status bar must show dead code as unknown, not the earlier count"
    );
    // The background refresh path reads counts from the inspect store; it must
    // not bring the old number back either.
    aft::runtime_drain::drain_inspect_events(&ctx);
    ctx.update_status_bar_tier2(None, None, None, None, false);
    // A `None` count renders as `D?` in the agent bar (see
    // response_finalize::agent_bar_marks_missing_categories_instead_of_zeroing_them).
    assert_eq!(ctx.status_bar_count_values().dead_code, None);

    // These analyses need no call graph and are computed locally even in a
    // borrow-only root, so they do report real numbers.
    assert_eq!(
        response["summary"]["unused_exports"]["count"], 1,
        "{response:#}"
    );
    assert!(
        response["summary"]["duplicates"]["count"].is_u64(),
        "{response:#}"
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
        indexes: aft::config::IndexesConfig {
            trigram: false,
            semantic: false,
            callgraph: true,
        },
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
    // A fresh dead-code result needs a usable callgraph; with the callgraph
    // index off the analysis is reported unavailable instead.
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);

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
fn scoped_inspect_does_not_wait_for_blocked_tier2() {
    let _env_lock = env_serial_lock();
    let (temp, owner) = fixture_project();
    write_file(&owner, "src/foo.ts", duplicate_fixture_source());
    let git = |root: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git fixture");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&owner, &["init"]);
    git(&owner, &["add", "src/foo.ts"]);
    git(
        &owner,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "fixture",
        ],
    );
    let linked = temp.path().join("linked");
    git(
        &owner,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );

    let gate_ready = temp.path().join("tier2-ready");
    let gate_release = temp.path().join("tier2-release");
    let _gate_root = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_GATE_ROOT", &linked.to_string_lossy());
    let _gate_ready = EnvVarGuard::set(
        "AFT_TEST_TIER2_REUSE_GATE_READY",
        &gate_ready.to_string_lossy(),
    );
    let _gate_release = EnvVarGuard::set(
        "AFT_TEST_TIER2_REUSE_GATE_RELEASE",
        &gate_release.to_string_lossy(),
    );
    let owner_ctx = configured_context(&owner);
    let linked_ctx = configured_context(&linked);
    let manager = linked_ctx.inspect_manager();
    let snapshot = InspectSnapshot::new(
        linked.clone(),
        linked_ctx.inspect_dir(),
        linked_ctx.config(),
        linked_ctx.symbol_cache(),
    );
    let tier2_root = linked.clone();
    let tier2_worker = thread::spawn(move || {
        manager.tier2_run_with_reuse_blocking(
            snapshot,
            InspectCategory::Duplicates,
            JobScope::for_project(tier2_root),
        )
    });
    wait_for_path_event(&gate_ready, "Tier-2 gate");
    let (tx, rx) = std::sync::mpsc::channel();
    thread::scope(|threads| {
        threads.spawn(|| {
            for ctx in [&linked_ctx, &owner_ctx] {
                ctx.lsp()
                    .override_binary(ServerKind::TypeScript, fake_server_path());
                ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
                for sections in [json!("diagnostics"), Value::Null] {
                    let response = handle_inspect_tool_call(
                        &request(json!({
                            "id": "scoped-blocked-tier2", "command": "inspect",
                            "scope": "src/foo.ts", "sections": sections,
                        })),
                        ctx,
                    );
                    tx.send(serde_json::to_value(response).unwrap()).unwrap();
                }
            }
        });
        // Release even on failure so the regression cannot strand a worker.
        let mut responses = Vec::new();
        for _ in 0..4 {
            let response = rx.recv_timeout(Duration::from_secs(15));
            let timed_out = response.is_err();
            responses.push(response);
            if timed_out {
                break;
            }
        }
        fs::write(&gate_release, b"release").expect("release Tier-2");
        tier2_worker.join().expect("Tier-2 worker finished");
        for response in responses {
            let response = response.expect("scoped inspect waited for blocked Tier-2");
            assert_eq!(response["success"], true, "{response:#}");
            assert!(
                response["summary"]["diagnostics"].is_object(),
                "{response:#}"
            );
            assert_eq!(
                response["summary"]["duplicates"]["complete"], true,
                "{response:#}"
            );
            assert_eq!(
                response["summary"]["duplicates"]["not_computed"], true,
                "{response:#}"
            );
            assert!(
                response["summary"]["duplicates"].get("count").is_none(),
                "{response:#}"
            );
            let phases = response["wait_stamp"]["phases"]
                .as_array()
                .expect("completed phase log");
            assert!(
                phases
                    .iter()
                    .any(|phase| phase["id"] == "stat_verification"),
                "{response:#}"
            );
            assert!(
                phases
                    .iter()
                    .all(|phase| phase["id"] != "tier2_rescan" && phase["id"] != "callgraph_ready"),
                "{response:#}"
            );
            let text = response["text"].as_str().unwrap();
            assert!(
                text.contains("duplicates")
                    && text.contains(
                        "not computed for scoped inspects; run aft_inspect without scope"
                    ),
                "{text}"
            );
            assert!(
                !text.contains("Duplicates: 0") && !text.contains("Dead code: 0"),
                "{text}"
            );
        }
    });
}

#[test]
fn scoped_inspect_building_discloses_progress_and_last_complete() {
    let _env_lock = env_serial_lock();
    for has_previous in [false, true] {
        let (temp, root) = fixture_project();
        write_file(&root, "src/foo.ts", duplicate_fixture_source());
        let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
        configure_fake_scope_lsp(&ctx);
        let manager = ctx.inspect_manager();
        manager.set_automatic_tier2_refresh_allowed(false);
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        let _root = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_GATE_ROOT", &root.to_string_lossy());
        let _ready = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_GATE_READY", &ready.to_string_lossy());
        let _category = EnvVarGuard::set("AFT_TEST_TIER2_REUSE_GATE_CATEGORY", "duplicates");
        let _release = EnvVarGuard::set(
            "AFT_TEST_TIER2_REUSE_GATE_RELEASE",
            &release.to_string_lossy(),
        );
        let snapshot = tier2_snapshot(&root, &ctx.inspect_dir());
        let scope = JobScope::for_project(root.clone());
        // Other categories may start before the duplicates worker. Their gate
        // events must not stand in for the duplicates build this test observes.
        fs::write(&release, b"release").unwrap();
        let other_category = manager.tier2_run_with_reuse(
            snapshot.clone(),
            InspectCategory::DeadCode,
            scope.clone(),
            None,
        );
        assert!(
            matches!(other_category, JobOutcome::Fresh { .. }),
            "{other_category:?}"
        );
        assert!(
            !ready.exists(),
            "a dead_code build signalled the duplicates gate"
        );
        if has_previous {
            let seed = manager.tier2_run_with_reuse(
                snapshot.clone(),
                InspectCategory::Duplicates,
                scope.clone(),
                None,
            );
            assert!(matches!(seed, JobOutcome::Fresh { .. }), "{seed:?}");
            fs::remove_file(&ready).unwrap();
            write_file(&root, "src/foo.ts", alternate_duplicate_fixture_source());
        }
        fs::remove_file(&release).unwrap();
        // Run inline on the joined thread so builder cleanup finishes before
        // the fixture drops its process-wide hook values.
        let worker = thread::spawn(move || {
            manager.tier2_run_with_reuse(snapshot, InspectCategory::Duplicates, scope, None)
        });
        wait_for_path_event(&ready, "Tier-2 gate");
        let response = inspect_tool_call(
            &ctx,
            json!({
                "id": "building", "command": "inspect", "scope": "src", "sections": "duplicates"
            }),
        );
        let unscoped = if !has_previous {
            Some(inspect_tool_call(
                &ctx,
                json!({
                    "id": "building-unscoped", "command": "inspect", "sections": "duplicates"
                }),
            ))
        } else {
            None
        };
        // Always release before assertions: a regression must not strand the worker.
        fs::write(&release, b"release").unwrap();
        let outcome = worker.join().unwrap();
        // The unscoped request also starts other enabled categories. Drain them
        // before dropping the process-wide hook values.
        let hang_deadline = Instant::now() + Duration::from_secs(90);
        while ctx.inspect_manager().tier2_any_in_flight() {
            assert!(
                Instant::now() < hang_deadline,
                "timed out draining Tier-2 workers after gate release"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(outcome, JobOutcome::Fresh { .. }), "{outcome:?}");
        if let Some(unscoped) = unscoped {
            assert_eq!(unscoped["success"], true, "{unscoped:#}");
            assert_eq!(
                unscoped["summary"]["duplicates"]["building"]["state"], "building",
                "{unscoped:#}"
            );
            assert!(
                !unscoped["summary"]["duplicates"]["gaps"][0]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("retry"),
                "{unscoped:#}"
            );
        }
        assert_eq!(response["success"], true, "{response:#}");
        assert_eq!(response["scope_files"], 1, "{response:#}");
        let summary = &response["summary"]["duplicates"];
        assert_eq!(summary["building"]["state"], "building", "{response:#}");
        assert_eq!(
            summary["building"]["progress"], "checking cache for 1 source files",
            "{response:#}"
        );
        assert!(
            summary["building"].get("estimated_remaining_ms").is_some(),
            "{response:#}"
        );
        let text = response["text"].as_str().unwrap();
        assert!(
            text.contains("building") && text.contains("estimate"),
            "{text}"
        );
        let reason = summary["background_reason"].as_str().unwrap();
        assert!(
            !reason.contains("retry")
                && !reason.contains("inspect_phase")
                && !reason.contains("tier2_rescan"),
            "{reason}"
        );
        assert!(
            !text.contains("no analyzed files under this scope"),
            "{text}"
        );
        if has_previous {
            assert!(
                summary["building"]["estimated_remaining_ms"].is_number(),
                "{response:#}"
            );
            assert_eq!(summary["last_complete"]["stale"], true, "{response:#}");
            assert!(
                summary["last_complete"]["age_s"].is_number(),
                "{response:#}"
            );
            assert!(
                summary["last_complete"]["payload"]["items"].is_array(),
                "{response:#}"
            );
            assert!(text.contains("last complete result is stale"), "{text}");
        } else {
            assert!(
                summary["building"]["estimated_remaining_ms"].is_null(),
                "{response:#}"
            );
            assert!(summary.get("last_complete").is_none(), "{response:#}");
        }
    }
}

#[cfg(debug_assertions)]
#[test]
fn scoped_inspect_inventory_skips_unrequested_scanners() {
    for blocking in [false, true] {
        let (_temp, root) = fixture_project();
        write_file(&root, "src/foo.ts", duplicate_fixture_source());
        write_file(&root, "outside.ts", "export const outside = 1;\n");
        let ctx = configured_context(&root);
        let req = request(json!({
            "id": "scoped-inventory", "command": "inspect", "scope": "src",
            "sections": "duplicates"
        }));
        let response = if blocking {
            handle_inspect_tool_call(&req, &ctx)
        } else {
            handle_inspect(&req, &ctx)
        };
        assert!(response.success, "{response:?}");
        // Each context has a cold, unique root, so memo reuse cannot mask an
        // unrequested scanner reading the files.
        assert_eq!(
            aft::inspect::scanners::metrics::file_read_count_for_debug(&root),
            0
        );
        assert_eq!(
            aft::inspect::scanners::todos::file_read_count_for_debug(&root),
            0
        );
        assert_eq!(response.data["scope_files"], 1, "{response:?}");
        for category in ["metrics", "todos", "diagnostics"] {
            assert_eq!(
                response.data["summary"][category],
                json!({"status": "not_requested"})
            );
        }
        assert!(
            response.data.get("no_files_matched_scope").is_none(),
            "{response:?}"
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn scoped_inspect_inventory_survives_blocked_metrics() {
    let _env_lock = env_serial_lock();
    let (temp, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "outside.ts", "export const outside = 1;\n");
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    let ready = temp.path().join("metrics-ready");
    let release = temp.path().join("metrics-release");
    let _root = EnvVarGuard::set("AFT_TEST_METRICS_GATE_ROOT", &root.to_string_lossy());
    let _ready = EnvVarGuard::set("AFT_TEST_METRICS_GATE_READY", &ready.to_string_lossy());
    let _release = EnvVarGuard::set("AFT_TEST_METRICS_GATE_RELEASE", &release.to_string_lossy());
    let (tx, rx) = std::sync::mpsc::channel();
    thread::scope(|threads| {
        threads.spawn(|| {
            tx.send(inspect_tool_call(
                &ctx,
                json!({
                    "id": "blocked-metrics", "command": "inspect", "scope": "src",
                    "sections": "metrics"
                }),
            ))
            .unwrap();
        });
        wait_for_path_event(&ready, "metrics scanner gate");
        let response = rx.recv_timeout(Duration::from_secs(15));
        // Release and reap the scanner before assertions, even on regression.
        fs::write(&release, b"release").unwrap();
        let completed = ctx.inspect_manager().submit_category_until(
            tier2_snapshot(&root, &ctx.inspect_dir()),
            InspectCategory::Metrics,
            JobScope::from_roots(root.clone(), vec![root.join("src")]),
            Instant::now() + Duration::from_secs(30),
        );
        assert!(
            matches!(completed, JobOutcome::Fresh { .. }),
            "{completed:?}"
        );
        let response = response.expect("inspect waited for a blocked metrics scanner");
        assert_eq!(response["success"], true, "{response:#}");
        assert_eq!(
            response["summary"]["metrics"]["status"], "unavailable",
            "{response:#}"
        );
        assert_eq!(
            response["summary"]["diagnostics"],
            json!({"status": "not_requested"})
        );
        assert_eq!(response["scope_files"], 1, "{response:#}");
        assert!(
            response.get("no_files_matched_scope").is_none(),
            "{response:#}"
        );
        assert!(
            response["text"]
                .as_str()
                .unwrap()
                .contains("scope: 1 root, 1 file"),
            "{response:#}"
        );
    });
}

#[test]
fn scoped_inspect_inventory_paths_agree_with_ignored_subfolders() {
    let (_temp, root) = fixture_project();
    write_file(&root, ".gitignore", "/src/ignored/\n/src/ignored.ts\n");
    write_file(&root, ".aftignore", "/src/vendored/\n");
    write_file(&root, "src/main.ts", "export const main = 1;\n");
    write_file(&root, "src/nested/lib.ts", "export const lib = 2;\n");
    write_file(&root, "src/ignored/hidden.ts", "export const hidden = 3;\n");
    write_file(&root, "src/ignored.ts", "export const ignored = 4;\n");
    write_file(&root, "src/vendored/dep.ts", "export const dep = 5;\n");
    write_file(&root, "src/node_modules/pkg.ts", "export const pkg = 6;\n");
    write_file(&root, "outside/other.ts", "export const other = 7;\n");
    let ctx = configured_context(&root);
    for (scope, expected) in [
        (json!("src"), 2),
        (json!(["src", "src/nested", "src/main.ts"]), 2),
        (json!("src/main.ts"), 1),
        (json!("src/ignored"), 0),
        (json!("src/ignored.ts"), 0),
        (json!("src/vendored"), 0),
        (json!("src/node_modules"), 0),
    ] {
        let req = request(json!({
            "id": "scope-ignore-parity", "command": "inspect", "scope": scope,
            "sections": "duplicates"
        }));
        let nonblocking = handle_inspect(&req, &ctx);
        let blocking = handle_inspect_tool_call(&req, &ctx);
        assert!(nonblocking.success, "{nonblocking:?}");
        assert!(blocking.success, "{blocking:?}");
        assert_eq!(blocking.data["scope_files"], expected, "{blocking:?}");
        assert_eq!(
            nonblocking.data["scope_files"], blocking.data["scope_files"],
            "{nonblocking:?}"
        );
    }
}

#[test]
fn scoped_inspect_views_worktree_reports_checkout_only_dead_function() {
    let _env_lock = env_serial_lock();
    let (temp, owner) = fixture_project();
    write_file(
        &owner,
        "main.ts",
        "import { used } from './target';\nused();\n",
    );
    write_file(&owner, "target.ts", "export function used() {}\n");
    write_file(
        &owner,
        "a_outside.ts",
        &(0..110)
            .map(|i| format!("export function outside_{i}() {{}}\n"))
            .collect::<String>(),
    );
    let git = |root: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&owner, &["init", "--quiet"]);
    git(&owner, &["add", "."]);
    git(
        &owner,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let linked = temp.path().join("linked");
    git(
        &owner,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    write_file(
        &linked,
        "target.ts",
        "export function used() {}\nexport function checkoutOnlyDead() {}\n",
    );
    let storage = temp.path().join("storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage.clone()),
            ..Config::default()
        },
    )
    .with_test_checkpoint_store();
    ctx.isolate_cold_build_limiter_for_test(2);
    let configured = handle_configure(
        &request(json!({
            "id": "configure-view", "command": "configure", "harness": "opencode",
            "project_root": linked, "storage_dir": storage,
            "config": crate::helpers::user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": true },
                "views": {"enabled": true}
            }))
        })),
        &ctx,
    );
    assert!(configured.success, "{configured:?}");
    assert!(
        !ctx.callgraph_writer(),
        "worktree must remain borrow-only for legacy artifacts"
    );
    let publish = aft::views::assembly::AssemblyRequest {
        storage,
        project_root: linked.clone(),
        family: aft::search_index::artifact_cache_key(&linked),
        scope: aft::path_identity::project_scope_key(&linked),
        desired_head: aft::views::assembly::head_tree_fingerprint(
            &aft::alias::head_tree_entries(&linked).unwrap(),
        ),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    aft::views::assembly::publish_checkout(&publish).unwrap();
    configure_fake_scope_lsp(&ctx);
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "worktree-dead", "command": "inspect", "scope": "target.ts",
            "sections": ["dead_code", "unused_exports"]
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert!(
        response["summary"]["dead_code"]
            .get("unavailable")
            .is_none(),
        "{response:#}"
    );
    assert!(
        response["wait_stamp"]["phases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|phase| phase["id"] == "callgraph_ready"),
        "{response:#}"
    );
    assert!(
        dead_code_items(&response)
            .iter()
            .any(|(_, symbol)| symbol == "checkoutOnlyDead"),
        "{response:#}"
    );
    assert!(
        !dead_code_items(&response)
            .iter()
            .any(|(_, symbol)| symbol == "used"),
        "{response:#}"
    );
    assert!(
        unused_export_items(&response)
            .iter()
            .any(|(_, symbol)| symbol == "checkoutOnlyDead"),
        "{response:#}"
    );
    assert!(!fs::read_to_string(owner.join("target.ts"))
        .unwrap()
        .contains("checkoutOnlyDead"));
    // An edit after the view was bound is reflected at once: the checkout
    // view is verified against the tree for every request, so an older view
    // is never passed off as this checkout.
    write_file(
        &linked,
        "target.ts",
        "export function used() {}\nexport function nextDead() {}\n",
    );
    let current = inspect_tool_call(
        &ctx,
        json!({
            "id": "worktree-view-current", "command": "inspect", "scope": "target.ts", "sections": "dead_code"
        }),
    );
    assert!(
        current["summary"]["dead_code"].get("unavailable").is_none(),
        "{current:#}"
    );
    let items = dead_code_items(&current);
    assert!(
        items.iter().any(|(_, symbol)| symbol == "nextDead"),
        "{current:#}"
    );
    assert!(
        !items.iter().any(|(_, symbol)| symbol == "checkoutOnlyDead"),
        "{current:#}"
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
    // This fixture disables the callgraph. Its unavailable dead-code analysis
    // makes an unscoped result PARTIAL, independently of the retired deadline.
    assert_eq!(response["complete"], false, "response: {response:#}");
    assert!(
        response["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("PARTIAL — dead code unavailable:")),
        "response: {response:#}"
    );
    assert_eq!(
        response["summary"]["dead_code"]["reason"],
        "call graph is disabled (indexes.callgraph=false)"
    );
    assert!(
        response["summary"]["duplicates"]["count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "the retired deadline must not prevent duplicate analysis: {response:#}"
    );
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

fn alternate_duplicate_fixture_source() -> &'static str {
    r#"
export function formatRecords(items: string[]) {
  let output = "";
  for (const item of items) {
    if (item.length > 4) {
      output += item.toUpperCase();
    } else {
      output += item.toLowerCase();
    }
    output += ":";
  }
  return output;
}
"#
}

#[test]
fn inspect_tool_call_refuses_space_separated_scope_as_one_missing_path() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "crates/one/src/lib.rs", "pub fn one() {}\n");
    write_file(&root, "crates/two/src/lib.rs", "pub fn two() {}\n");
    let ctx = configured_context(&root);
    let spelling = "crates/one/src crates/two/src";

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-space-separated-scope",
            "command": "inspect",
            "scope": spelling,
            "sections": ["duplicates"],
        }),
    );

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["code"], "path_not_found", "response: {response:#}");
    let message = response["message"].as_str().expect("error message");
    assert!(message.contains(spelling), "response: {response:#}");
    assert!(
        message.contains("scope accepts one path string or an array of paths"),
        "response: {response:#}"
    );
}

#[test]
fn inspect_tool_call_distinguishes_outside_root_from_missing_scope() {
    let (_temp_dir, root) = fixture_project();
    let outside = tempfile::tempdir().expect("outside project tempdir");
    let ctx = configured_restricted_context(&root);

    let outside_response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-outside-scope",
            "command": "inspect",
            "scope": outside.path().to_string_lossy(),
        }),
    );
    let missing_response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-missing-scope",
            "command": "inspect",
            "scope": "missing/src",
        }),
    );

    assert_eq!(
        outside_response["code"], "path_outside_root",
        "response: {outside_response:#}"
    );
    assert_eq!(
        missing_response["code"], "path_not_found",
        "response: {missing_response:#}"
    );
    assert_ne!(outside_response["code"], missing_response["code"]);
}

#[test]
fn inspect_tool_call_refuses_mixed_scope_array_with_missing_element() {
    let (_temp_dir, root) = fixture_project();
    fs::create_dir_all(root.join("crates/one/src")).expect("first scope root");
    fs::create_dir_all(root.join("crates/two/src")).expect("second scope root");
    let ctx = configured_context(&root);

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-partial-scope",
            "command": "inspect",
            "scope": ["crates/one/src", "crates/missing/src", "crates/two/src"],
        }),
    );

    assert_eq!(response["success"], false, "response: {response:#}");
    assert_eq!(response["code"], "path_not_found", "response: {response:#}");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains("crates/missing/src")),
        "response: {response:#}"
    );
}

#[test]
fn inspect_tool_call_multiple_roots_reports_combined_corpus_and_duplicates() {
    let (_temp_dir, root) = fixture_project();
    for file in ["left.ts", "right.ts"] {
        write_file(
            &root,
            &format!("crates/alpha/src/{file}"),
            duplicate_fixture_source(),
        );
        write_file(
            &root,
            &format!("crates/beta/src/{file}"),
            alternate_duplicate_fixture_source(),
        );
    }
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    configure_fake_scope_lsp(&ctx);
    tier2_run(
        &ctx,
        &[
            "dead_code",
            "unused_exports",
            "duplicates",
            "cycles",
            "complexity",
        ],
    );

    let call = |id: &str, scope: Value| {
        inspect_tool_call(
            &ctx,
            json!({
                "id": id,
                "command": "inspect",
                "scope": scope,
                "sections": ["duplicates"],
            }),
        )
    };
    let alpha = call("inspect-alpha-scope", json!("crates/alpha/src"));
    let beta = call("inspect-beta-scope", json!("crates/beta/src"));
    let combined = call(
        "inspect-combined-scope",
        json!(["crates/alpha/src", "crates/beta/src"]),
    );

    for response in [&alpha, &beta, &combined] {
        assert_eq!(response["success"], true, "response: {response:#}");
    }
    assert_eq!(
        combined["scope_roots"],
        json!(["crates/alpha/src", "crates/beta/src"])
    );
    assert_eq!(combined["scope_files"], 4);
    assert!(
        combined["text"]
            .as_str()
            .is_some_and(|text| text.lines().any(|line| line == "scope: 2 roots, 4 files")),
        "response: {combined:#}"
    );
    let alpha_count = alpha["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("alpha duplicate count");
    let beta_count = beta["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("beta duplicate count");
    assert!(alpha_count > 0, "response: {alpha:#}");
    assert!(beta_count > 0, "response: {beta:#}");
    assert_eq!(
        combined["summary"]["duplicates"]["count"],
        alpha_count + beta_count,
        "disjoint scoped duplicate counts should add: {combined:#}"
    );
}

#[test]
fn inspect_tool_call_empty_analyzed_scope_uses_zero_denominator() {
    let (_temp_dir, root) = fixture_project();
    fs::create_dir_all(root.join("docs")).expect("docs-only scope root");
    write_file(&root, "src/main.ts", "export const outsideScope = 1;\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    tier2_run(
        &ctx,
        &[
            "dead_code",
            "unused_exports",
            "duplicates",
            "cycles",
            "complexity",
        ],
    );

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-empty-corpus-scope",
            "command": "inspect",
            "scope": "docs",
            "sections": ["duplicates"],
        }),
    );

    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["scope_roots"], json!(["docs"]));
    assert_eq!(response["scope_files"], 0);
    assert_eq!(response["no_files_matched_scope"], true);
    assert_eq!(response["summary"]["duplicates"]["total_analyzed_lines"], 0);
    let text = response["text"].as_str().expect("rendered inspect text");
    assert!(
        text.starts_with("FRESH\nscope: 1 root, 0 files (no analyzed files under this scope)\n"),
        "response: {response:#}"
    );
    assert!(
        text.contains(
            "Duplicates: 0 duplicated lines (0.0% of 0 analyzed lines) across 0 files, 0 groups"
        ),
        "response: {response:#}"
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

fn enable_automatic_rust_checks(ctx: &AppContext) {
    // These fixtures exercise automatic analyzer checks. Opt in explicitly so
    // changing the production default does not change their flycheck contract.
    ctx.update_config(|config| {
        config.lsp_servers.push(aft::config::UserServerDef {
            id: "rust".into(),
            initialization_options: Some(json!({"checkOnSave": true})),
            ..Default::default()
        });
    });
}

fn configure_fake_rust_lsp(ctx: &AppContext) {
    enable_automatic_rust_checks(ctx);
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
}

fn open_with_lsp(ctx: &AppContext, file: &Path, content: &str) {
    let config = ctx.config().clone();
    let (pre_snapshot, expected_versions) = {
        let mut lsp = ctx.lsp();
        lsp.drain_events();
        let pre_snapshot = lsp.snapshot_pre_edit_state(file);
        let expected_versions = lsp
            .notify_file_changed_versioned(file, content, &config)
            .expect("notify file changed");
        (pre_snapshot, expected_versions)
    };
    assert!(
        !expected_versions.is_empty(),
        "fake LSP should match {file:?}"
    );
    let result = aft::lsp::fresh_wait::wait_for_post_edit_diagnostics_unlocked(
        || ctx.lsp(),
        file,
        &expected_versions,
        &pre_snapshot,
        std::time::Instant::now() + Duration::from_secs(30),
    );
    assert!(
        result.complete(),
        "fake LSP must report the notified versions for {file:?}: {result:?}"
    );
    assert!(
        !result.diagnostics.is_empty(),
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

/// On the nonblocking inspect path, scope must not change collection work: a
/// scoped request may not spawn servers, open documents, or pull diagnostics
/// beyond what the warm path already did. (A blocking scoped inspect does
/// open the scoped files; see the `scoped_blocking_inspect_*` tests.)
/// Assertions target the producer/LSP call surface (server
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

/// A string file scope selects only the owning TypeScript workspace, even
/// when other packages and languages have source files in the same project.
#[test]
fn scoped_typescript_file_inspect_starts_only_its_workspace() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{\"name\":\"root\"}\n");
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "src/lib.rs", "pub fn root() {}\n");
    write_file(&root, "scripts/other.sh", "echo other\n");
    write_file(&root, "scripts/second.sh", "echo second\n");
    write_file(
        &root,
        "packages/dashboard/src-tauri/Cargo.toml",
        "[package]\nname = \"dashboard\"\nversion = \"0.1.0\"\n",
    );
    write_file(
        &root,
        "packages/dashboard/src-tauri/src/lib.rs",
        "pub fn dashboard() {}\n",
    );
    write_file(
        &root,
        "packages/plugin/package.json",
        "{\"name\":\"plugin\"}\n",
    );
    write_file(
        &root,
        "packages/plugin/tsconfig.json",
        "{\"references\":[{\"path\":\"./scripts\"}]}\n",
    );
    write_file(
        &root,
        "packages/plugin/scripts/tsconfig.json",
        "{\"compilerOptions\":{}}\n",
    );
    write_file(
        &root,
        "packages/plugin/scripts/sentinel.ts",
        "export const sentinel = 1;\n",
    );
    for package in [
        "cli",
        "pi-plugin",
        "e2e-tests",
        "docs",
        "retina-local-fs",
        "dashboard",
    ] {
        write_file(&root, &format!("packages/{package}/package.json"), "{}\n");
        write_file(
            &root,
            &format!("packages/{package}/src/index.ts"),
            "export const other = 2;\n",
        );
    }
    let ctx = configured_context(&root);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx.lsp()
        .override_binary(ServerKind::Bash, fake_server_path());

    let response = serde_json::to_value(handle_with_dispatch(
        &request(json!({
            "id": "inspect-scoped-ts-monorepo",
            "command": "tool_call",
            "name": "aft_inspect",
            "args": {
                "scope": "packages/plugin/scripts/sentinel.ts",
                "sections": "diagnostics"
            }
        })),
        &ctx,
        &|_, _| panic!("inspect tool calls use the inspect dispatcher"),
    ))
    .expect("inspect response serializes");
    let active = ctx.lsp().active_server_keys();
    assert_eq!(response["scope_files"], 1, "scope was lost: {response:#}");
    assert_eq!(active.len(), 1, "response: {response:#}");
    assert_eq!(active[0].kind, ServerKind::TypeScript);
    // No package installs its own TypeScript, so every package sees the same
    // (none) and one server at the outermost TypeScript root serves them all;
    // the scope still starts only that one server, not one per package.
    assert_eq!(
        active[0].root,
        crate::helpers::canonicalize_like_product(&root)
    );
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
    // A nested standalone workspace outside `spikes/` and fixture directories,
    // which the whole-project walk skips; this one must still get its own
    // server when the request is unscoped.
    write_file(
        &root,
        "tools/foreign/Cargo.toml",
        "[workspace]\n[package]\nname = \"foreign\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "tools/foreign/src/lib.rs", "pub fn foreign() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-scoped-owning-workspace",
            "sections": ["diagnostics"],
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
        key.root == crate::helpers::canonicalize_like_product(&root.join("tools/foreign"))
    }));
}

/// A Rust Cargo project whose `package.json` exists only to install a tool
/// (Cloudflare's `wrangler` here). The manifest is a TypeScript root marker but
/// not a TypeScript source file.
fn wrangler_rust_fixture() -> (tempfile::TempDir, PathBuf) {
    let (temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"wrangler-worker\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "src/lib.rs", "pub fn worker() {}\n");
    write_file(
        &root,
        "package.json",
        "{\n  \"private\": true,\n  \"devDependencies\": { \"wrangler\": \"4.0.0\" }\n}\n",
    );
    (temp_dir, root)
}

/// Both the Rust and the TypeScript producers run the fake LSP child, so a
/// TypeScript start is observable as a live server key rather than depending
/// on whether `typescript-language-server` happens to be installed.
fn configure_fake_rust_and_typescript_lsp(ctx: &AppContext) {
    configure_fake_rust_lsp(ctx);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
}

fn active_server_kinds(ctx: &AppContext) -> Vec<ServerKind> {
    ctx.lsp()
        .active_server_keys()
        .into_iter()
        .map(|key| key.kind)
        .collect()
}

/// A context whose Tier-2 aggregates are already built, so an unscoped inspect
/// returns a fresh payload with a diagnostics summary instead of refusing
/// while the callgraph is still building.
fn tier2_ready_context(root: &Path) -> AppContext {
    let ctx = configured_context_with_callgraph_store(root, true);
    ensure_callgraph_store_ready(&ctx);
    tier2_run(
        &ctx,
        &[
            "dead_code",
            "unused_exports",
            "duplicates",
            "cycles",
            "complexity",
        ],
    );
    ctx
}

fn not_applicable_producers(response: &Value) -> Vec<String> {
    response["summary"]["diagnostics"]["not_applicable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["producer"].as_str().map(str::to_owned))
        .collect()
}

fn failed_producers(response: &Value) -> Vec<String> {
    response["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|gap| gap["kind"] == "failed_producer")
        .filter_map(|gap| gap["producer"].as_str().map(str::to_owned))
        .collect()
}

/// Standalone OpenCode and Pi call inspect through `tool_call` with the
/// registered name `aft_inspect`. The agent-visible text must be the rendered
/// inspect text, as for `read` and `grep`, not the response serialized as JSON.
#[test]
fn tool_call_aft_inspect_text_is_the_rendered_inspect_text() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/main.rs", "fn main() {}\n// TODO: tidy\n");
    let ctx = configured_context(&root);

    let response = serde_json::to_value(handle_with_dispatch(
        &request(json!({
            "id": "tool-call-inspect-text",
            "command": "tool_call",
            "name": "aft_inspect",
            "arguments": {"scope": "src", "sections": ["todos", "diagnostics"]}
        })),
        &ctx,
        &|_, _| panic!("inspect tool calls use the inspect dispatcher"),
    ))
    .expect("inspect response serializes");

    let text = response["text"].as_str().expect("text");
    assert!(
        !text.trim_start().starts_with('{'),
        "text is serialized JSON: {text}"
    );
    // `src/main.rs` has no Cargo.toml, so Rust diagnostics are unknown and the
    // rendered text opens with the partial header that names rust.
    let mut lines = text.lines();
    assert!(
        lines.next().is_some_and(|line| line
            .starts_with("PARTIAL — diagnostics unknown: rust-analyzer")
            && line.contains("retry aft_inspect")),
        "{text}"
    );
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("scope: 1 root")),
        "{text}"
    );
    assert!(text.contains("TODOs: 1"), "{text}");
}

/// A scoped inspect selects producers from the files inside the scope only.
/// A JavaScript file elsewhere in the project (a checked-in `.mjs` test stub)
/// must not start TypeScript for a scope that contains only Rust.
#[test]
fn scoped_rust_inspect_does_not_start_typescript_for_js_outside_the_scope() {
    let (_temp_dir, root) = wrangler_rust_fixture();
    write_file(
        &root,
        "tests/usage-stub/index.mjs",
        "export default { fetch() { return new Response(\"ok\"); } };\n",
    );
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-scoped-rust-with-outside-js",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": "src/lib.rs",
        }),
    );

    assert_eq!(
        active_server_kinds(&ctx),
        vec![ServerKind::Rust],
        "a Rust-only scope must start only rust-analyzer: {response:#}"
    );
    assert!(
        !failed_producers(&response).contains(&"typescript".to_string()),
        "response: {response:#}"
    );
}

fn diagnostic_messages_for(response: &Value, file: &str) -> Vec<String> {
    response["details"]["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["file"] == file)
        .filter_map(|item| item["message"].as_str().map(str::to_owned))
        .collect()
}

/// `(file, cause reason)` of every scoped file inspect could not certify.
fn uncovered_files(response: &Value) -> Vec<(String, String)> {
    response["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|gap| gap["kind"] == "uncovered_file")
        .map(|gap| {
            (
                gap["file"].as_str().unwrap_or_default().to_string(),
                gap["cause"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn single_crate_fixture(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
    );
    let lib = write_file(&root, "src/lib.rs", "pub fn f() {}\n");
    (temp_dir, root, lib)
}

#[cfg(unix)]
#[test]
fn rust_worker_inspect_is_partial_and_never_invokes_cargo_on_path() {
    let _guard = super::lsp_manager_test::rust_worker_policy_test_guard();
    let (temp, root, _) = single_crate_fixture("worker-inspect");
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = temp.path().join("cargo-invocations");
    let cargo = bin.join("cargo");
    let real_cargo = which::which("cargo").unwrap();
    crate::test_helpers::executable_cache::write_executable(&cargo, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = check ]; then exit 1; fi\nexec '{}' \"$@\"\n",
        log.display(), real_cargo.display()
    ).as_bytes()).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let ctx = configured_context(&root);
    let mut config = (*ctx.config()).clone();
    config.inspect.diagnostics_timeout_ms = 10_000;
    ctx.set_config(config);
    {
        let mut lsp = ctx.lsp();
        lsp.override_binary(ServerKind::Rust, fake_server_path());
        lsp.set_extra_env("PATH", path.to_str().unwrap());
        // Pin the same recorder for the explicit check as well as PATH lookup;
        // inherited toolchain discovery must not bypass the fake Cargo.
        lsp.set_extra_env("CARGO", cargo.to_str().unwrap());
    }
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "worker-inspect", "command": "inspect", "scope": "src/lib.rs",
            "sections": "diagnostics", "worker_session": true,
        }),
    );
    assert!(
        !log.exists(),
        "worker inspect must never invoke Cargo: {:?}",
        fs::read_to_string(&log)
    );
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    assert_eq!(response["complete"], false);
    let text = response["text"].as_str().unwrap();
    eprintln!("worker inspect wording: {text}");
    assert_eq!(text.lines().next(), Some("PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo_check_disabled_for_worker: compile diagnostics are rust-analyzer-only for this session. Run cargo check via runon: \"linux,4c\" for compiler diagnostics."));
    assert!(!text.contains("retry aft_inspect"), "{text}");
    assert!(
        response["summary"]["diagnostics"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["reason_code"] == "cargo_check_disabled_for_worker"),
        "{response:#}"
    );
}

#[test]
fn rust_worker_inspect_capacity_refusal_is_partial_and_names_cap() {
    let _guard = super::lsp_manager_test::rust_worker_policy_test_guard();
    let fixtures = (0..2)
        .map(|_| single_crate_fixture("occupied-worker-slot"))
        .collect::<Vec<_>>();
    let mut occupied = Vec::new();
    for (_, _, file) in &fixtures {
        let mut manager = aft::lsp::manager::LspManager::new();
        manager.override_binary(ServerKind::Rust, fake_server_path());
        manager.disable_rust_cargo_for_worker();
        assert_eq!(
            crate::test_helpers::settled_lsp(&mut manager, |startup_manager| startup_manager
                .ensure_server_for_file(file, &Config::default()))
            .len(),
            1
        );
        occupied.push(manager);
    }
    let (_temp, root, _) = single_crate_fixture("refused-worker-slot");
    let ctx = configured_context(&root);
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "worker-inspect-cap", "command": "inspect", "scope": "src/lib.rs",
            "sections": "diagnostics", "worker_session": true,
        }),
    );
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    let text = response["text"].as_str().unwrap();
    assert!(text.contains("rust_analyzer_capacity_exhausted"), "{text}");
    assert!(text.contains("cap=2"), "{text}");
    assert_eq!(ctx.lsp().server_count(), 0);
}

fn scoped_diagnostics_inspect(ctx: &AppContext, id: &str, scope: &str) -> Value {
    inspect_tool_call(
        ctx,
        json!({
            "id": id,
            "command": "inspect",
            "scope": scope,
            "sections": "diagnostics",
            "topK": 20,
        }),
    )
}

/// [`inspect`] for a scope outside the project, dispatched as an OpenCode
/// host's request is. These fixtures live in the system temp directory, which
/// the engine lets an OpenCode session read without asking (as the plugin did
/// before the outside-access decision moved into the engine).
fn inspect_outside(ctx: &AppContext, payload: Value) -> Value {
    inspect_as_opencode(ctx, payload, inspect)
}

/// [`inspect_tool_call`] for a scope outside the project, dispatched as in
/// [`inspect_outside`].
fn inspect_tool_call_outside(ctx: &AppContext, payload: Value) -> Value {
    inspect_as_opencode(ctx, payload, inspect_tool_call)
}

/// [`scoped_diagnostics_inspect`] for a scope outside the project, dispatched
/// as in [`inspect_outside`].
fn scoped_diagnostics_inspect_outside(ctx: &AppContext, id: &str, scope: &str) -> Value {
    inspect_tool_call_outside(
        ctx,
        json!({
            "id": id,
            "command": "inspect",
            "scope": scope,
            "sections": "diagnostics",
            "topK": 20,
        }),
    )
}

#[cfg(unix)]
#[test]
fn rust_inspect_runs_cargo_with_checks_off_and_reuses_saved_record() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    if !std::process::Command::new(&cargo)
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipping rust_inspect_runs_cargo_with_checks_off_and_reuses_saved_record: cargo unavailable");
        return;
    }
    let (_temp_dir, root, lib) = single_crate_fixture("inspect-explicit-check");
    std::fs::write(
        &lib,
        "pub fn f() { let s = String::new(); let moved = s; println!(\"{}\", s); drop(moved); }\n",
    )
    .unwrap();
    assert!(std::process::Command::new(&cargo)
        .current_dir(&root)
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap()
        .success());
    let storage = tempfile::tempdir().unwrap();
    let trace = storage.path().join("cargo-runs");
    let wrapper = storage.path().join("cargo-wrapper");
    let source = format!(
        "#!/bin/sh\n[ \"$GIT_OPTIONAL_LOCKS\" = 0 ] || exit 7\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        trace.display(),
        Path::new(&cargo).display()
    );
    crate::test_helpers::executable_cache::link_cached_executable(&wrapper, source.as_bytes())
        .unwrap();
    let make_context = || {
        let ctx = configured_context(&root);
        ctx.update_config(|config| config.storage_dir = Some(storage.path().to_path_buf()));
        ctx.lsp()
            .override_binary(ServerKind::Rust, fake_server_path());
        ctx.lsp().set_extra_env("CARGO", wrapper.to_str().unwrap());
        ctx
    };
    let ctx = make_context();
    open_with_lsp(&ctx, &lib, &std::fs::read_to_string(&lib).unwrap());
    let native_only = inspect_warm_event_driven(
        &ctx,
        json!({
            "id": "inspect-native-only", "command": "inspect", "scope": "src", "sections": "diagnostics"
        }),
    );
    assert_eq!(
        native_only["complete"], false,
        "native diagnostics alone must stay partial: {native_only:#}"
    );
    let response = scoped_diagnostics_inspect(&ctx, "inspect-explicit-cargo", "src");
    let messages = diagnostic_messages_for(&response, "src/lib.rs");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("borrow of moved value")),
        "real borrow-check diagnostic missing: {response:#}"
    );
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );
    assert!(!response
        .to_string()
        .contains("rust-analyzer: cargo check still running"));
    let runs = || {
        std::fs::read_to_string(&trace)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("check "))
            .count()
    };
    assert_eq!(runs(), 1);
    let command = std::fs::read_to_string(&trace).unwrap();
    assert!(command.contains("check --workspace --message-format=json --locked --target-dir"));
    assert!(
        command.contains("rust-analyzer"),
        "must share analyzer target directory: {command}"
    );

    let second = scoped_diagnostics_inspect(&ctx, "inspect-explicit-cargo-again", "src");
    assert!(
        diagnostic_messages_for(&second, "src/lib.rs")
            .iter()
            .any(|message| message.contains("borrow of moved value")),
        "saved report missing: {second:#}"
    );
    assert_eq!(runs(), 1, "warm inspect must not run cargo twice");
    ctx.lsp().shutdown_all();
    drop(ctx);

    let ctx = make_context();
    let restarted = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-explicit-cargo-restart", "command": "inspect", "sections": "diagnostics"
        }),
    );
    assert!(
        diagnostic_messages_for(&restarted, "src/lib.rs")
            .iter()
            .any(|message| message.contains("borrow of moved value")),
        "persisted report missing: {restarted:#}"
    );
    assert_eq!(runs(), 1, "cold manager must reuse the persisted check");
    std::fs::write(&lib, "include!(concat!(\"\", \"borrow.rs\"));\n").unwrap();
    std::fs::write(
        root.join("src/borrow.rs"),
        "pub fn f() { let s = String::new(); let moved = s; println!(\"{}\", s); drop(moved); }\n",
    )
    .unwrap();
    let uncertified = scoped_diagnostics_inspect(&ctx, "inspect-dynamic-include", "src");
    assert_eq!(
        runs(),
        2,
        "unsupported fingerprints must not prevent the explicit compiler check"
    );
    assert_eq!(
        uncertified["complete"], false,
        "uncertifiable inputs must not be cached as clean: {uncertified:#}"
    );
    assert!(
        diagnostic_messages_for(&uncertified, "src/borrow.rs")
            .iter()
            .any(|message| message.contains("borrow of moved value")),
        "provisional compiler diagnostics must remain visible: {uncertified:#}"
    );
    ctx.lsp().shutdown_all();
}

#[cfg(unix)]
#[test]
fn rust_inspect_explicit_cargo_deadline_is_partial_and_kills_the_check() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    if !std::process::Command::new(&cargo)
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }
    let (_temp_dir, root, _lib) = single_crate_fixture("inspect-check-deadline");
    assert!(std::process::Command::new(&cargo)
        .current_dir(&root)
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap()
        .success());
    let storage = tempfile::tempdir().unwrap();
    let wrapper = storage.path().join("cargo-wrapper");
    let pid_file = storage.path().join("check-pid");
    let source = format!(
        "#!/bin/sh\nif [ \"$1\" = check ]; then echo $$ > '{}'; exec sleep 30; fi\nexec '{}' \"$@\"\n",
        pid_file.display(),
        Path::new(&cargo).display()
    );
    crate::test_helpers::executable_cache::link_cached_executable(&wrapper, source.as_bytes())
        .unwrap();
    let ctx = configured_context_with_diagnostics_timeout(&root, 3000);
    ctx.update_config(|config| config.storage_dir = Some(storage.path().to_path_buf()));
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx.lsp().set_extra_env("CARGO", wrapper.to_str().unwrap());
    let response = scoped_diagnostics_inspect(&ctx, "inspect-cargo-deadline", "src");
    assert!(
        response
            .to_string()
            .contains("cargo check running for aft_inspect; retry"),
        "must name the actual check owner: {response:#}"
    );
    assert!(!response
        .to_string()
        .contains("rust-analyzer: cargo check still running"));
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "response: {response:#}"
    );
    let pid: u32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The deadline sends the kill before inspect answers, but the OS reaps the
    // process a moment later; a loaded runner can take longer than one check.
    // A check that was never signalled stays alive for its full 30 s sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while aft::bash_background::process::is_process_alive(pid)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !aft::bash_background::process::is_process_alive(pid),
        "timed-out check must be terminated"
    );
    ctx.lsp().shutdown_all();
}

/// Language servers publish diagnostics only for files they were told about.
/// The fake here behaves like rust-analyzer without pull support: it publishes
/// for a file only after `didOpen`. A blocking scoped inspect opens the scoped
/// file, reports what the server published, and closes the file again.
#[test]
fn scoped_blocking_inspect_opens_scoped_files_for_a_push_only_server() {
    let (_temp_dir, root, lib) = single_crate_fixture("sweep-push");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-push", "src");

    assert!(
        diagnostic_messages_for(&response, "src/lib.rs")
            .iter()
            .any(|message| message.contains("test diagnostic error")),
        "the opened file's diagnostics must be reported: {response:#}"
    );
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"],
        json!({"files": 1, "examined": 1, "authoritative": 1, "not_examined": 0, "file_cap": 1000}),
        "response: {response:#}"
    );
    assert!(
        !ctx.lsp().document_is_open_for_test(&lib),
        "inspect must close the documents it opened"
    );

    // The fake answers the close with an empty publish, as TypeScript does.
    // That reply describes the closed document, not the file, and must not
    // replace the diagnostics inspect collected.
    wait_for_close_reply(&ctx);
    assert_eq!(
        ctx.lsp().get_diagnostics_for_file(&lib).len(),
        2,
        "the close reply erased the collected diagnostics"
    );
}

fn python_progress_fixture(mode: Option<&str>) -> (tempfile::TempDir, AppContext) {
    python_progress_fixture_with_timeout(mode, 3000)
}

fn dynamic_python_pull_fixture() -> (tempfile::TempDir, AppContext, PathBuf) {
    let (dir, ctx) = python_progress_fixture(None);
    let log = dir.path().join(".aft-test-storage/python-pull.log");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DYNAMIC_PULL", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_DYNAMIC_PULL_LOG", log.to_str().unwrap());
    (dir, ctx, log)
}

#[test]
fn scoped_python_dynamic_pull_replaces_startup_empty_push_with_real_error() {
    let (dir, ctx, log) = dynamic_python_pull_fixture();
    std::fs::write(dir.path().join("unrequested.py"), "other: int = 'wrong'\n").unwrap();
    let response = scoped_diagnostics_inspect(&ctx, "python-dynamic-pull", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    assert!(
        !response["summary"]["diagnostics"]["notes"]
            .to_string()
            .contains("2s"),
        "document pull must not describe a timing certificate: {response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["python"]["authority"][0]["rule"],
        "document_pull",
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["python"]["authority"][0]
            ["no_progress_grace_ms"],
        0,
        "{response:#}"
    );
    assert!(
        diagnostic_messages_for(&response, "broken.py")
            .iter()
            .any(|m| m.contains("test pull diagnostic")),
        "{response:#}"
    );
    let events = std::fs::read_to_string(log).unwrap();
    assert!(events.contains("register-accepted"));
    assert!(events.contains("pull registered=true"));
    assert!(!events.contains("pull registered=false"), "{events}");
    assert!(
        !events.contains("unrequested.py"),
        "the scoped pull must not analyze unopened neighbors: {events}"
    );
}

#[test]
fn scoped_python_dynamic_pull_failures_cannot_certify_startup_empty_push() {
    for failure in [
        "AFT_FAKE_LSP_PULL_ERROR",
        "AFT_FAKE_LSP_PULL_METHOD_NOT_FOUND",
        "AFT_FAKE_LSP_PULL_UNCHANGED",
        "AFT_FAKE_LSP_PULL_CANCEL",
    ] {
        let (_dir, ctx, log) = dynamic_python_pull_fixture();
        ctx.lsp().set_extra_env(
            failure,
            if failure == "AFT_FAKE_LSP_PULL_CANCEL" {
                "always"
            } else {
                "1"
            },
        );
        let response = scoped_diagnostics_inspect(&ctx, "python-pull-failed", "broken.py");
        let diagnostics = &response["summary"]["diagnostics"];
        assert_eq!(diagnostics["complete"], false, "{failure}: {response:#}");
        assert!(diagnostics["errors"].is_null(), "{failure}: {response:#}");
        assert!(
            !uncovered_files(&response).is_empty(),
            "{failure}: {response:#}"
        );
        let events = std::fs::read_to_string(log).unwrap();
        assert!(
            events.contains("pull registered=true"),
            "{failure}: {events}"
        );
        assert!(
            !events.contains("pull registered=false"),
            "{failure}: {events}"
        );
    }
}

#[test]
fn scoped_python_dynamic_pull_waits_for_registration_after_open() {
    let (_dir, ctx, log) = dynamic_python_pull_fixture();
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_REGISTER_DIAGNOSTICS_DELAY_MS", "600");
    let response = scoped_diagnostics_inspect(&ctx, "python-late-pull-registration", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    let events = std::fs::read_to_string(log).unwrap();
    assert!(
        events.contains("pull registered=true") && !events.contains("pull registered=false"),
        "{events}"
    );
}

#[test]
fn scoped_python_dynamic_pull_opens_large_documents_before_starting_analysis() {
    let (dir, ctx, log) = dynamic_python_pull_fixture();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("a.py"), "value: int = 'wrong'\n").unwrap();
    // Exercise stdin backpressure, not CPU load: the fake stops reading for
    // 600ms during a pull, so all large bodies must precede those requests.
    std::fs::write(
        root.join("b.py"),
        format!("# {}\n", "transport padding ".repeat(131_072)),
    )
    .unwrap();
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL_DELAY_MS", "600");
    let response = inspect_tool_call(
        &ctx,
        json!({"id":"python-open-before-pull","command":"inspect","scope":["a.py","b.py"],"sections":"diagnostics","topK":20}),
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 2,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    let events = std::fs::read_to_string(log).unwrap();
    let first_pull = events.find("pull registered=true").unwrap();
    assert!(
        events.find("a.py").unwrap() < first_pull && events.find("b.py").unwrap() < first_pull,
        "{events}"
    );
}

#[test]
fn review_python_background_pull_preserves_queued_rust_alert_once() {
    let (dir, ctx, _log) = dynamic_python_pull_fixture();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'mixed-alerts'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let rust = root.join("src/lib.rs");
    std::fs::write(&rust, "pub fn broken() {}\n").unwrap();
    configure_fake_rust_lsp(&ctx);
    let config = ctx.config();
    let rust_key = crate::test_helpers::settled_lsp(&mut ctx.lsp(), |manager| {
        manager.ensure_file_open(&rust, &config)
    })
    .unwrap()
    .server_keys
    .into_iter()
    .find(|key| key.kind == ServerKind::Rust)
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ctx.lsp().drain_events();
        if ctx.lsp().producer_has_settled(&rust_key) {
            break;
        }
        assert!(Instant::now() < deadline, "Rust startup did not settle");
        thread::sleep(Duration::from_millis(10));
    }
    let signal = root.join("pull-started");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_DELAY_SIGNAL", signal.to_str().unwrap());
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL_DELAY_MS", "500");
    let python = root.join("broken.py");
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(&python, &config)
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !signal.exists() {
        ctx.lsp().drain_events();
        assert!(
            Instant::now() < deadline,
            "background Python pull did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ctx.lsp().enqueue_event_for_test(LspEvent::Notification {
        // The client's own root and a URI made from the product-normalized path:
        // on Windows `fs::canonicalize` gives a verbatim `\\?\` spelling that
        // matches no client root and is not a valid `file://` URI, so the
        // publication would be dropped before alert ingestion.
        server_kind: ServerKind::Rust, root: rust_key.root.clone(),
        method: "textDocument/publishDiagnostics".into(),
        params: Some(json!({"uri":file_uri(&rust),"version":0,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":1,"message":"queued Rust alert"}]})),
    });
    // Do not drain here: the background completion must consume the queued Rust
    // publication while applying the Python response, then retain the Rust
    // publication as an accepted snapshot.
    loop {
        if ctx
            .lsp()
            .get_diagnostics_for_file(&python)
            .iter()
            .any(|d| d.message == "test pull diagnostic")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background Python pull did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut accepted = ctx.lsp().drain_events().accepted_snapshots;
    accepted.extend(ctx.lsp().drain_events().accepted_snapshots);
    let rust_snapshots = accepted
        .into_iter()
        .filter(|s| {
            s.diagnostics
                .iter()
                .any(|d| d.message == "queued Rust alert")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rust_snapshots.len(),
        1,
        "the queued publication must reach alert ingestion exactly once"
    );
    let mut state = aft::alert_state::AlertDeltaState::default();
    let mut renderer = aft::response_finalize::alert_render::AlertEngine::default();
    let first = &rust_snapshots[0];
    accept_progress_alerts(
        &root,
        &mut state,
        &mut renderer,
        vec![aft::alert_state::AcceptedDiagnosticSnapshot::new(
            first.server_key.clone(),
            first.document_version,
            Vec::new(),
        )],
    );
    let transitions = accept_progress_alerts(&root, &mut state, &mut renderer, rust_snapshots);
    assert_eq!(transitions.len(), 1);
    assert!(renderer
        .finalize("progress-alerts", &root, "read")
        .is_some());
}

#[test]
fn python_dynamic_pull_populates_status_and_alert_source_without_inspect() {
    let (dir, ctx, log) = dynamic_python_pull_fixture();
    let file = std::fs::canonicalize(dir.path()).unwrap().join("broken.py");
    let config = ctx.config();
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(&file, &config)
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut snapshots = Vec::new();
    loop {
        snapshots.extend(ctx.lsp().drain_events().accepted_snapshots);
        let counts = ctx.status_bar_count_values();
        if counts.errors == Some(1)
            && snapshots.iter().any(|s| {
                s.diagnostics
                    .iter()
                    .any(|d| d.message == "test pull diagnostic")
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no-inspect warm E1/accepted pull snapshot missing: {counts:?}; events={}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!std::fs::read_to_string(log)
        .unwrap()
        .contains("pull registered=false"));
}

#[test]
fn scoped_python_dynamic_pull_respects_disable_pull_diagnostics() {
    let (_dir, ctx, log) = dynamic_python_pull_fixture();
    ctx.update_config(|config| {
        config.lsp_servers.push(aft::config::UserServerDef {
            id: "python".into(),
            extensions: vec!["py".into()],
            binary: String::new(),
            args: Vec::new(),
            root_markers: vec!["pyproject.toml".into()],
            env: Default::default(),
            initialization_options: Some(json!({"disablePullDiagnostics":true})),
            disabled: false,
        })
    });
    let response = scoped_diagnostics_inspect(&ctx, "python-disabled-pull", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(
        diagnostic_messages_for(&response, "broken.py")
            .iter()
            .any(|m| m.contains("test diagnostic error")),
        "{response:#}"
    );
    let events = std::fs::read_to_string(log).unwrap();
    assert!(events.contains("disabled=true"), "{events}");
    assert!(
        !events.contains("register-accepted") && !events.contains("pull registered="),
        "{events}"
    );
}

fn open_dynamic_python(
    ctx: &AppContext,
    file: &Path,
    expected: usize,
) -> Vec<aft::alert_state::AcceptedDiagnosticSnapshot> {
    let config = ctx.config();
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(file, &config)
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut accepted = Vec::new();
    loop {
        accepted.extend(ctx.lsp().drain_events().accepted_snapshots);
        if ctx.status_bar_count_values().errors == Some(expected) && !accepted.is_empty() {
            return accepted;
        }
        assert!(
            Instant::now() < deadline,
            "warm pull did not produce its count and accepted observation"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn python_dynamic_pull_refresh_updates_warm_counts_and_alerts_without_inspect() {
    use aft::alert_state::{AcceptedDiagnosticSnapshot, AlertDeltaState};
    use aft::response_finalize::alert_render::AlertEngine;
    let (dir, ctx, log) = dynamic_python_pull_fixture();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let file = root.join("broken.py");
    let mut accepted = open_dynamic_python(&ctx, &file, 1);
    let first = accepted.pop().unwrap();
    let mut state = AlertDeltaState::default();
    let mut renderer = AlertEngine::default();
    accept_progress_alerts(
        &root,
        &mut state,
        &mut renderer,
        vec![AcceptedDiagnosticSnapshot::new(
            first.server_key.clone(),
            first.document_version,
            Vec::new(),
        )],
    );
    accept_progress_alerts(&root, &mut state, &mut renderer, vec![first]);
    assert!(renderer
        .finalize("progress-alerts", &root, "read")
        .is_some());
    let dependency = root.join("dependency.py");
    std::fs::write(&dependency, "changed = True\n").unwrap();
    let config = ctx.config();
    ctx.lsp()
        .notify_files_watched_changed(&[(dependency, lsp_types::FileChangeType::CHANGED)], &config)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut clean = Vec::new();
    loop {
        clean.extend(
            ctx.lsp()
                .drain_events()
                .accepted_snapshots
                .into_iter()
                .filter(|snapshot| snapshot.diagnostics.is_empty()),
        );
        if ctx.status_bar_count_values().errors == Some(0) && !clean.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "server refresh did not drive a clean open-file pull"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let transitions =
        accept_progress_alerts(&root, &mut state, &mut renderer, vec![clean.pop().unwrap()]);
    assert!(transitions
        .iter()
        .any(|transition| !transition.closed.is_empty()));
    assert!(renderer
        .finalize("progress-alerts", &root, "read")
        .is_none());
    assert!(std::fs::read_to_string(log)
        .unwrap()
        .contains("refresh-requested"));
}

#[test]
fn python_dynamic_pull_unregistration_prevents_new_requests() {
    let (dir, ctx, log) = dynamic_python_pull_fixture();
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_UNREGISTER_DIAGNOSTICS", "1");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let file = root.join("broken.py");
    open_dynamic_python(&ctx, &file, 1);
    let config = ctx.config();
    ctx.lsp()
        .notify_files_watched_changed(
            &[(file.clone(), lsp_types::FileChangeType::CHANGED)],
            &config,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let drained = ctx.lsp().drain_events();
        if drained.events.iter().any(|event| matches!(event, LspEvent::Notification { method, .. } if method == "custom/diagnosticsUnregistered")) { break; }
        assert!(
            Instant::now() < deadline,
            "unregistration acknowledgement missing"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let before = std::fs::read_to_string(&log)
        .unwrap()
        .matches("pull registered=")
        .count();
    let results = aft::lsp::manager::pull_file_diagnostics_unlocked(
        || ctx.lsp(),
        &file,
        &config,
        Some(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(
        results.iter().all(|result| matches!(
            result.outcome,
            aft::lsp::manager::PullFileOutcome::PullNotSupported
        )),
        "{results:?}"
    );
    let events = std::fs::read_to_string(log).unwrap();
    assert_eq!(
        events.matches("pull registered=").count(),
        before,
        "{events}"
    );
    assert!(!events.contains("pull registered=false"), "{events}");
}

fn python_progress_fixture_with_timeout(
    mode: Option<&str>,
    timeout_ms: u64,
) -> (tempfile::TempDir, AppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = 'progress-fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    std::fs::write(root.join("broken.py"), "value: int = 'wrong'\n").unwrap();
    let ctx = configured_context_with_diagnostics_timeout(&root, timeout_ms);
    ctx.lsp()
        .override_binary(ServerKind::Python, fake_server_path());
    if let Some(mode) = mode {
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_ANALYSIS_PROGRESS", mode);
    }
    (dir, ctx)
}

#[test]
fn scoped_python_progress_waits_past_startup_empty_publication() {
    let (_dir, ctx) = python_progress_fixture(Some("finish"));
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-finish", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

#[test]
fn scoped_python_progress_without_end_is_pending_not_clean() {
    let (_dir, ctx) = python_progress_fixture(Some("never"));
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-never", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "{response:#}"
    );
    assert!(
        response.to_string().contains("analysis_progress_pending"),
        "{response:#}"
    );
}

#[test]
fn scoped_python_progress_unsupported_retains_first_publication() {
    let (_dir, ctx) = python_progress_fixture(None);
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-unsupported", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["python"]["authority"][0]["rule"],
        "first_publish_no_analysis_progress",
        "{response:#}"
    );
}

#[test]
fn scoped_python_progress_end_before_post_open_publication_is_pending() {
    let (_dir, ctx) = python_progress_fixture(Some("end_before_publication"));
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-early-end", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "{response:#}"
    );
    assert!(
        response.to_string().contains("analysis_progress_pending"),
        "{response:#}"
    );
}

#[test]
fn scoped_python_progress_new_begin_waits_for_its_own_end() {
    let (_dir, ctx) = python_progress_fixture(Some("new_begin"));
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-new-begin", "broken.py");
    assert_eq!(
        diagnostic_messages_for(&response, "broken.py"),
        vec!["test diagnostic after change"],
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

#[test]
fn scoped_python_progress_existing_run_certifies_later_opened_files() {
    let (_dir, ctx) = python_progress_fixture(Some("already_running"));
    let response = scoped_diagnostics_inspect(&ctx, "python-progress-existing-run", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

#[test]
fn scoped_python_progress_announced_run_cannot_use_no_run_grace() {
    // Inspect reserves five seconds and gives each phase half the work budget;
    // leave this injected >2s announcement delay room to finish its real run.
    let (_dir, ctx) = python_progress_fixture_with_timeout(Some("announce_delay"), 30_000);
    let response = scoped_diagnostics_inspect(&ctx, "python-announced-run", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

fn python_short_run_fixture() -> (tempfile::TempDir, AppContext) {
    let (dir, ctx) = python_progress_fixture(Some("short_runs"));
    let first = scoped_diagnostics_inspect(&ctx, "python-short-warmup", "broken.py");
    assert_eq!(first["summary"]["diagnostics"]["errors"], 1, "{first:#}");
    (dir, ctx)
}

fn assert_short_run_complete(ctx: &AppContext, file: &str) {
    let response = scoped_diagnostics_inspect(ctx, "python-short-open", file);
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["python"]["authority"][0]["rule"],
        "post_input_publish_no_analysis_run",
        "{response:#}"
    );
}

#[test]
fn scoped_python_progress_excluded_file_without_new_run_settles() {
    let (dir, ctx) = python_short_run_fixture();
    std::fs::write(
        dir.path().join("pyrightconfig.json"),
        r#"{"exclude":["excluded.py"]}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("excluded.py"), "value: int = 'wrong'\n").unwrap();
    assert_short_run_complete(&ctx, "excluded.py");
}

#[test]
fn scoped_python_progress_nonsource_file_without_new_run_settles() {
    let (dir, ctx) = python_short_run_fixture();
    ctx.update_config(|config| {
        config.lsp_servers.push(aft::config::UserServerDef {
            id: "python".into(),
            extensions: vec!["py".into(), "txt".into()],
            binary: String::new(),
            args: Vec::new(),
            root_markers: vec!["pyproject.toml".into()],
            env: Default::default(),
            initialization_options: None,
            disabled: false,
        })
    });
    std::fs::write(dir.path().join("notes.txt"), "plain text\n").unwrap();
    assert_short_run_complete(&ctx, "notes.txt");
}

#[test]
fn scoped_python_progress_unchanged_reopen_without_new_run_settles() {
    let (_dir, ctx) = python_short_run_fixture();
    assert_short_run_complete(&ctx, "broken.py");
}

#[test]
fn scoped_python_progress_second_file_after_idle_without_new_run_settles() {
    let (dir, ctx) = python_progress_fixture(Some("short_runs"));
    std::fs::write(dir.path().join("second.py"), "value: int = 'wrong'\n").unwrap();
    let first = std::fs::canonicalize(dir.path()).unwrap().join("broken.py");
    let config = ctx.config();
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(&first, &config)
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut ended = false;
    loop {
        let drained = ctx.lsp().drain_events();
        ended |= drained.events.iter().any(|event| matches!(event, LspEvent::Notification { method, params: Some(params), .. } if method == "$/progress" && params["value"]["kind"] == "end"));
        if ended && ctx.lsp().has_authoritative_report_for_file(&first) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first analysis never completed"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ctx.lsp().document_is_open_for_test(&first),
        "the first file remains open while the second is opened"
    );
    assert_short_run_complete(&ctx, "second.py");
}

#[test]
fn scoped_python_progress_silent_open_cannot_reuse_cached_report_or_poison_server() {
    let (dir, ctx) = python_progress_fixture(Some("short_no_publish"));
    let first = scoped_diagnostics_inspect(&ctx, "python-silent-warmup", "broken.py");
    assert_eq!(first["summary"]["diagnostics"]["errors"], 1, "{first:#}");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(
        !ctx.lsp()
            .get_diagnostics_for_file(&root.join("broken.py"))
            .is_empty(),
        "the pre-open cache must actually exist"
    );
    let response = scoped_diagnostics_inspect(&ctx, "python-silent-open", "broken.py");
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "the cached pre-open report is not a reply: {response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{response:#}"
    );
    // Keyed by the normalized root the manager uses (no Windows `\\?\`
    // prefix). With the verbatim spelling no client matches, and both checks
    // below pass without looking at a server.
    let key = ServerKey {
        kind: ServerKind::Python,
        root: crate::helpers::canonicalize_like_product(&root),
    };
    assert!(
        ctx.lsp().active_server_keys().contains(&key),
        "the key must name the running Python server: {key:?}"
    );
    assert!(
        !ctx.lsp().server_is_warming(&key),
        "absence of a publication is a file gap, not a permanent producer run"
    );
    assert!(
        ctx.lsp().producer_has_settled(&key),
        "the unanswered file must not leave a fictitious producer run"
    );
}

/// `file` relative to `root`, compared the way the alert state compares them
/// (`DiagnosticIdentity::from_stored` canonicalizes both sides). On Windows a
/// root from `fs::canonicalize` is spelled `\\?\C:\...` while the diagnostics
/// store holds `C:\...`, so a plain `strip_prefix` fails.
fn alert_relative_file(root: &Path, file: &Path) -> PathBuf {
    let canonical_root = fs::canonicalize(root).expect("canonical alert root");
    let canonical_file = fs::canonicalize(file).expect("canonical diagnostic file");
    canonical_file
        .strip_prefix(&canonical_root)
        .unwrap_or_else(|_| panic!("diagnostic file {file:?} is outside alert root {root:?}"))
        .to_path_buf()
}

fn accept_progress_alerts(
    root: &Path,
    state: &mut aft::alert_state::AlertDeltaState,
    renderer: &mut aft::response_finalize::alert_render::AlertEngine,
    snapshots: Vec<aft::alert_state::AcceptedDiagnosticSnapshot>,
) -> Vec<aft::alert_state::AcceptedObservationResult> {
    use aft::alert_state::AcceptedObservationBatch;
    use aft::response_finalize::alert_render::{AlertDiagnostic, AlertObservation};
    let batch =
        AcceptedObservationBatch::from_diagnostic_snapshots("progress-alerts", root, snapshots)
            .unwrap();
    let results = state.accept_batch(&batch).unwrap();
    renderer.observe_authoritative_batch(
        "progress-alerts",
        root,
        batch.observations().iter().map(|observation| {
            AlertObservation::new(
                observation.producer_key.as_str(),
                observation
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == aft::lsp::diagnostics::DiagnosticSeverity::Error)
                    .map(|d| {
                        AlertDiagnostic::error(
                            alert_relative_file(root, &d.file).to_string_lossy(),
                            d.line,
                            d.message.clone(),
                        )
                    })
                    .collect(),
            )
        }),
    );
    results
}

#[test]
fn scoped_python_progress_reanalysis_keeps_warm_counts_and_alert_episode() {
    use aft::alert_state::{
        AcceptedDiagnosticSnapshot, AlertDeltaState, AlertPartitionKey, DiagnosticIdentity,
        ProducerKey,
    };
    use aft::response_finalize::alert_render::AlertEngine;
    let (dir, ctx) = python_progress_fixture(Some("held_runs"));
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let file = root.join("broken.py");
    let release = root.join("analysis-release");
    let before_publish = root.join("before-publish-release");
    std::fs::write(&release, "ready").unwrap();
    std::fs::write(&before_publish, "ready").unwrap();
    ctx.lsp().set_extra_env(
        "AFT_FAKE_LSP_ANALYSIS_RELEASE_FILE",
        release.to_str().unwrap(),
    );
    ctx.lsp().set_extra_env(
        "AFT_FAKE_LSP_BEFORE_ANALYSIS_PUBLISH",
        before_publish.to_str().unwrap(),
    );
    let first = scoped_diagnostics_inspect(&ctx, "progress-alert-warmup", "broken.py");
    assert_eq!(first["summary"]["diagnostics"]["errors"], 1, "{first:#}");
    // The manager keys its client by the normalized root (no Windows `\\?\`
    // prefix). A key with the verbatim spelling finds no client, so
    // `server_is_warming` would report false at once.
    let key = ServerKey {
        kind: ServerKind::Python,
        root: crate::helpers::canonicalize_like_product(&root),
    };
    assert!(
        ctx.lsp().active_server_keys().contains(&key),
        "the key must name the running Python server: {key:?}"
    );
    let diagnostics = ctx
        .lsp()
        .get_diagnostics_for_file(&file)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let identity = DiagnosticIdentity::from_stored(
        &root,
        diagnostics
            .iter()
            .find(|d| d.severity == aft::lsp::diagnostics::DiagnosticSeverity::Error)
            .unwrap(),
    )
    .unwrap();
    let partition =
        AlertPartitionKey::new("progress-alerts", &root, ProducerKey::from_server_key(&key));
    let mut state = AlertDeltaState::default();
    let mut renderer = AlertEngine::default();
    accept_progress_alerts(
        &root,
        &mut state,
        &mut renderer,
        vec![AcceptedDiagnosticSnapshot::new(key.clone(), 0, Vec::new())],
    );
    accept_progress_alerts(
        &root,
        &mut state,
        &mut renderer,
        vec![AcceptedDiagnosticSnapshot::new(key.clone(), 0, diagnostics)],
    );
    let episode = state
        .partition(&partition)
        .unwrap()
        .episode_for(&identity)
        .unwrap();
    assert!(renderer
        .finalize("progress-alerts", &root, "read")
        .is_some());
    let counts = ctx.status_bar_count_values();
    assert_eq!((counts.errors, counts.warnings), (Some(1), Some(1)));

    std::fs::remove_file(&release).unwrap();
    std::fs::remove_file(&before_publish).unwrap();
    ctx.lsp()
        .diagnostics_store_mut_for_test()
        .mark_stale_for_file(&file);
    let config = ctx.config();
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(&file, &config)
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let drained = ctx.lsp().drain_events();
        assert!(drained.accepted_snapshots.is_empty());
        if drained.events.iter().any(|event| matches!(event, LspEvent::Notification { method, .. } if method == "custom/beforeAnalysisPublish")) { break; }
        assert!(
            Instant::now() < deadline,
            "fake never reached the pre-publication hold"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let counts = ctx.status_bar_count_values();
    assert_eq!(
        (counts.errors, counts.warnings),
        (Some(1), Some(1)),
        "a pending producer cannot turn stale/missing storage into a clean zero"
    );
    std::fs::write(&before_publish, "ready").unwrap();
    loop {
        let drained = ctx.lsp().drain_events();
        assert!(
            drained.accepted_snapshots.is_empty(),
            "an uncertified empty report must not clear alerts"
        );
        accept_progress_alerts(&root, &mut state, &mut renderer, drained.accepted_snapshots);
        if drained.events.iter().any(|event| matches!(event, LspEvent::Notification { method, params: Some(params), .. } if method == "$/progress" && params["value"]["kind"] == "begin")) { break; }
        assert!(Instant::now() < deadline, "fake analysis never began");
        thread::sleep(Duration::from_millis(10));
    }
    let counts = ctx.status_bar_count_values();
    assert_eq!(
        (counts.errors, counts.warnings),
        (Some(1), Some(1)),
        "a reanalysis must retain the last certified warm pair"
    );
    assert_eq!(
        state.partition(&partition).unwrap().episode_for(&identity),
        Some(episode)
    );
    assert!(renderer
        .finalize("progress-alerts", &root, "read")
        .is_none());

    std::fs::write(&release, "ready").unwrap();
    let mut accepted = Vec::new();
    loop {
        let drained = ctx.lsp().drain_events();
        accepted.extend(drained.accepted_snapshots);
        if !ctx.lsp().server_is_warming(&key) {
            break;
        }
        assert!(Instant::now() < deadline, "fake analysis never ended");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        accepted.len(),
        1,
        "the completed replacement must reach observation ingestion exactly once"
    );
    let results = accept_progress_alerts(&root, &mut state, &mut renderer, accepted);
    assert!(results
        .iter()
        .all(|r| r.entered.is_empty() && r.closed.is_empty()));
    assert_eq!(
        state.partition(&partition).unwrap().episode_for(&identity),
        Some(episode)
    );
    assert!(
        renderer
            .finalize("progress-alerts", &root, "read")
            .is_none(),
        "unchanged errors must not re-appear as new reminders"
    );
    let counts = ctx.status_bar_count_values();
    assert_eq!((counts.errors, counts.warnings), (Some(1), Some(1)));
}

/// Drain events until the fake reports the `didClose` and then sends the
/// publish that follows it.
fn wait_for_close_reply(ctx: &AppContext) {
    let hang_deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_close = false;
    loop {
        assert!(
            Instant::now() < hang_deadline,
            "fake never answered didClose"
        );
        for event in ctx.lsp().drain_events().events {
            if let LspEvent::Notification { method, .. } = event {
                if method == "custom/documentClosed" {
                    saw_close = true;
                } else if saw_close && method == "textDocument/publishDiagnostics" {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The empty publish that clears a closed document is ignored however late
/// it arrives; there is no time window after which it would be stored.
#[test]
fn scoped_blocking_inspect_ignores_a_late_clearing_publish_after_its_close() {
    let (_temp_dir, root, lib) = single_crate_fixture("sweep-close-late");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_CLOSE_DELAY_MS", "2500");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-close-late", "src");
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );

    wait_for_close_reply(&ctx);
    assert_eq!(
        ctx.lsp().get_diagnostics_for_file(&lib).len(),
        2,
        "a late clearing publish erased the collected diagnostics"
    );
}

/// Only an empty publish after inspect's close is ignored. A publish with
/// diagnostics that arrives right after the close is real news about the file
/// and is stored.
#[test]
fn scoped_blocking_inspect_stores_a_non_empty_publish_after_its_close() {
    let (_temp_dir, root, lib) = single_crate_fixture("sweep-close-news");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CLOSE_PUBLISHES", "1");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-close-news", "src");
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );

    wait_for_close_reply(&ctx);
    let stored = ctx
        .lsp()
        .get_diagnostics_for_file(&lib)
        .into_iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        stored,
        vec!["test diagnostic after change".to_string()],
        "the publish after the close was not stored"
    );
}

/// A scoped inspect closes the documents it opened, and the server answers
/// each close with an empty publish. When the next inspect reopens the file
/// before that reply arrives, the reply still describes the closed document.
/// Storing it made the reopened file read as checked clean before the server
/// had analyzed it again, so the second inspect reported no diagnostics.
#[test]
fn scoped_inspect_reopen_ignores_the_late_reply_to_the_earlier_close() {
    let (_temp_dir, root, _file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    // The fake answers didClose only after the close delay, so the second
    // inspect reopens the file while that reply is still on its way. It then
    // reads the didOpen and publishes the reopened file's report only after
    // the open delay, leaving the late close reply alone in the meantime.
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_CLOSE_DELAY_MS", "300");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_OPEN_PUBLISH_DELAY_MS", "1000");

    let first = scoped_diagnostics_inspect(&ctx, "inspect-reopen-first", "broken.ts");
    let expected = vec![
        "test diagnostic error".to_string(),
        "test diagnostic warning".to_string(),
    ];
    assert_eq!(
        diagnostic_messages_for(&first, "broken.ts"),
        expected,
        "response: {first:#}"
    );

    let second = scoped_diagnostics_inspect(&ctx, "inspect-reopen-second", "broken.ts");
    assert_eq!(
        diagnostic_messages_for(&second, "broken.ts"),
        expected,
        "the reply to the earlier close was read as the reopened file's report: {second:#}"
    );
    assert_eq!(
        second["summary"]["diagnostics"]["errors"], 1,
        "response: {second:#}"
    );
}

#[test]
fn scoped_typescript_sync_empty_then_real_reports_the_semantic_error() {
    let (_temp, root, _file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "empty_then_real");

    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-error", "broken.ts");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

#[test]
fn scoped_typescript_sync_clean_file_is_authoritative() {
    let (_temp, root, file) = external_typescript_fixture();
    std::fs::write(file, "export const clean: number = 1;\n").unwrap();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "clean");

    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-clean", "broken.ts");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 0,
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
        "{response:#}"
    );
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
}

#[test]
fn scoped_typescript_sync_unsupported_is_pending_not_clean() {
    let (_temp, root, _file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "unsupported");

    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-unsupported", "broken.ts");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{response:#}"
    );
    assert_eq!(uncovered_files(&response).len(), 1, "{response:#}");
    assert!(
        response.to_string().contains("typescript_sync_unsupported"),
        "{response:#}"
    );
}

#[test]
fn scoped_typescript_sync_collects_every_category_without_changing_warm_counts() {
    let (_temp, root, file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "categories");
    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-categories", "broken.ts");
    let diagnostics = &response["summary"]["diagnostics"];
    assert_eq!(diagnostics["errors"], 1, "{response:#}");
    assert_eq!(diagnostics["warnings"], 1, "{response:#}");
    assert_eq!(diagnostics["hints"], 1, "{response:#}");
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    assert!(
        ctx.lsp().get_diagnostics_for_file(&file).is_empty(),
        "request-local answers must not replace the empty push store"
    );
}

#[test]
fn scoped_typescript_sync_malformed_is_pending_not_clean() {
    let (_temp, root, _file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "malformed");
    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-malformed", "broken.ts");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{response:#}"
    );
    assert_eq!(uncovered_files(&response).len(), 1, "{response:#}");
    assert!(
        response
            .to_string()
            .contains("typescript_sync_invalid_response"),
        "{response:#}"
    );
}

#[test]
fn scoped_typescript_sync_timeout_is_pending_not_clean() {
    let (_temp, root, _file) = external_typescript_fixture();
    let ctx = configured_context_with_diagnostics_timeout(&root, 3000);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "timeout");
    let response = scoped_diagnostics_inspect(&ctx, "ts-sync-timeout", "broken.ts");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{response:#}"
    );
    assert_eq!(uncovered_files(&response).len(), 1, "{response:#}");
    assert!(
        response.to_string().contains("typescript_sync_timeout"),
        "{response:#}"
    );
}

/// A server that answers `textDocument/diagnostic` is asked directly. The
/// fake pushes nothing useful here (an empty list on open), so only the pull
/// can produce its diagnostic.
#[test]
fn scoped_blocking_inspect_pulls_diagnostics_from_a_pull_server() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-pull");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-pull", "src/lib.rs");

    assert_eq!(
        diagnostic_messages_for(&response, "src/lib.rs"),
        vec!["test pull diagnostic".to_string()],
        "response: {response:#}"
    );
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );
}

/// rust-analyzer publishes compiler errors only when its `cargo check` run
/// finishes. The fake's check ends 1.5s after the file is opened and adds one
/// compiler error; inspect must wait for it instead of answering with the
/// analyzer's own diagnostics alone.
#[test]
fn scoped_blocking_inspect_waits_for_rust_analyzer_cargo_check() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-flycheck");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_FLYCHECK", "1500");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-flycheck", "src");

    let messages = diagnostic_messages_for(&response, "src/lib.rs");
    assert!(
        messages
            .iter()
            .any(|message| message == "flycheck diagnostic"),
        "cargo check result missing: {response:#}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message == "test diagnostic error"),
        "response: {response:#}"
    );
    assert!(
        uncovered_files(&response).is_empty(),
        "response: {response:#}"
    );
}

/// rust-analyzer that answers pulls returns its own analysis through the pull
/// and pushes `cargo check` results separately, for the same file. Both
/// reach one store entry, so the later one would hide the other; inspect
/// must report the pulled diagnostic and the compiler error together.
#[test]
fn scoped_blocking_inspect_keeps_cargo_check_results_beside_pulled_diagnostics() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-pull-flycheck");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_FLYCHECK", "1500");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-pull-flycheck", "src");

    let messages = diagnostic_messages_for(&response, "src/lib.rs");
    for expected in ["test pull diagnostic", "flycheck diagnostic"] {
        assert!(
            messages.iter().any(|message| message == expected),
            "{expected} missing: {response:#}"
        );
    }
}

/// Pulls are sent several files at a time and then waited for in turn; each
/// wait must end at the request budget, not restart its own timeout. With a
/// server that never answers within the budget, a scope of many files still
/// returns close to the configured diagnostics deadline.
#[test]
fn scoped_blocking_inspect_pipelined_pulls_stay_within_the_request_budget() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-pull-budget");
    for index in 0..40 {
        write_file(&root, &format!("src/m{index:02}.rs"), "pub fn g() {}\n");
    }
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_DELAY_MS", "60000");

    let started = std::time::Instant::now();
    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-pull-budget", "src");
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(25),
        "a 10s inspect took {elapsed:?}: {response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "unanswered pulls are gaps: {response:#}"
    );
}

/// A compiler push after a cancelled pull does not answer rust-analyzer's
/// native analysis request. Retry the pull and retain both sources.
#[test]
fn scoped_blocking_inspect_retries_cancelled_rust_pull_after_compiler_push() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-cancelled-pull");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL_CANCEL", "once");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-cancelled-pull", "src/lib.rs");
    let reported = diagnostic_sources_for(&response, "src/lib.rs");
    for expected in [
        ("fake-lsp".to_string(), "test pull diagnostic".to_string()),
        ("rustc".to_string(), "fake compile error".to_string()),
    ] {
        assert!(
            reported.contains(&expected),
            "{expected:?} missing: {response:#}"
        );
    }
    assert!(uncovered_files(&response).is_empty(), "{response:#}");
    assert_eq!(response["inspect_terminal"], "fresh", "{response:#}");
}

/// When the retry is cancelled too, compiler results alone cannot certify
/// the file's diagnostics. Keep the failed native analysis visible as a gap.
#[test]
fn scoped_blocking_inspect_marks_cancelled_rust_pull_unknown_despite_compiler_push() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-cancelled-pull-unknown");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_CANCEL", "always");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-cancelled-pull-unknown", "src/lib.rs");
    assert_eq!(
        response["summary"]["diagnostics"]["complete"], false,
        "{response:#}"
    );
    let gaps = uncovered_files(&response);
    assert!(
        gaps.iter().any(|(file, reason)| {
            file == "src/lib.rs" && reason.contains("server cancelled the request")
        }),
        "cancelled native analysis must be unknown: {response:#}"
    );
}

/// A successful pull before the current check can still describe the native
/// analysis from before a watched-file change. Pull only after that check.
#[test]
fn scoped_blocking_inspect_waits_to_pull_native_rust_diagnostics_until_current_check_settles() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-native-after-check");
    write_file(
        &root,
        "src/lib.rs",
        "// fake_compile_error\npub fn f() {}\n",
    );
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "600");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_WAIT_FOR_CHECK", "1");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-native-after-check", "src/lib.rs");
    let messages = diagnostic_messages_for(&response, "src/lib.rs");
    for expected in ["test pull diagnostic", "fake compile error"] {
        assert!(
            messages.iter().any(|message| message == expected),
            "{expected} missing: {response:#}"
        );
    }
    assert_eq!(response["inspect_terminal"], "fresh", "{response:#}");
}

/// A scope with more files than one inspect opens reports how many it
/// examined, and names the rest with that cause instead of opening them.
#[test]
fn scoped_blocking_inspect_caps_the_files_it_opens() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-cap");
    for index in 0..1000 {
        write_file(&root, &format!("src/m{index:04}.rs"), "pub fn g() {}\n");
    }
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-cap", "src");

    assert_eq!(
        response["summary"]["diagnostics"]["coverage"],
        json!({"files": 1001, "examined": 1000, "authoritative": 1000, "not_examined": 1, "file_cap": 1000}),
        "response: {response:#}"
    );
    let uncovered = uncovered_files(&response);
    assert_eq!(uncovered.len(), 1, "response: {response:#}");
    assert!(
        uncovered[0].1.starts_with("not examined:"),
        "response: {response:#}"
    );
    let text = response["text"].as_str().expect("rendered text");
    // The cap's unknown file is explained once in the header; exact coverage
    // remains structured rather than repeating incompleteness in a body line.
    assert!(
        text.starts_with("PARTIAL — diagnostics unknown: rust-analyzer @ .: not examined:")
            && text.contains("(1 file); retry aft_inspect."),
        "{text}"
    );
}

/// When `cargo check` is still running at the end of the budget, the Rust
/// files are named as incomplete with that cause instead of being certified
/// from reports that lack the compiler's errors.
#[test]
fn scoped_blocking_inspect_names_an_unfinished_cargo_check() {
    let (_temp_dir, root, _lib) = single_crate_fixture("sweep-flycheck-slow");
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_FLYCHECK", "never");

    let response = scoped_diagnostics_inspect(&ctx, "inspect-sweep-flycheck-slow", "src");

    assert_eq!(response["complete"], false, "response: {response:#}");
    let uncovered = uncovered_files(&response);
    assert_eq!(uncovered.len(), 1, "response: {response:#}");
    assert_eq!(uncovered[0].0, "src/lib.rs");
    assert!(
        uncovered[0].1.starts_with("still checking:"),
        "response: {response:#}"
    );
    let text = response["text"].as_str().expect("rendered text");
    assert!(
        text.starts_with("PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo check still running (1 file); retry aft_inspect."),
        "{text}"
    );
}

/// A TypeScript server started for an earlier request keeps its runtime note
/// (which SDK it runs). An inspect scoped to Rust files must not repeat it.
/// The fake is installed as the project's `typescript-language-server` so the
/// real SDK resolution runs and records a note, which a binary override skips.
#[cfg(unix)]
#[test]
fn scoped_rust_inspect_omits_the_note_of_a_typescript_server_started_earlier() {
    let (_temp_dir, root, _lib) = single_crate_fixture("notes-scope");
    write_file(&root, "package.json", "{\"name\":\"notes-scope\"}\n");
    write_file(&root, "web/index.ts", "export const x = 1;\n");
    let bin_dir = root.join("node_modules/.bin");
    fs::create_dir_all(&bin_dir).expect("node_modules/.bin");
    std::os::unix::fs::symlink(
        fake_server_path(),
        bin_dir.join("typescript-language-server"),
    )
    .expect("link fake typescript-language-server");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);

    let typescript = scoped_diagnostics_inspect(&ctx, "inspect-notes-ts", "web/index.ts");
    assert!(
        typescript.to_string().contains("TypeScript: "),
        "precondition: the TypeScript scope reports its runtime note: {typescript:#}"
    );

    let rust = scoped_diagnostics_inspect(&ctx, "inspect-notes-rust", "src/lib.rs");
    assert!(
        active_server_kinds(&ctx).contains(&ServerKind::TypeScript),
        "precondition: the TypeScript server is still running"
    );
    assert!(
        !rust.to_string().contains("TypeScript"),
        "a Rust-only scope must not carry a TypeScript note: {rust:#}"
    );
}

/// With no file TypeScript handles anywhere in the project, `package.json`
/// alone never starts it: a scoped Rust inspect starts only rust-analyzer, and
/// an unscoped inspect names TypeScript as not applicable instead of starting
/// it, failing it, or saying nothing.
#[test]
fn package_json_without_typescript_files_never_starts_typescript() {
    let (_temp_dir, root) = wrangler_rust_fixture();
    // Each inspect gets its own context so the live-server roster after it
    // reflects that call alone.
    let ctx = tier2_ready_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    let unscoped = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-unscoped-wrangler",
            "command": "inspect",
        }),
    );
    assert_eq!(unscoped["success"], true, "response: {unscoped:#}");
    assert_eq!(
        active_server_kinds(&ctx),
        vec![ServerKind::Rust],
        "response: {unscoped:#}"
    );
    assert_eq!(
        not_applicable_producers(&unscoped),
        vec!["typescript".to_string()],
        "response: {unscoped:#}"
    );
    assert!(
        failed_producers(&unscoped).is_empty(),
        "not applicable is not a failure: {unscoped:#}"
    );
    let text = unscoped["text"].as_str().expect("inspect text");
    assert!(
        text.contains("diagnostics: producer typescript not applicable (package.json found"),
        "text: {text}"
    );

    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    let scoped = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-scoped-rust-wrangler",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": "src/lib.rs",
        }),
    );
    assert_eq!(
        active_server_kinds(&ctx),
        vec![ServerKind::Rust],
        "response: {scoped:#}"
    );
}

/// Positive control for the two tests above: once the project has a
/// TypeScript file, TypeScript is selected exactly as before, both for an
/// unscoped inspect and for a scope naming that file.
#[test]
fn a_typescript_file_still_starts_typescript() {
    let (_temp_dir, root) = wrangler_rust_fixture();
    write_file(&root, "worker/index.ts", "export const answer = 42;\n");
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);

    let scoped = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-scoped-typescript",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": "worker/index.ts",
        }),
    );
    assert_eq!(
        active_server_kinds(&ctx),
        vec![ServerKind::TypeScript],
        "response: {scoped:#}"
    );

    let ctx = tier2_ready_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    let unscoped = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-unscoped-typescript",
            "command": "inspect",
        }),
    );
    assert_eq!(unscoped["success"], true, "response: {unscoped:#}");
    let mut kinds = active_server_kinds(&ctx);
    kinds.sort_by(|left, right| left.id_str().cmp(right.id_str()));
    assert_eq!(
        kinds,
        vec![ServerKind::Rust, ServerKind::TypeScript],
        "response: {unscoped:#}"
    );
    assert!(
        not_applicable_producers(&unscoped).is_empty(),
        "response: {unscoped:#}"
    );
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
        scoped["summary"]["diagnostics"].get("complete").is_none(),
        "covered diagnostics must be complete even with Tier-2 gaps: {scoped:#}"
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

fn diagnostic_path(file: &Path) -> String {
    file.to_string_lossy().replace('\\', "/")
}

fn external_typescript_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, root) = fixture_project();
    write_file(&root, "package.json", "{\"private\":true}\n");
    write_file(&root, "tsconfig.json", "{\"include\":[\"*.ts\"]}\n");
    let file = write_file(
        &root,
        "broken.ts",
        "export const broken: number = \"not a number\";\n",
    );
    (
        temp,
        crate::helpers::canonicalize_like_product(&root),
        crate::helpers::canonicalize_like_product(&file),
    )
}

fn publish_external_typescript_report(
    ctx: &AppContext,
    root: &Path,
    file: &Path,
    diagnostics: Vec<StoredDiagnostic>,
) {
    ctx.lsp().diagnostics_store_mut_for_test().publish(
        ServerKey {
            kind: ServerKind::TypeScript,
            root: root.to_path_buf(),
        },
        crate::helpers::canonicalize_like_product(file),
        diagnostics,
    );
}

fn external_error(file: &Path) -> StoredDiagnostic {
    StoredDiagnostic {
        file: crate::helpers::canonicalize_like_product(file),
        line: 1,
        column: 1,
        end_line: 1,
        end_column: 2,
        severity: DiagnosticSeverity::Error,
        message: "not assignable to number".into(),
        code: Some("2322".into()),
        source: Some("typescript".into()),
    }
}

#[test]
fn scoped_diagnostics_external_directory_without_workspace_marker_counts_unchecked_files() {
    let (_temp, root) = fixture_project();
    let (_external, external_root) = fixture_project();
    let files = [
        write_file(&external_root, "first.ts", "export const first = 1;\n"),
        write_file(&external_root, "second.ts", "export const second = 2;\n"),
    ];
    let ctx = configured_context(&root);
    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-rootless-directory",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": external_root,
            "topK": 20,
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["scope_files"], 2, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
    for file in files {
        let file = crate::helpers::canonicalize_like_product(&file);
        assert!(
            response["gaps"].as_array().unwrap().iter().any(|gap| {
                gap["kind"] == "uncovered_file"
                    && gap["file"] == diagnostic_path(&file)
                    && gap["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("outside_project_root_unverified"))
                    && gap["cause"]["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("no workspace root marker found"))
            }),
            "{response:#}"
        );
    }
}

#[test]
fn scoped_diagnostics_external_directory_counts_reported_files_and_keeps_rows() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, broken) = external_typescript_fixture();
    let clean = write_file(&external_root, "clean.ts", "export const clean = 1;\n");
    let ctx = configured_context(&root);
    publish_external_typescript_report(
        &ctx,
        &external_root,
        &broken,
        vec![external_error(&broken)],
    );
    publish_external_typescript_report(&ctx, &external_root, &clean, Vec::new());

    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-directory-reports",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": external_root,
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["scope_files"], 2, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(response["complete"], true, "{response:#}");
    assert!(
        response["details"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == diagnostic_path(&broken)
                && row["message"] == "not assignable to number"),
        "external directory report was dropped: {response:#}"
    );
}

#[test]
fn scoped_diagnostics_external_directory_names_unreported_file_gap() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, unreported) = external_typescript_fixture();
    let reported = write_file(
        &external_root,
        "reported.ts",
        "export const reported = 1;\n",
    );
    let ctx = configured_context(&root);
    publish_external_typescript_report(&ctx, &external_root, &reported, Vec::new());

    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-directory-unreported",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": external_root,
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["scope_files"], 2, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
    assert!(
        response["gaps"].as_array().unwrap().iter().any(|gap| {
            gap["kind"] == "uncovered_file"
                && gap["file"] == diagnostic_path(&unreported)
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("outside_project_root_unverified"))
        }),
        "external unreported file should be a named gap: {response:#}"
    );
}

#[test]
fn scoped_diagnostics_external_directory_and_file_are_counted_once() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, broken) = external_typescript_fixture();
    let clean = write_file(&external_root, "clean.ts", "export const clean = 1;\n");
    let ctx = configured_context(&root);
    publish_external_typescript_report(
        &ctx,
        &external_root,
        &broken,
        vec![external_error(&broken)],
    );
    publish_external_typescript_report(&ctx, &external_root, &clean, Vec::new());

    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-directory-overlap",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": [external_root, clean],
            "topK": 20,
        }),
    );

    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["scope_files"], 2, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(response["complete"], true, "{response:#}");
}

#[test]
fn scoped_diagnostics_external_biome_workspace_counts_json_with_authoritative_reports() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, broken) = external_typescript_fixture();
    write_file(&external_root, "biome.json", "{}\n");
    let clean = write_file(&external_root, "clean.ts", "export const clean = 1;\n");
    let ctx = configured_context(&root);
    publish_external_typescript_report(
        &ctx,
        &external_root,
        &broken,
        vec![external_error(&broken)],
    );
    publish_external_typescript_report(&ctx, &external_root, &clean, Vec::new());
    let inspect_workspace = |id: &str| {
        inspect_outside(
            &ctx,
            json!({
                "id": id, "command": "inspect", "sections": ["diagnostics"], "scope": external_root,
            }),
        )
    };
    let unchecked = inspect_workspace("inspect-outside-biome-unchecked");
    assert_eq!(unchecked["success"], true, "{unchecked:#}");
    assert_eq!(unchecked["scope_files"], 5, "{unchecked:#}");
    assert_eq!(unchecked["complete"], false, "{unchecked:#}");
    assert!(
        unchecked["summary"]["diagnostics"]["errors"].is_null(),
        "{unchecked:#}"
    );
    for name in ["package.json", "tsconfig.json", "biome.json"] {
        assert!(
            unchecked["gaps"].as_array().unwrap().iter().any(|gap| {
                gap["kind"] == "uncovered_file"
                    && gap["file"] == diagnostic_path(&external_root.join(name))
            }),
            "{unchecked:#}"
        );
    }
    for name in ["package.json", "tsconfig.json", "biome.json"] {
        ctx.lsp().diagnostics_store_mut_for_test().publish(
            ServerKey {
                kind: ServerKind::Biome,
                root: external_root.clone(),
            },
            crate::helpers::canonicalize_like_product(&external_root.join(name)),
            Vec::new(),
        );
    }
    let response = inspect_workspace("inspect-outside-biome-reported");
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["scope_files"], 5, "{response:#}");
    assert_eq!(response["complete"], true, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["biome"]["errors"], 0,
        "{response:#}"
    );
    assert!(response.get("gaps").is_none(), "{response:#}");
}

#[test]
fn scoped_diagnostics_outside_project_root_report_error_or_named_gap() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);

    let response =
        scoped_diagnostics_inspect_outside(&ctx, "inspect-outside-error", file.to_str().unwrap());
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(response["scope_files"], 1, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
        "{response:#}"
    );
    assert!(
        response["details"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["file"] == diagnostic_path(&file)),
        "{response:#}"
    );
    assert!(
        response["text"]
            .as_str()
            .unwrap()
            .contains("scope: 1 root, 1 file"),
        "{response:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == ServerKind::TypeScript && key.root == external_root),
        "external workspace must own its server"
    );
}

#[test]
fn scoped_diagnostics_outside_project_root_unreported_is_named_gap() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    let reported = write_file(&external_root, "reported.ts", "export const ok = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    open_with_lsp(&ctx, &reported, "export const ok = 1;\n");
    // Read the warm store without opening the scoped file: the server is live
    // for this workspace but has never checked this particular file.
    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-unreported", "command": "inspect",
            "sections": ["diagnostics"], "scope": file,
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["scope_files"], 1, "{response:#}");
    let gap = response["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gap| gap["kind"] == "uncovered_file" && gap["file"] == diagnostic_path(&file))
        .unwrap_or_else(|| panic!("missing outside gap: {response:#}"));
    assert!(
        gap["reason"]
            .as_str()
            .unwrap()
            .contains("outside_project_root_unverified"),
        "{response:#}"
    );
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
}

#[test]
fn scoped_diagnostics_mixed_inside_and_outside_project_root() {
    let (_temp, root, inside) = external_typescript_fixture();
    let (_external, external_root, outside) = external_typescript_fixture();
    let unreported = write_file(&external_root, "unreported.ts", "export const other = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    let response = inspect_tool_call_outside(
        &ctx,
        json!({
            "id": "inspect-mixed-errors", "command": "inspect", "sections": ["diagnostics"],
            "scope": [inside, outside],
        }),
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 2,
        "{response:#}"
    );
    assert_eq!(response["scope_files"], 2, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 2,
        "{response:#}"
    );
    let rows = response["details"]["diagnostics"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| row["file"] == "broken.ts"),
        "{response:#}"
    );
    assert!(
        rows.iter()
            .any(|row| row["file"] == diagnostic_path(&outside)),
        "{response:#}"
    );

    open_with_lsp(
        &ctx,
        &inside,
        "export const broken: number = \"not a number\";\n",
    );
    let partial = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-mixed-gap", "command": "inspect", "sections": ["diagnostics"],
            "scope": [inside, unreported],
        }),
    );
    assert_eq!(partial["complete"], false, "{partial:#}");
    assert_eq!(partial["scope_files"], 2, "{partial:#}");
    assert!(
        partial["details"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == "broken.ts"),
        "{partial:#}"
    );
    assert!(
        partial["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == diagnostic_path(&unreported)
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("outside_project_root_unverified"))),
        "{partial:#}"
    );
}

#[test]
fn scoped_diagnostics_outside_project_root_without_producer_is_named_gap() {
    let (_temp, root) = fixture_project();
    let (_external, external_root) = fixture_project();
    let file = write_file(&external_root, "unknown.aft-unknown", "not checked\n");
    let file = crate::helpers::canonicalize_like_product(&file);
    let ctx = configured_context(&root);
    let response = scoped_diagnostics_inspect_outside(
        &ctx,
        "inspect-outside-no-producer",
        file.to_str().unwrap(),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["scope_files"], 1, "{response:#}");
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
    assert!(
        response["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == diagnostic_path(&file)
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("outside_project_root_unverified")
                        && reason.contains("no LSP producer"))),
        "{response:#}"
    );
}

#[test]
fn scoped_diagnostics_outside_project_root_excluded_by_tsconfig_is_named_gap() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    write_file(
        &external_root,
        "tsconfig.json",
        "{\"include\":[\"included.ts\"]}\n",
    );
    write_file(&external_root, "included.ts", "export const ok = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    let response = inspect_outside(
        &ctx,
        json!({
            "id": "inspect-outside-excluded", "command": "inspect",
            "sections": ["diagnostics"], "scope": file,
        }),
    );
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["scope_files"], 1, "{response:#}");
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
    assert!(
        response["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == diagnostic_path(&file)
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("outside_project_root_unverified")
                        && reason.contains("tsconfig"))),
        "{response:#}"
    );
}

#[test]
fn scoped_typescript_sync_excluded_external_file_is_not_requested() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, excluded) = external_typescript_fixture();
    write_file(
        &external_root,
        "tsconfig.json",
        "{\"include\":[\"included.ts\"]}\n",
    );
    let included = write_file(&external_root, "included.ts", "export const ok = 1;\n");
    let log = root.join("typescript-sync-requests.jsonl");
    let ctx = configured_context(&root);
    configure_fake_rust_and_typescript_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC", "categories");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_TYPESCRIPT_SYNC_LOG", log.to_str().unwrap());
    // The external root is outside the project, so the inspect is dispatched
    // the way an OpenCode host's request is; without a host route the engine
    // refuses outside paths before TypeScript is ever involved.
    let response =
        scoped_diagnostics_inspect_outside(&ctx, "ts-sync-excluded", excluded.to_str().unwrap());
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["scope_files"], 1, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{response:#}"
    );
    assert!(
        diagnostic_messages_for(&response, &diagnostic_path(&excluded)).is_empty(),
        "{response:#}"
    );
    assert!(
        response["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["file"] == diagnostic_path(&excluded)
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("tsconfig"))),
        "{response:#}"
    );
    assert!(
        !log.exists(),
        "an excluded file must not trigger synchronous requests"
    );

    // An included file exercises the same logger and successful server replies,
    // so an empty excluded-file log cannot pass because logging was disconnected.
    let included_response =
        scoped_diagnostics_inspect_outside(&ctx, "ts-sync-included", included.to_str().unwrap());
    assert_eq!(
        included_response["summary"]["diagnostics"]["errors"], 1,
        "{included_response:#}"
    );
    let requests = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|request| request["arguments"][1]["file"] == file_uri(&included)));
}

/// Serve TypeScript under `package_root` the way TypeScript 7 is served: a
/// `node_modules/typescript` at version 7 selects the native compiler's own
/// server (`tsc --lsp`), here the fake. That server only answers pull
/// requests (it publishes nothing for an opened source file) and reports one
/// error for each line of the text it was last sent that contains `BROKEN`.
/// These tests disable the in-process file watcher, so no watcher event marks
/// a stored report stale (outside the project none would arrive anyway).
/// Call before `configured_context`, so the installation is seen.
fn install_content_pull_native_typescript(package_root: &Path) -> ServerKind {
    let package_dir = package_root.join("node_modules").join("typescript");
    write_file(
        &package_dir,
        "package.json",
        "{\"name\":\"typescript\",\"version\":\"7.0.0\"}\n",
    );
    ServerKind::TypeScriptNative(Arc::from(
        crate::helpers::canonicalize_like_product(&package_dir).as_path(),
    ))
}

fn configure_content_pull_typescript_lsp(ctx: &AppContext, native: ServerKind) {
    ctx.lsp().override_binary(native, fake_server_path());
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_FROM_CONTENT", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_SERVER_NAME", "typescript-go");
}

const CONTENT_PULL_ERROR: &str = "BROKEN marker in synced text";

/// `edit_match` dispatched as an OpenCode host's request is, so a file
/// outside the project can be edited.
fn edit_match_as_opencode(ctx: &AppContext, id: &str, file: &Path, from: &str, to: &str) {
    let response = aft::access_policy::with_native_route(
        request(json!({
            "id": id, "command": "edit_match",
            "file": file.display().to_string(), "match": from, "replacement": to,
        })),
        ctx,
        |req, ctx| aft::commands::edit_match::handle_edit_match(&req, ctx),
    );
    assert!(response.success, "{response:?}");
}

/// Fail when `response` certifies diagnostics for `file` that do not describe
/// its current text. A stale result marked incomplete is acceptable; one
/// counted as authoritative is not.
fn assert_not_certified_stale(response: &Value, file: &Path, expected_errors: u64) {
    let messages = diagnostic_messages_for(response, &diagnostic_path(file));
    let reported = messages
        .iter()
        .filter(|message| message.as_str() == CONTENT_PULL_ERROR)
        .count() as u64;
    if response["complete"] == false {
        assert!(
            uncovered_files(response)
                .iter()
                .any(|(gap, _)| gap == &diagnostic_path(file)),
            "an incomplete result must name the file it could not certify: {response:#}"
        );
        return;
    }
    assert_eq!(
        reported, expected_errors,
        "diagnostics certified as authoritative must describe the file's current text: {response:#}"
    );
}

/// A scoped inspect of a file outside the project pulled its
/// diagnostics (and closed the document again); an AFT edit then fixed the
/// error and reopened the document in the server. The pulled report stays in
/// the store as current, and the next inspect reads an already-open document's
/// stored report instead of asking again, so it certifies the fixed error as
/// authoritative.
#[test]
fn scoped_inspect_outside_project_drops_an_error_fixed_by_an_aft_edit() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    fs::write(&file, "export const n: number = BROKEN;\n").unwrap();
    let native = install_content_pull_native_typescript(&external_root);
    let ctx = configured_context(&root);
    configure_content_pull_typescript_lsp(&ctx, native.clone());

    let before =
        scoped_diagnostics_inspect_outside(&ctx, "fixed-outside-before", file.to_str().unwrap());
    assert_eq!(before["complete"], true, "{before:#}");
    assert_eq!(
        diagnostic_messages_for(&before, &diagnostic_path(&file)),
        vec![CONTENT_PULL_ERROR.to_string()],
        "control: the first inspect reports the error: {before:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == native),
        "control: the native TypeScript server analyzed the file"
    );

    edit_match_as_opencode(&ctx, "fixed-outside-edit", &file, "BROKEN", "5");
    let after =
        scoped_diagnostics_inspect_outside(&ctx, "fixed-outside-after", file.to_str().unwrap());
    assert_not_certified_stale(&after, &file, 0);
}

/// After the AFT edit left the document open, the file is changed again by a
/// plain disk write (as a shell append would), which nothing reports to the
/// server, so its analysis still describes the earlier text.
#[test]
fn scoped_inspect_outside_project_sees_a_disk_write_after_an_aft_edit() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    fs::write(&file, "export const n: number = 5;\n").unwrap();
    let native = install_content_pull_native_typescript(&external_root);
    let ctx = configured_context(&root);
    configure_content_pull_typescript_lsp(&ctx, native.clone());

    let before =
        scoped_diagnostics_inspect_outside(&ctx, "append-outside-before", file.to_str().unwrap());
    assert_eq!(before["complete"], true, "{before:#}");
    assert!(
        diagnostic_messages_for(&before, &diagnostic_path(&file)).is_empty(),
        "control: the clean file is reported clean: {before:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == native),
        "control: the native TypeScript server analyzed the file"
    );
    edit_match_as_opencode(&ctx, "append-outside-edit", &file, "= 5", "= 6");
    let edited =
        scoped_diagnostics_inspect_outside(&ctx, "append-outside-edited", file.to_str().unwrap());
    assert_not_certified_stale(&edited, &file, 0);

    let mut appended = fs::read_to_string(&file).unwrap();
    appended.push_str("export const m: number = BROKEN;\n");
    fs::write(&file, appended).unwrap();
    let after =
        scoped_diagnostics_inspect_outside(&ctx, "append-outside-after", file.to_str().unwrap());
    assert_not_certified_stale(&after, &file, 1);
}

/// The same sequence as the out-of-project edit, inside the project. In a
/// running session the project watcher sees AFT's own write and marks the old
/// analysis stale, which hides the defect; without that event (these tests run
/// no watcher) the old analysis is certified exactly as outside the project.
#[test]
fn scoped_inspect_inside_project_drops_an_error_fixed_by_an_aft_edit_without_a_watcher_event() {
    let (_temp, root, _broken) = external_typescript_fixture();
    let file = crate::helpers::canonicalize_like_product(&write_file(
        &root,
        "a.ts",
        "export const n: number = BROKEN;\n",
    ));
    let native = install_content_pull_native_typescript(&root);
    let ctx = configured_context(&root);
    configure_content_pull_typescript_lsp(&ctx, native.clone());

    let before = scoped_diagnostics_inspect(&ctx, "fixed-inside-before", "a.ts");
    assert_eq!(before["complete"], true, "{before:#}");
    assert_eq!(
        diagnostic_messages_for(&before, "a.ts"),
        vec![CONTENT_PULL_ERROR.to_string()],
        "control: the first inspect reports the error: {before:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == native),
        "control: the native TypeScript server analyzed the file"
    );

    let fix = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "fixed-inside-edit", "command": "edit_match",
            "file": file.display().to_string(), "match": "BROKEN", "replacement": "5",
        })),
        &ctx,
    );
    assert!(fix.success, "{fix:?}");
    let after = scoped_diagnostics_inspect(&ctx, "fixed-inside-after", "a.ts");
    if after["complete"] == true {
        assert!(
            diagnostic_messages_for(&after, "a.ts").is_empty(),
            "diagnostics certified as authoritative must describe the file's current text: {after:#}"
        );
    }
}

/// After an AFT edit left the document open, the file changed on disk to
/// bytes that are not UTF-8 text, so AFT cannot send the server the current
/// text. The server's analysis describes the older text, so the file must be
/// reported unknown, with the reason, never certified.
#[test]
fn scoped_inspect_reports_a_file_unknown_when_its_changed_text_cannot_be_sent() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    fs::write(&file, "export const n: number = BROKEN;\n").unwrap();
    let native = install_content_pull_native_typescript(&external_root);
    let ctx = configured_context(&root);
    configure_content_pull_typescript_lsp(&ctx, native.clone());

    let before =
        scoped_diagnostics_inspect_outside(&ctx, "unsendable-before", file.to_str().unwrap());
    assert_eq!(
        diagnostic_messages_for(&before, &diagnostic_path(&file)),
        vec![CONTENT_PULL_ERROR.to_string()],
        "control: the first inspect reports the error: {before:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == native),
        "control: the native TypeScript server analyzed the file"
    );
    edit_match_as_opencode(&ctx, "unsendable-edit", &file, "BROKEN", "5");
    let edited =
        scoped_diagnostics_inspect_outside(&ctx, "unsendable-edited", file.to_str().unwrap());
    assert_eq!(edited["complete"], true, "control: {edited:#}");
    assert!(
        diagnostic_messages_for(&edited, &diagnostic_path(&file)).is_empty(),
        "control: the fixed file is reported clean: {edited:#}"
    );

    fs::write(&file, b"export const n: number = BROKEN;\n// \xff\xfe\n").unwrap();
    let after =
        scoped_diagnostics_inspect_outside(&ctx, "unsendable-after", file.to_str().unwrap());
    assert_eq!(after["complete"], false, "{after:#}");
    let gaps = uncovered_files(&after);
    // The full agent-visible reason, pinned word for word: the wording was
    // approved as written, so any change to it must show up here.
    assert!(
        gaps.iter().any(|(gap, reason)| gap == &diagnostic_path(&file)
            && reason
                == "changed on disk since the server last analyzed it, and AFT could not send it the current text; retry aft_inspect"),
        "the file must be named unknown with the reason: {after:#}"
    );
    assert!(
        diagnostic_messages_for(&after, &diagnostic_path(&file)).is_empty(),
        "no analysis of the older text may be reported: {after:#}"
    );
}

/// A server that answers pulls and clears a closed document's diagnostics
/// with an empty, unversioned `publishDiagnostics` sent the clear late: after
/// an AFT edit had already reopened the document. That push says nothing
/// about the open text (which still has the error), so a scoped inspect must
/// ask the server again rather than certify the file clean from it.
#[test]
fn scoped_inspect_asks_a_pull_server_again_after_a_late_clearing_push() {
    let (_temp, root) = fixture_project();
    let (_external, external_root, file) = external_typescript_fixture();
    fs::write(
        &file,
        "export const n: number = BROKEN;\nexport const other = 1;\n",
    )
    .unwrap();
    let native = install_content_pull_native_typescript(&external_root);
    let ctx = configured_context(&root);
    configure_content_pull_typescript_lsp(&ctx, native.clone());
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_CONTENT_CLEARS_ON_CLOSE", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_CLOSE_DELAY_MS", "500");

    let before =
        scoped_diagnostics_inspect_outside(&ctx, "late-clear-before", file.to_str().unwrap());
    assert_eq!(
        diagnostic_messages_for(&before, &diagnostic_path(&file)),
        vec![CONTENT_PULL_ERROR.to_string()],
        "control: the first inspect reports the error: {before:#}"
    );
    assert!(
        ctx.lsp()
            .active_server_keys()
            .iter()
            .any(|key| key.kind == native),
        "control: the native TypeScript server analyzed the file"
    );
    // An edit that keeps the error reopens the document before the server's
    // delayed reply to the inspect's close arrives.
    edit_match_as_opencode(&ctx, "late-clear-edit", &file, "other = 1", "other = 2");

    // Control: the late clearing push reached the store as a current,
    // pushed (not pulled), empty report for the file.
    let canonical = crate::helpers::canonicalize_like_product(&file);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut lsp = ctx.lsp();
        lsp.drain_events();
        let landed = lsp
            .diagnostics_store_for_test()
            .entries_for_file(&canonical)
            .into_iter()
            .any(|(key, entry)| {
                key.kind == native
                    && !entry.stale
                    && !entry.from_document_pull
                    && entry.diagnostics.is_empty()
            });
        drop(lsp);
        if landed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "control: the late clearing push never reached the store"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let after =
        scoped_diagnostics_inspect_outside(&ctx, "late-clear-after", file.to_str().unwrap());
    assert_not_certified_stale(&after, &file, 1);
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
    assert_eq!(summary.get("errors"), Some(&Value::Null));
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
        mutated["gaps"]
            .as_array()
            .is_none_or(|gaps| gaps.iter().all(|gap| gap["kind"] != "uncovered_file")),
        "forced coverage must remove the diagnostics gap: {mutated:#}"
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
        response["gaps"]
            .as_array()
            .is_none_or(|gaps| gaps.iter().all(|gap| gap["kind"] != "uncovered_file")),
        "covered clean scope must not carry a diagnostics gap: {response:#}"
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

/// A producer that never settles leaves a named gap without hiding other categories.
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

    assert_eq!(response["success"], true, "response: {response:#}");
    assert_eq!(response["complete"], false);
    assert!(response["text"]
        .as_str()
        .unwrap()
        .contains("still indexing after"));
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
    let counts = ctx.status_bar_count_values();
    assert_eq!((counts.errors, counts.warnings), (None, None));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the terminal reserve must return well before the 10s request budget: {:?}",
        started.elapsed()
    );
}

/// A larger configured budget still bounds indexing and reports how to retry.
#[test]
fn blocking_inspect_indexing_gap_names_elapsed_wait_and_retry() {
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

    assert_eq!(response["success"], true, "response: {response:#}");
    let gap = response["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gap| gap["producer"] == "rust")
        .unwrap();
    let reason = gap["reason"].as_str().unwrap();
    assert!(reason.contains("still indexing after"), "{reason}");
    assert!(reason.contains("retry aft_inspect"), "{reason}");
}

/// This fake neither begins a compiler check nor publishes diagnostics.
/// Quiescence alone is not evidence of a completed clean Rust check.
#[test]
fn blocking_inspect_names_a_quiescent_rust_producer_before_any_check_begins() {
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
    // The request completed, but Rust diagnostics are unknown, so the terminal
    // is partial (a completed result that names the unknown producer) rather
    // than fresh.
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    assert!(
        response["partial_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("rust")),
        "{response:#}"
    );
    let phases = response["wait_stamp"]["phases"]
        .as_array()
        .unwrap_or_else(|| panic!("wait_stamp.phases missing: {response:#}"));
    assert!(
        phases.iter().any(|phase| phase["id"] == "lsp_quiescence"),
        "quiescence must complete before the completed terminal: {response:#}"
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
    assert!(summary["errors"].is_null(), "{response:#}");
    assert!(summary["warnings"].is_null(), "{response:#}");
    assert_eq!(summary["complete"], false, "{response:#}");
    assert_eq!(summary["gaps"][0]["producer"], "rust", "{response:#}");
    assert!(summary["by_producer"]["rust"]["errors"].is_null());
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
    let open_result = crate::test_helpers::settled_lsp(&mut ctx.lsp(), |manager| {
        manager.ensure_file_open(&pre_opened, &config)
    })
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

/// A scoped request whose `sections` list other categories and leave out
/// diagnostics does no language-server work at all. A scoped request that
/// omits `sections` is different: it still collects diagnostics, because
/// checking a file's compiler errors is the main reason to scope an inspect.
#[test]
fn scoped_inspect_without_diagnostics_does_no_producer_work() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-no-work\"\n");
    let pre_opened = write_file(&root, "src/a.rs", "fn a() {}\n");
    let never_opened = [
        write_file(&root, "src/b.rs", "fn b() {}\n"),
        write_file(&root, "src/c.rs", "fn c() {}\n"),
    ];
    write_file(&root, "src/cold.ts", "export const cold = 1;\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_file_open(&pre_opened, &ctx.config())
    })
    .unwrap();
    collect_lsp_notifications(&ctx, "custom/documentOpened", 1);
    ctx.update_status_bar_tier2(Some(11), Some(12), Some(13), Some(14), false);

    for sections in [json!(["dead_code"]), json!("todos")] {
        let params = json!({
            "id": "inspect-no-diagnostics", "command": "inspect", "scope": "src",
            "sections": sections,
        });
        let response = handle_inspect_tool_call(&request(params), &ctx);
        assert!(response.success, "{response:?}");
        // Check whether inspect opened or analyzed documents before checking
        // its response: discarded diagnostics must not conceal document analysis.
        for file in &never_opened {
            assert!(!ctx.lsp().document_is_open_for_test(file));
            assert!(
                !ctx.lsp().has_diagnostic_report_for_file(file),
                "cold file was analyzed: {file:?}"
            );
        }
        let leftover = ctx.lsp().drain_events().events;
        assert!(
            leftover.iter().all(|event| !matches!(event,
                LspEvent::Notification { method, .. }
                if method == "custom/documentOpened" || method == "custom/documentClosed"
            )),
            "unexpected document notifications: {leftover:?}"
        );
        assert_eq!(
            response.data["summary"]["diagnostics"],
            json!({"status": "not_requested"})
        );
        assert!(response.data["details"].get("diagnostics").is_none());
        assert!(!response.data["text"]
            .as_str()
            .unwrap()
            .contains("diagnostics:"));
        assert_eq!(
            ctx.lsp().active_client_count(),
            1,
            "scoped request started a cold producer"
        );
        assert!(response.data["wait_stamp"]["phases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|phase| !matches!(phase["id"].as_str(), Some("lsp_start" | "lsp_quiescence"))));
        let bar = ctx.status_bar_count_values();
        assert_eq!(
            (bar.dead_code, bar.unused_exports, bar.duplicates, bar.todos),
            (Some(11), Some(12), Some(13), Some(14))
        );
    }

    // Omitted sections: the scoped request reports diagnostics and waits for
    // the started producers, unlike the explicit non-diagnostics requests above.
    let response = handle_inspect_tool_call(
        &request(json!({"id": "inspect-omitted-sections", "command": "inspect", "scope": "src"})),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    let diagnostics = &response.data["summary"]["diagnostics"];
    assert!(
        diagnostics.is_object() && diagnostics.get("status").is_none(),
        "omitted sections must still report diagnostics: {diagnostics:#}"
    );
    assert!(
        response.data["wait_stamp"]["phases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|phase| phase["id"] == "lsp_quiescence"),
        "omitted sections must wait for diagnostics producers: {:#}",
        response.data["wait_stamp"]
    );
    let bar = ctx.status_bar_count_values();
    assert_eq!(
        (bar.dead_code, bar.unused_exports, bar.duplicates, bar.todos),
        (Some(11), Some(12), Some(13), Some(14)),
        "a scoped request must not overwrite project totals"
    );
}

/// The unused-exports drill-down used to stop at 100 rows and set
/// `drill_down_capped`. Now the full list is stored and paged: at `topK` 100 a
/// 101-row result shows a bounded first page that names the cut in text and in
/// its structured envelope, and the next page returns the remaining row.
#[test]
fn inspect_unused_exports_pages_101_items_past_the_former_cap() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        "{\"name\":\"cap-paging-app\",\"private\":true}",
    );
    write_file(&root, "src/main.ts", "console.log('entry');\n");
    let source = (0..101)
        .map(|i| format!("export function unused_{i:03}() {{ return {i}; }}\n"))
        .collect::<String>();
    write_file(&root, "src/spares.ts", &source);
    let ctx = configured_context(&root);
    let page = |offset| {
        inspect(
            &ctx,
            json!({
                "id": format!("cap-page-{offset}"), "command": "inspect",
                "sections": ["unused_exports"], "topK": 100, "offset": offset,
            }),
        )
    };

    let first = page(0);
    assert_eq!(first["success"], true, "{first:#}");
    let first_rows = first["details"]["unused_exports"].as_array().unwrap();
    assert_eq!(first_rows.len(), 100, "{first:#}");
    let envelope = &first["details"]["unused_exports_list_envelope"];
    assert_eq!(envelope["total"]["value"], 101, "{envelope:#}");
    assert_eq!(envelope["next_offset"], 100, "{envelope:#}");
    assert_eq!(envelope["reasons"], json!(["cap"]), "{envelope:#}");
    let first_text = first["text"].as_str().unwrap();
    assert!(
        first_text.contains("shown 100 of 101 items (cap)"),
        "{first_text}"
    );

    let second = page(100);
    assert_eq!(second["success"], true, "{second:#}");
    let second_rows = second["details"]["unused_exports"].as_array().unwrap();
    assert_eq!(second_rows.len(), 1, "{second:#}");
    let envelope = &second["details"]["unused_exports_list_envelope"];
    assert_eq!(envelope["offset"], 100, "{envelope:#}");
    assert_eq!(envelope["next_offset"], Value::Null, "{envelope:#}");
    assert_eq!(envelope["reasons"], json!([]), "{envelope:#}");
    let second_text = second["text"].as_str().unwrap();
    assert!(!second_text.contains("shown "), "{second_text}");

    let symbols = first_rows
        .iter()
        .chain(second_rows)
        .filter_map(|row| row["symbol"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(symbols.len(), 101);
    assert!(symbols.contains("unused_000") && symbols.contains("unused_100"));
}

#[test]
fn inspect_offset_pages_all_150_unused_exports() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        "{\"name\":\"paging-app\",\"private\":true}",
    );
    write_file(&root, "src/main.ts", "console.log('entry');\n");
    let source = (0..150)
        .map(|i| format!("export function spare{i:03}() {{ return {i}; }}\n"))
        .collect::<String>();
    write_file(&root, "src/spares.ts", &source);
    let ctx = configured_context(&root);
    let page = |offset| {
        inspect(
            &ctx,
            json!({
                "id": format!("page-{offset}"), "command": "inspect", "sections": ["unused_exports"],
                "topK": 100, "offset": offset,
            }),
        )
    };
    let first = page(0);
    assert_eq!(first["success"], true, "{first:#}");
    let bar = ctx.status_bar_count_values();
    for (category, count) in [
        ("dead_code", bar.dead_code),
        ("unused_exports", bar.unused_exports),
        ("duplicates", bar.duplicates),
        ("todos", bar.todos),
    ] {
        assert_eq!(
            count,
            first["summary"][category]["count"]
                .as_u64()
                .map(|n| n as usize)
        );
    }
    assert!(first["text"]
        .as_str()
        .unwrap()
        .contains("Unused exports: 150"));
    let details = &first["details"];
    assert_eq!(
        details["unused_exports_list_envelope"]["total"]["value"],
        150
    );
    assert_eq!(details["unused_exports_list_envelope"]["next_offset"], 100);
    assert_eq!(details["unused_exports"].as_array().unwrap().len(), 100);
    assert_eq!(first["details"], page(0)["details"]);
    let second = page(100);
    let second_details = &second["details"];
    assert_eq!(
        second_details["unused_exports"].as_array().unwrap().len(),
        50
    );
    assert_eq!(
        second_details["unused_exports_list_envelope"]["offset"],
        100
    );
    assert_eq!(
        second_details["unused_exports_list_envelope"]["next_offset"],
        Value::Null
    );
    assert_eq!(
        second_details["unused_exports_list_envelope"]["reasons"],
        json!([])
    );
    let rows = details["unused_exports"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second_details["unused_exports"].as_array().unwrap())
        .map(|row| {
            (
                row["file"].to_string(),
                row["symbol"].to_string(),
                row["line"].to_string(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(rows.len(), 150);
    let exhausted = page(150);
    assert_eq!(exhausted["details"]["unused_exports"], json!([]));
    assert_eq!(
        exhausted["details"]["unused_exports_list_envelope"]["total"]["value"],
        150
    );
    assert_eq!(
        exhausted["details"]["unused_exports_list_envelope"]["next_offset"],
        Value::Null
    );
    let scoped = inspect(
        &ctx,
        json!({
            "id": "scoped-page-100", "command": "inspect", "sections": ["unused_exports"],
            "scope": "src/spares.ts", "topK": 100, "offset": 100,
        }),
    );
    assert_eq!(
        scoped["details"]["unused_exports"],
        second_details["unused_exports"]
    );
    assert_eq!(ctx.status_bar_count_values().unused_exports, Some(150));
}

#[test]
fn inspect_python_gap_is_unknown_without_supported_files_and_counted_when_mixed() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "service.py", "def spare():\n    return 1\n");
    write_file(&root, "README.md", "documentation\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    ctx.update_status_bar_tier2(Some(99), Some(0), Some(0), Some(0), false);
    let response = inspect(
        &ctx,
        json!({"id":"python-only", "command":"inspect", "sections":["dead_code"]}),
    );
    assert_eq!(response["success"], true, "{response:#}");
    let summary = &response["summary"]["dead_code"];
    assert_eq!(summary["count"], Value::Null, "{summary:#}");
    assert_eq!(summary["complete"], false);
    assert!(summary["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| gap["kind"] == "language_unsupported"
            && gap["language"] == "python"
            && gap["files"] == 1));
    assert_eq!(ctx.status_bar_count_values().dead_code, None);

    write_file(&root, "main.ts", "console.log('supported');\n");
    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({"id":"python-mixed", "command":"inspect", "sections":["dead_code"]})),
        &ctx,
    ))
    .unwrap();
    let summary = &response["summary"]["dead_code"];
    assert!(summary["count"].is_u64(), "{response:#}");
    assert_eq!(summary["complete"], false);
    assert_eq!(
        ctx.status_bar_count_values().dead_code,
        summary["count"].as_u64().map(|count| count as usize)
    );
    assert!(response["text"].as_str().unwrap().contains("python"));
}

#[test]
fn scoped_diagnostics_discovery_preserves_project_bar_counts() {
    let (_temp_dir, root, _lib) = single_crate_fixture("project-bar");
    let pre_opened = write_file(&root, "src/old.rs", "pub fn old() {}\n");
    let cold = write_file(&root, "src/cold.rs", "pub fn cold() {}\n");
    let ctx = configured_context(&root);
    let ready_release = root.join("release-initial-quiescence");
    let _ready_cleanup = super::helpers::ReleaseOnDrop::new(&ready_release);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "publish_then_quiescent");
    ctx.lsp().set_extra_env(
        "AFT_FAKE_LSP_QUIESCENCE_RELEASE_FILE",
        ready_release.to_str().unwrap(),
    );
    // This fixture intentionally holds the initial report provisional. Wait for
    // its publication here; authoritative version proof must wait until the
    // readiness gate below is released.
    let config = ctx.config().clone();
    let (pre, expected) = {
        let mut lsp = ctx.lsp();
        lsp.drain_events();
        let pre = lsp.snapshot_pre_edit_state(&pre_opened);
        let expected = lsp
            .notify_file_changed_versioned(&pre_opened, "pub fn old() {}\n", &config)
            .expect("notify file changed");
        (pre, expected)
    };
    assert!(
        !expected.is_empty(),
        "the fake must receive a versioned edit"
    );
    wait_for_lsp_report_state(&ctx, &pre_opened, true);
    let wait_for_notified_versions = |timeout| {
        let deadline = Instant::now() + timeout;
        loop {
            // A zero-duration check drains current events without holding the
            // manager mutex across the wait for the server's readiness signal.
            let outcome = ctx.lsp().wait_for_post_edit_diagnostics(
                &pre_opened,
                &config,
                &expected,
                &pre,
                Duration::ZERO,
            );
            if outcome.complete() || Instant::now() >= deadline {
                break outcome;
            }
            thread::sleep(Duration::from_millis(10));
        }
    };
    let warming = wait_for_notified_versions(Duration::ZERO);
    assert!(
        !warming.complete(),
        "warming reports cannot prove freshness"
    );
    assert!(warming.accepted_snapshots.is_empty());
    assert_eq!(warming.pending_servers.len(), expected.len());
    ctx.update_status_bar_tier2(Some(11), Some(12), Some(13), Some(14), false);
    assert!(ctx.lsp().has_diagnostic_report_for_file(&pre_opened));
    assert!(
        ctx.status_bar_count_values().errors.is_none(),
        "the fake must hold the initial report provisional until the readiness wait"
    );
    let wait_for_before = || {
        // Require the same notified versions after readiness, without sending
        // another edit. Keep the first bar snapshot with a known error count;
        // a later read could hit the manager-contention fallback again.
        std::fs::write(&ready_release, "ready\n").unwrap();
        let ready = wait_for_notified_versions(Duration::from_secs(30));
        assert!(
            ready.complete(),
            "the notified versions must become fresh: {ready:?}"
        );
        assert_eq!(ready.accepted_snapshots.len(), expected.len());
        assert!(!ready.diagnostics.is_empty());
        wait_for_lsp_report_state(&ctx, &pre_opened, false);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let before = ctx.status_bar_count_values();
            if before.errors.is_some() {
                break before;
            }
            assert!(Instant::now() < deadline, "project bar never acquired a known error count after the authoritative report for {pre_opened:?}: {before:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let before = wait_for_before();
    assert!(before.errors.is_some());
    assert!(!ctx.lsp().has_diagnostic_report_for_file(&cold));
    let response = handle_inspect_tool_call(
        &request(json!({
            "id": "scoped-discovery", "command": "inspect", "sections": ["diagnostics"], "scope": cold,
        })),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    let after = ctx.status_bar_count_values();
    assert_eq!(
        (
            after.dead_code,
            after.unused_exports,
            after.duplicates,
            after.todos
        ),
        (
            before.dead_code,
            before.unused_exports,
            before.duplicates,
            before.todos
        )
    );
    assert_eq!(after.diagnostics, before.diagnostics);
    assert_eq!(after.diagnostics_gaps, before.diagnostics_gaps);
    assert!(
        after.errors.unwrap() > before.errors.unwrap(),
        "{before:?} -> {after:?}"
    );
    assert_eq!(
        after.errors.unwrap(),
        ctx.lsp().warm_error_warning_counts().0
    );
    assert!(
        after.errors.unwrap()
            > response.data["summary"]["diagnostics"]["errors"]
                .as_u64()
                .unwrap() as usize
    );
}

#[test]
fn scoped_typescript_inspect_preserves_missing_gopls_mask() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{}");
    let ts = write_file(&root, "src/main.ts", "console.log('ts');\n");
    write_file(&root, "go.mod", "module example.test/app\n");
    write_file(&root, "main.go", "package main\nfunc main() {}\n");
    let ctx = configured_context(&root);
    ctx.update_config(|config| {
        config.disabled_lsp.insert("biome".into());
        config.disabled_lsp.insert("oxlint".into());
    });
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp()
        .override_binary(ServerKind::Go, root.join("missing-gopls"));
    // Configure takes the project-wide applicability snapshot after the test's
    // deterministic binary overrides, so host-installed gopls is irrelevant.
    ctx.set_config(ctx.config().as_ref().clone());
    // The status bar never waits for the background applicability walk this
    // configuration publication started; it would read "pending" until then.
    assert!(
        ctx.wait_for_project_applicability(Duration::from_secs(30)),
        "the language-server applicability walk did not finish"
    );
    open_with_lsp(&ctx, &ts, "console.log('ts');\n");
    let before = ctx.status_bar_count_values();
    assert_eq!(before.diagnostics, Some("producer_missing"));
    assert!(before
        .diagnostics_gaps
        .iter()
        .any(|gap| gap["producer"] == "gopls"));
    let response = handle_inspect_tool_call(
        &request(json!({
            "id": "ts-missing-go", "command": "inspect", "sections": ["diagnostics"], "scope": ts,
        })),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    let after = ctx.status_bar_count_values();
    assert_eq!(
        (after.errors, after.warnings, after.diagnostics),
        (None, None, Some("producer_missing"))
    );
    assert_eq!(after.diagnostics_gaps, before.diagnostics_gaps);
}

/// `configured_context_with_diagnostics_timeout`, with every language-server
/// applicability walk of the context held from before configure starts the
/// first one, so the walk's result cannot land before the test releases it.
fn configured_context_with_held_applicability(
    root: &Path,
    timeout_ms: u64,
) -> (AppContext, aft::context::ProjectApplicabilityHold) {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    ctx.isolate_cold_build_limiter_for_test(2);
    let hold = ctx.hold_project_applicability_walks_for_test();
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
            "inspect": { "diagnostics_timeout_ms": timeout_ms }
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    (ctx, hold)
}

/// A TypeScript file served by the fake server next to a Go module whose
/// gopls is missing, configured while applicability walks are held. The walk
/// that will run on release is the one for this final configuration.
fn held_typescript_with_missing_gopls(
    timeout_ms: u64,
) -> (
    tempfile::TempDir,
    PathBuf,
    AppContext,
    aft::context::ProjectApplicabilityHold,
) {
    let (temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{}");
    let ts = write_file(&root, "src/main.ts", "console.log('ts');\n");
    write_file(&root, "go.mod", "module example.test/app\n");
    write_file(&root, "main.go", "package main\nfunc main() {}\n");
    let (ctx, hold) = configured_context_with_held_applicability(&root, timeout_ms);
    ctx.update_config(|config| {
        config.disabled_lsp.insert("biome".into());
        config.disabled_lsp.insert("oxlint".into());
    });
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp()
        .override_binary(ServerKind::Go, root.join("missing-gopls"));
    // Supersedes configure's held walk with one for the configuration above.
    ctx.set_config(ctx.config().as_ref().clone());
    open_with_lsp(&ctx, &ts, "console.log('ts');\n");
    (temp_dir, ts, ctx, hold)
}

fn assert_pending_on_running_walk(ctx: &AppContext) {
    let values = ctx.status_bar_count_values();
    assert_eq!(
        (values.errors, values.diagnostics, values.diagnostics_reason),
        (None, Some("pending"), Some("applicability_walk_running")),
        "{values:?}"
    );
    let status = serde_json::to_value(aft::commands::status::handle_status(
        &request(json!({"id": "status", "command": "status"})),
        ctx,
    ))
    .unwrap();
    let status_values = &status["status_bar_values"];
    assert_eq!(status_values["diagnostics"], "pending", "{status:#}");
    assert_eq!(
        status_values["diagnostics_reason"], "applicability_walk_running",
        "{status:#}"
    );
}

/// Runs `scoped_ts` inspect on a thread and waits until it is either inside
/// the applicability wait or finished. Returns whether it was seen waiting,
/// the response, and the status values read as soon as it returned.
fn scoped_inspect_against_held_walk(
    ctx: &AppContext,
    scoped_ts: &Path,
    hold: aft::context::ProjectApplicabilityHold,
    release_before_join: bool,
) -> (bool, Value, aft::context::StatusBarCountValues) {
    thread::scope(|threads| {
        let inspect = threads.spawn(|| {
            let response = handle_inspect_tool_call(
                &request(json!({
                    "id": "ts-held-walk", "command": "inspect",
                    "sections": ["diagnostics"], "scope": scoped_ts,
                })),
                ctx,
            );
            // Read before anything else can release the walk.
            (
                serde_json::to_value(response).unwrap(),
                ctx.status_bar_count_values(),
            )
        });
        let hang_deadline = Instant::now() + Duration::from_secs(60);
        let waited = loop {
            if ctx.project_applicability_waiters_for_test() > 0 {
                break true;
            }
            if inspect.is_finished() || Instant::now() >= hang_deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(2));
        };
        if waited {
            // While the walk is still running, the status bar and the status
            // reply (neither of which waits for the walk) must say diagnostics
            // are pending and name the running walk as the reason.
            assert_pending_on_running_walk(ctx);
        }
        let mut hold = Some(hold);
        if release_before_join {
            drop(hold.take());
        }
        let (response, values) = inspect.join().expect("inspect thread");
        drop(hold);
        (waited, response, values)
    })
}

#[test]
fn scoped_inspect_waits_for_the_applicability_walk_and_reports_missing_producer() {
    let (_temp_dir, ts, ctx, hold) = held_typescript_with_missing_gopls(40_000);
    let (waited, response, values) = scoped_inspect_against_held_walk(&ctx, &ts, hold, true);
    assert_eq!(response["success"], true, "{response:#}");
    assert!(
        !response.to_string().contains("applicability_pending"),
        "a walk that finished within the wait leaves no gap: {response:#}"
    );
    assert_eq!(
        (values.diagnostics, values.diagnostics_reason),
        (Some("producer_missing"), None),
        "inspect answered before the applicability walk stored its result: {values:?}"
    );
    assert!(
        values
            .diagnostics_gaps
            .iter()
            .any(|gap| gap["producer"] == "gopls"),
        "{values:?}"
    );
    assert!(waited, "inspect never waited for the applicability walk");
}

#[test]
fn scoped_inspect_reports_pending_with_reason_when_the_walk_outlasts_its_wait() {
    // A 14 s inspect budget keeps 5 s back for assembling the reply, leaving
    // 9 s of work time. The applicability wait may use half of the remaining
    // work time, so it gives up after about 4.5 s and leaves the rest for the
    // scoped diagnostics.
    let (_temp_dir, ts, ctx, hold) = held_typescript_with_missing_gopls(14_000);
    let (waited, response, values) = scoped_inspect_against_held_walk(&ctx, &ts, hold, false);
    assert!(waited, "inspect never waited for the applicability walk");
    assert_eq!(response["success"], true, "{response:#}");
    // Which language servers apply was not decided, so the reply must not read
    // as a determination: PARTIAL, naming the reason and retry guidance.
    let header = response["text"]
        .as_str()
        .and_then(|text| text.lines().next())
        .unwrap_or_default();
    assert_eq!(
        header,
        "PARTIAL — diagnostics unknown: language-server applicability: applicability_walk_running: the walk that decides which language servers apply to this project has not finished; retry aft_inspect.",
        "{response:#}"
    );
    assert_eq!(response["complete"], false, "{response:#}");
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    let diagnostics = &response["summary"]["diagnostics"];
    assert_eq!(diagnostics["complete"], false, "{response:#}");
    assert!(diagnostics["errors"].is_null(), "{response:#}");
    let is_walk_gap = |gap: &Value| {
        gap["kind"] == "applicability_pending"
            && gap["producer"] == "language-server applicability"
            && gap["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("applicability_walk_running"))
    };
    assert!(
        diagnostics["gaps"]
            .as_array()
            .is_some_and(|gaps| gaps.iter().any(is_walk_gap)),
        "{response:#}"
    );
    assert!(
        response["gaps"].as_array().is_some_and(|gaps| gaps
            .iter()
            .any(|gap| is_walk_gap(gap) && gap["categories"] == json!(["diagnostics"]))),
        "{response:#}"
    );
    assert_eq!(
        (values.errors, values.diagnostics, values.diagnostics_reason),
        (None, Some("pending"), Some("applicability_walk_running")),
        "{values:?}"
    );
    // Once released, the walk's result replaces the pending state.
    assert!(ctx.wait_for_project_applicability(Duration::from_secs(30)));
    assert_eq!(
        ctx.status_bar_count_values().diagnostics,
        Some("producer_missing")
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
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
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
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
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
        crate::context_storage::isolate(Config {
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
        }),
    )
    .with_test_checkpoint_store();
    crate::helpers::disable_in_process_file_watcher();
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "indexes": { "trigram": false, "semantic": false }
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
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
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
fn inspect_failed_producer_reason_names_exit_code_and_first_stderr_line() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"inspect-exit\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "src/lib.rs", "pub fn rust_value() -> u8 { 1 }\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );

    let mut lsp = ctx.lsp();
    lsp.override_binary(ServerKind::Rust, fake_server_path());
    lsp.set_extra_env("AFT_FAKE_LSP_INIT_EXIT_CODE", "3");
    // Many stderr lines: the inspect reason must quote the first one and
    // stay short instead of inlining the whole tail.
    let mut stderr = vec!["error: rust-analyzer could not load the workspace".to_string()];
    stderr.extend((0..40).map(|index| format!("stderr filler line {index:02}")));
    lsp.set_extra_env("AFT_FAKE_LSP_INIT_EXIT_STDERR", &stderr.join("|"));
    drop(lsp);

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-producer-exit",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": "src/lib.rs",
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    let text = response["text"].as_str().expect("rendered text");
    assert!(text.starts_with("PARTIAL — diagnostics unknown:"), "{text}");
    assert!(!text.contains("0 errors"), "{text}");
    assert!(text.contains("rust"), "{text}");
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
    assert!(response["summary"]["diagnostics"]["warnings"].is_null());
    let counts = ctx.status_bar_count_values();
    assert_eq!((counts.errors, counts.warnings), (None, None));

    let rust_gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["producer"] == "rust"))
        .unwrap_or_else(|| panic!("Rust producer gap missing: {response:#}"));
    let reason = rust_gap["reason"].as_str().expect("gap reason");
    assert!(
        reason.contains("server crashed during initialize (exit 3 after "),
        "reason must name the exit code: {reason}"
    );
    assert!(
        reason.contains("error: rust-analyzer could not load the workspace"),
        "reason must quote the first stderr line: {reason}"
    );
    assert!(
        !reason.contains("stderr filler line 39"),
        "reason must not inline the whole stderr tail: {reason}"
    );
    assert!(
        !reason.contains('\n') && reason.len() <= 600,
        "reason must stay a bounded single line ({} bytes): {reason:?}",
        reason.len()
    );
}

#[test]
fn scoped_files_without_diagnostics_roll_up_to_one_cause_and_top_k_paths() {
    // A fresh worktree of a JavaScript project: package.json but no
    // node_modules, so the TypeScript server binary cannot be resolved.
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "web/package.json", "{\"name\":\"rollup-web\"}\n");
    // Keep package.json itself out of the scoped candidates: it is a JSON
    // file with a producer of its own and would add a second cause.
    write_file(&root, ".aftignore", "package.json\n");
    for index in 0..7 {
        write_file(
            &root,
            &format!("web/src/file_{index}.ts"),
            &format!("export const value{index} = {index};\n"),
        );
    }
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    {
        let mut lsp = ctx.lsp();
        lsp.override_binary(
            ServerKind::TypeScript,
            PathBuf::from("/definitely/missing/typescript-language-server"),
        );
        // Keep the other JavaScript-family servers out of the picture so the
        // test does not depend on what is installed on this machine.
        lsp.override_binary(
            ServerKind::Biome,
            PathBuf::from("/definitely/missing/biome"),
        );
    }

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-uncovered-rollup",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": "web",
            "topK": 3,
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["complete"], false);
    let text = response["text"].as_str().expect("rendered text");
    let group_lines = text
        .lines()
        .filter(|line| line.starts_with("PARTIAL — diagnostics unknown:"))
        .collect::<Vec<_>>();
    assert_eq!(group_lines.len(), 1, "one line per cause: {text}");
    assert!(
        group_lines[0].starts_with("PARTIAL — diagnostics unknown: typescript @ web: "),
        "{text}"
    );
    // The status header carries the install remedy exactly once.
    let remedy = "no node_modules in web: the project's dependencies are not installed; \
                  run your package manager's install";
    assert_eq!(
        text.matches(remedy).count(),
        1,
        "the cause must name the install remedy exactly once: {text}"
    );
    assert!(
        text.lines().any(|line| line
            .starts_with("PARTIAL — diagnostics unknown: typescript @ web: ")
            && line.contains(remedy)),
        "the producer line must carry the install remedy: {text}"
    );
    assert!(
        group_lines[0].ends_with("(7 files); retry aft_inspect."),
        "{text}"
    );
    let listed = text
        .lines()
        .filter(|line| line.starts_with("  web/src/file_") && line.ends_with(".ts"))
        .count();
    assert_eq!(listed, 3, "at most topK paths: {text}");
    assert!(
        text.contains("shown 3 of 7 items (cap) · narrow: topK, scope, sections"),
        "{text}"
    );

    // Every file keeps its structured gap row for machine consumers.
    let uncovered = response["gaps"]
        .as_array()
        .expect("gaps")
        .iter()
        .filter(|gap| gap["kind"] == "uncovered_file")
        .collect::<Vec<_>>();
    assert_eq!(uncovered.len(), 7, "{response:#}");
    assert!(uncovered
        .iter()
        .all(|gap| gap["cause"]["producer"] == "typescript" && gap["cause"]["root"] == "web"));
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

    // A blocking scoped inspect reads the live report of a document that is
    // already open instead of asking the server again, so the TypeScript
    // findings warmed through the normal edit path are the ones it reports.
    let app_ts = root.join("web/src/app.ts");
    open_with_lsp(
        &ctx,
        &app_ts,
        "export function tsValue(): number { return 1; }\n",
    );

    let before_failure = ctx.status_bar_count_values();
    // TypeScript's report alone cannot certify project totals while the
    // expected Rust producer has not supplied authoritative diagnostics.
    assert_eq!(
        (before_failure.errors, before_failure.warnings),
        (None, None)
    );

    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-producer-gap",
            "sections": ["diagnostics"],
            "command": "inspect",
            "scope": ["src/lib.rs", "web/src/app.ts"],
        })),
        &ctx,
    ))
    .expect("inspect response serializes");

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    // The Rust producer failed, so the completed result is partial and its
    // reason names rust.
    assert_eq!(response["inspect_terminal"], "partial");
    assert!(
        response["partial_reason"]
            .as_str()
            .is_some_and(
                |reason| reason.starts_with("diagnostics unknown: rust-analyzer @ .:")
                    && reason.ends_with("retry aft_inspect.")
            ),
        "{response:#}"
    );
    assert_eq!(response["complete"], false);
    assert!(
        response["summary"]["diagnostics"]["by_producer"]["typescript"]["errors"]
            .as_u64()
            .is_some_and(|errors| errors > 0),
        "the working TypeScript producer's diagnostics should survive: {response:#}"
    );
    let rust_gap = response["gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["producer"] == "rust"))
        .unwrap_or_else(|| panic!("Rust producer gap missing: {response:#}"));
    let text = response["text"].as_str().expect("rendered text");
    assert!(text.starts_with("PARTIAL — diagnostics unknown:"), "{text}");
    assert!(!text.contains("diagnostics: 0 errors"), "{text}");
    assert!(text.contains("from typescript"), "{text}");
    assert!(text.contains("test diagnostic error"), "{text}");
    assert!(text.contains("rust"), "{text}");
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
    let counts = ctx.status_bar_count_values();
    assert_eq!((counts.errors, counts.warnings), (None, None));
    assert_eq!(rust_gap["kind"], "failed_producer");
    assert_eq!(rust_gap["categories"], json!(["diagnostics"]));
    assert!(
        rust_gap["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "failed producer reason missing: {response:#}"
    );
}

#[test]
fn scoped_rust_inspect_preserves_stale_cargo_lock() {
    let available = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        eprintln!("SKIP scoped_rust_inspect_preserves_stale_cargo_lock: rust-analyzer is not installed (or rustup component unavailable)");
        return;
    }
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"lock-reader\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nlocal-dep = { path = \"dep\" }\n");
    write_file(
        &root,
        "src/lib.rs",
        "pub fn value() -> u8 { local_dep::value() }\n",
    );
    let manifest = write_file(
        &root,
        "dep/Cargo.toml",
        "[package]\nname = \"local-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&root, "dep/src/lib.rs", "pub fn value() -> u8 { 1 }\n");
    let generated = std::process::Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo is required for Rust integration tests");
    assert!(generated.status.success(), "{generated:?}");
    let before = fs::read(root.join("Cargo.lock")).unwrap();
    fs::write(
        manifest,
        "[package]\nname = \"local-dep\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-stale-lock",
            "command": "inspect",
            "scope": "src",
            "sections": ["diagnostics"],
        })),
        &ctx,
    ))
    .unwrap();
    assert_eq!(
        fs::read(root.join("Cargo.lock")).unwrap(),
        before,
        "Rust-scoped inspect rewrote the stale Cargo.lock"
    );
    assert_eq!(ctx.lsp().active_server_keys().len(), 1, "{response:#}");
    assert_eq!(response["complete"], false, "{response:#}");
    let gap = response["gaps"]
        .as_array()
        .and_then(|gaps| {
            gaps.iter()
                .find(|gap| gap["kind"] == "failed_producer" && gap["producer"] == "rust")
        })
        .unwrap_or_else(|| panic!("missing rust-analyzer failure: {response:#}"));
    assert!(
        gap["reason"].as_str().unwrap().contains("--locked"),
        "metadata failure must explain why diagnostics are unavailable: {response:#}"
    );
}

/// A rust-analyzer run with `--locked` over a stale Cargo.lock reports a
/// failed workspace load and keeps it: it re-reads the lockfile only when the
/// client reports a change or asks for a reload. After the lockfile is fixed
/// outside AFT (as `cargo update` in a shell does), the next scoped inspect
/// must make the server reload and report the new load, not the old failure.
/// The fake server reads Cargo.lock only when it loads the workspace, which is
/// exactly the behavior that made the failure stick.
#[test]
fn scoped_rust_inspect_recovers_after_a_stale_cargo_lock_is_fixed() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"lock-recovery\"\nversion = \"0.1.0\"\n",
    );
    write_file(&root, "Cargo.lock", "# stale lockfile\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "cargo_lock");

    let stale = scoped_diagnostics_inspect(&ctx, "inspect-lock-stale", "src/main.rs");
    assert_eq!(failed_producers(&stale), vec!["rust"], "{stale:#}");
    let text = stale["text"].as_str().expect("rendered text");
    assert_eq!(
        text.matches("--locked was passed").count(),
        1,
        "the producer failure must be printed once: {text}"
    );

    // Modification times must land after the server's load for the change to
    // count; a short pause keeps coarse file-system clocks from tying.
    thread::sleep(Duration::from_millis(50));
    fs::write(root.join("Cargo.lock"), "# fixed lockfile\n").unwrap();

    let fixed = scoped_diagnostics_inspect(&ctx, "inspect-lock-fixed", "src/main.rs");
    assert!(
        failed_producers(&fixed).is_empty(),
        "the fixed lockfile must not keep reporting the old failure: {fixed:#}"
    );
    assert!(uncovered_files(&fixed).is_empty(), "{fixed:#}");
    assert_eq!(fixed["summary"]["diagnostics"]["errors"], 1, "{fixed:#}");
    assert!(
        diagnostic_messages_for(&fixed, "src/main.rs")
            .iter()
            .any(|message| message.contains("test diagnostic error")),
        "{fixed:#}"
    );
}

/// The same sequence against a real rust-analyzer: a scoped inspect over a
/// workspace whose Cargo.lock is stale for `--locked` fails, and after
/// `cargo update` fixes the lockfile the next scoped inspect reports the new
/// load instead of the old `cargo metadata` error.
#[test]
fn scoped_rust_inspect_recovers_after_cargo_update_with_real_rust_analyzer() {
    let available = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        eprintln!("SKIP scoped_rust_inspect_recovers_after_cargo_update_with_real_rust_analyzer: rust-analyzer is not installed (or rustup component unavailable)");
        return;
    }
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"lock-reader\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nlocal-dep = { path = \"dep\" }\n");
    write_file(
        &root,
        "src/lib.rs",
        "pub fn value() -> u8 { local_dep::value() }\n",
    );
    let manifest = write_file(
        &root,
        "dep/Cargo.toml",
        "[package]\nname = \"local-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&root, "dep/src/lib.rs", "pub fn value() -> u8 { 1 }\n");
    let generated = std::process::Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo is required for Rust integration tests");
    assert!(generated.status.success(), "{generated:?}");
    // A sibling crate moves its version: the lockfile is now stale.
    fs::write(
        manifest,
        "[package]\nname = \"local-dep\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let ctx = configured_context(&root);

    let stale = scoped_diagnostics_inspect(&ctx, "inspect-real-lock-stale", "src/lib.rs");
    assert!(
        failed_producers(&stale).contains(&"rust".to_string()),
        "{stale:#}"
    );
    // rust-analyzer re-fetches its workspace once on its own shortly after
    // startup; fix the lockfile only after that, so recovery cannot come
    // from that startup fetch.
    thread::sleep(Duration::from_secs(3));
    let updated = std::process::Command::new("cargo")
        .args(["update", "--workspace", "--offline"])
        .current_dir(&root)
        .output()
        .expect("run cargo update");
    assert!(updated.status.success(), "{updated:?}");

    let fixed = scoped_diagnostics_inspect(&ctx, "inspect-real-lock-fixed", "src/lib.rs");
    assert!(
        failed_producers(&fixed).is_empty(),
        "the fixed lockfile must not keep reporting the old failure: {fixed:#}"
    );
    assert!(
        !fixed["text"]
            .as_str()
            .unwrap_or_default()
            .contains("--locked was passed"),
        "{fixed:#}"
    );
}

/// A crate whose `src/user.rs` builds `S { a, b }` from `src/s.rs`. Returns
/// the root and the path of `src/s.rs`.
fn field_removal_crate() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"field-removal\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&root, "src/lib.rs", "pub mod s;\npub mod user;\n");
    let s = write_file(
        &root,
        "src/s.rs",
        "pub struct S {\n    pub a: u8,\n    pub b: u8,\n}\n",
    );
    write_file(
        &root,
        "src/user.rs",
        "use crate::s::S;\n\npub fn make() -> S {\n    S { a: 1, b: 2 }\n}\n",
    );
    // AFT runs rust-analyzer with `--locked`, which needs a lockfile.
    let generated = std::process::Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo is required for Rust integration tests");
    assert!(generated.status.success(), "{generated:?}");
    (temp_dir, root, s)
}

/// `(source, message)` of every diagnostic inspect reported for `file`.
fn diagnostic_sources_for(response: &Value, file: &str) -> Vec<(String, String)> {
    response["details"]["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["file"] == file)
        .map(|item| {
            (
                item["source"].as_str().unwrap_or_default().to_string(),
                item["message"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// After `b` was removed from `S`, `src/user.rs` no longer compiles. A
/// scoped inspect of it must say so, from the compiler (`cargo check`,
/// source `rustc`) and from rust-analyzer's own analysis, or name the
/// unfinished check. A clean answer is the stale result this guards against.
fn assert_removed_field_reported(response: &Value) {
    let text = response["text"].as_str().expect("rendered text");
    let still_checking = response["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|gap| gap["kind"] == "checking_producer");
    if still_checking {
        assert_eq!(response["complete"], false, "{response:#}");
        assert!(
            text.starts_with(
                "PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo check still running"
            ),
            "{text}"
        );
        return;
    }
    let reported = diagnostic_sources_for(response, "src/user.rs");
    assert!(
        reported
            .iter()
            .any(|(source, message)| source == "rustc" && message.contains("no field named `b`")),
        "cargo check's error for the removed field is missing: {text}\n{response:#}"
    );
    assert!(
        reported
            .iter()
            .any(|(source, message)| source == "rust-analyzer" && message.contains("no such field")),
        "rust-analyzer's own error for the removed field is missing: {text}\n{response:#}"
    );
    assert!(!text.contains("diagnostics: 0 errors"), "{text}");
}

/// An agent edits a use site (which leaves it open in rust-analyzer), then
/// removes a struct field with an AFT edit. rust-analyzer re-runs `cargo
/// check` only when told of a save, and analyzes an open document only when
/// asked for its diagnostics. Unless AFT sends the save and asks again, the
/// next inspect certifies the use site from diagnostics made before the
/// removal.
#[test]
fn scoped_rust_inspect_reports_a_removed_field_after_an_aft_edit_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "scoped_rust_inspect_reports_a_removed_field_after_an_aft_edit_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, s) = field_removal_crate();
    let ctx = configured_context(&root);
    enable_automatic_rust_checks(&ctx);
    let warm = scoped_diagnostics_inspect(&ctx, "field-removal-warm", "src");
    assert_eq!(warm["summary"]["diagnostics"]["errors"], 0, "{warm:#}");

    let user_edit = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "field-removal-edit-user",
            "command": "edit_match",
            "file": root.join("src/user.rs").display().to_string(),
            "match": "use crate::s::S;\n",
            "replacement": "use crate::s::S;\n// builds S\n",
        })),
        &ctx,
    );
    assert!(user_edit.success, "{user_edit:?}");
    let before = scoped_diagnostics_inspect(&ctx, "field-removal-before", "src/user.rs");
    assert_eq!(
        before["summary"]["diagnostics"]["errors"], 0,
        "control: the use site compiles before the removal: {before:#}"
    );

    let removal = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "field-removal-edit",
            "command": "edit_match",
            "file": s.display().to_string(),
            "match": "    pub b: u8,\n",
            "replacement": "",
        })),
        &ctx,
    );
    assert!(removal.success, "{removal:?}");
    let after = scoped_diagnostics_inspect(&ctx, "field-removal-after", "src/user.rs");
    assert_removed_field_reported(&after);
}

/// The same removal made by another program (a plain file write the project
/// watcher reports): rust-analyzer hears of it only as a watched-file change,
/// which does not start `cargo check`, so AFT sends rust-analyzer a save
/// notification for the changed file to start one.
#[test]
fn scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, s) = field_removal_crate();
    let ctx = configured_context(&root);
    enable_automatic_rust_checks(&ctx);
    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(watcher_rx);
    // Only the use site is examined first, so AFT has no diagnostics for
    // `src/s.rs`. Its change then reaches rust-analyzer only through the
    // watcher path, not through the resync AFT does for files it has
    // diagnostics for.
    let warm = scoped_diagnostics_inspect(&ctx, "outside-removal-warm", "src/user.rs");
    assert_eq!(warm["summary"]["diagnostics"]["errors"], 0, "{warm:#}");

    fs::write(&s, "pub struct S {\n    pub a: u8,\n}\n").unwrap();
    // Feed the event the project file watcher would report for the write.
    watcher_tx
        .send(aft::watcher_filter::WatcherDispatchEvent::Paths(vec![
            s.clone()
        ]))
        .expect("send watcher event");
    aft::runtime_drain::drain_watcher_events(&ctx);

    let after = scoped_diagnostics_inspect(&ctx, "outside-removal-after", "src/user.rs");
    assert_removed_field_reported(&after);
}

/// When a watcher drain forwards an outside edit while another thread holds
/// the LSP manager, the change waits in a queue for a helper thread, and
/// until that helper runs rust-analyzer has not heard of the edit, so no
/// `cargo check` is expected for it. The helper is held here, the way a
/// loaded machine can delay it, so the inspect runs while the change is still
/// queued. It must deliver the change itself and then report the compiler
/// error of the check the change starts, or name that check as still
/// running; the stale answer it used to give is a complete result without
/// that error.
#[test]
fn scoped_rust_inspect_forwards_a_queued_outside_edit_before_certifying() {
    let (_temp_dir, root, lib) = single_crate_fixture("queued-outside-edit");
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "0");
    let warm = scoped_diagnostics_inspect(&ctx, "queued-outside-edit-warm", "src/lib.rs");
    assert!(!reported_check_still_running(&warm), "{warm:#}");
    // The fake's own analysis always reports a test error and warning; only
    // a check of a file containing `fake_compile_error` adds this one.
    assert!(
        !diagnostic_messages_for(&warm, "src/lib.rs")
            .iter()
            .any(|message| message == "fake compile error"),
        "control: the file compiles before the outside edit: {warm:#}"
    );

    // Another program adds a compiler error.
    fs::write(&lib, "// fake_compile_error\npub fn f() {}\n").unwrap();
    let paused = queue_watcher_change_for_held_helper(&ctx, &lib);

    let after = scoped_diagnostics_inspect(&ctx, "queued-outside-edit-after", "src/lib.rs");
    drop(paused);
    if reported_check_still_running(&after) {
        return;
    }
    let text = after["text"].as_str().expect("rendered text");
    assert!(
        diagnostic_messages_for(&after, "src/lib.rs")
            .iter()
            .any(|message| message == "fake compile error"),
        "the compiler error of the outside edit is missing: {text}\n{after:#}"
    );
}

/// Forward the watcher's change to `path` while another thread holds the LSP
/// manager, so it is queued for the helper thread, and keep that helper from
/// delivering it until the returned guard is dropped.
fn queue_watcher_change_for_held_helper<'a>(
    ctx: &'a AppContext,
    path: &Path,
) -> parking_lot::MutexGuard<'a, ()> {
    let paused = ctx.pause_lsp_watcher_forwarding_for_test();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    thread::scope(|scope| {
        scope.spawn(move || {
            let _busy = ctx.lsp();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();
        ctx.lsp_forward_watcher_file_events(&[path.to_path_buf()]);
        release_tx.send(()).unwrap();
    });
    assert!(
        ctx.lsp_watcher_forward_pending_for_test(),
        "control: the change must still be queued"
    );
    paused
}

/// `lsp_diagnostics` reads the same check state as inspect, so a change
/// still queued for the held helper thread must not be answered from
/// rust-analyzer's results for the file before it: the answer reports the
/// compiler error of the check the change starts, or is incomplete with the
/// note that the check is still running.
#[test]
fn rust_lsp_diagnostics_forwards_a_queued_outside_edit_before_answering() {
    let (_temp_dir, root, lib) = single_crate_fixture("queued-outside-edit-lsp");
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "0");
    let lsp_diagnostics = |id: &str| -> Value {
        serde_json::to_value(aft::commands::lsp_diagnostics::handle_lsp_diagnostics(
            &request(json!({
                "id": id,
                "command": "lsp_diagnostics",
                "file": lib.display().to_string(),
                "wait_ms": 10_000,
            })),
            &ctx,
        ))
        .expect("response serializes")
    };
    let compiler_error_reported = |response: &Value| {
        response["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|diagnostic| diagnostic["message"] == "fake compile error")
    };
    let warm = lsp_diagnostics("queued-outside-edit-lsp-warm");
    assert_eq!(warm["success"], true, "{warm:#}");
    assert!(
        !compiler_error_reported(&warm),
        "control: the file compiles before the outside edit: {warm:#}"
    );

    // Another program adds a compiler error.
    fs::write(&lib, "// fake_compile_error\npub fn f() {}\n").unwrap();
    let paused = queue_watcher_change_for_held_helper(&ctx, &lib);

    let after = lsp_diagnostics("queued-outside-edit-lsp-after");
    drop(paused);
    assert_eq!(after["success"], true, "{after:#}");
    if after["complete"] == false && after["note"].as_str().is_some() {
        assert_eq!(
            after["note"], "rust-analyzer: cargo check still running; retry",
            "{after:#}"
        );
        return;
    }
    assert!(
        compiler_error_reported(&after),
        "the compiler error of the outside edit is missing: {after:#}"
    );
}

/// The opposite direction: a compiler error present when rust-analyzer
/// starts (its first `cargo check` reports it) is then fixed with an AFT
/// edit. `use of moved value` comes only from the compiler, never from
/// rust-analyzer's own analysis, so only a new check can clear it. Until
/// one ran, inspect certified the fixed file with the old error.
#[test]
fn scoped_rust_inspect_drops_a_fixed_compiler_error_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "scoped_rust_inspect_drops_a_fixed_compiler_error_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, _s) = field_removal_crate();
    let broken = "pub fn twice(v: Vec<u8>) -> (Vec<u8>, Vec<u8>) {\n    (v, v)\n}\n";
    let fixed = "pub fn twice(v: Vec<u8>) -> (Vec<u8>, Vec<u8>) {\n    (v.clone(), v)\n}\n";
    let moves = write_file(&root, "src/moves.rs", broken);
    write_file(
        &root,
        "src/lib.rs",
        "pub mod moves;\npub mod s;\npub mod user;\n",
    );
    let ctx = configured_context(&root);
    enable_automatic_rust_checks(&ctx);
    let moved_value = |response: &Value| {
        diagnostic_sources_for(response, "src/moves.rs")
            .iter()
            .any(|(_, message)| message.contains("moved value"))
    };
    let before = scoped_diagnostics_inspect(&ctx, "fixed-error-before", "src");
    assert!(
        moved_value(&before),
        "control: the first check reports the error: {before:#}"
    );

    let fix = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "fixed-error-edit",
            "command": "edit_match",
            "file": moves.display().to_string(),
            "match": broken,
            "replacement": fixed,
        })),
        &ctx,
    );
    assert!(fix.success, "{fix:?}");
    let after = scoped_diagnostics_inspect(&ctx, "fixed-error-after", "src/moves.rs");
    let text = after["text"].as_str().expect("rendered text");
    let still_checking = after["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|gap| gap["kind"] == "checking_producer");
    if still_checking {
        assert_eq!(after["complete"], false, "{after:#}");
        return;
    }
    assert!(
        !moved_value(&after),
        "the fixed error is still reported: {text}\n{after:#}"
    );
    assert!(
        text.contains("diagnostics: 0 errors"),
        "the fixed file must be reported clean: {text}"
    );
}

/// Whether a scoped inspect reported rust-analyzer's `cargo check` as still
/// running instead of certifying the files. Then the answer must be marked
/// incomplete and say that diagnostics are unknown.
fn reported_check_still_running(response: &Value) -> bool {
    let still_checking = response["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|gap| gap["kind"] == "checking_producer");
    if still_checking {
        assert_eq!(response["complete"], false, "{response:#}");
        let text = response["text"].as_str().expect("rendered text");
        assert!(
            text.starts_with(
                "PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo check still running"
            ),
            "{text}"
        );
    }
    still_checking
}

fn completed_rust_check_record(storage: &Path, expected_errors: u64) -> Option<PathBuf> {
    fs::read_dir(storage.join("rust-completed-checks"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .find(|path| {
            let Some(reports) = fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|record| record["record"]["diagnostics"].as_object().cloned())
            else {
                return false;
            };
            let errors = reports
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter(|diagnostic| diagnostic["severity"] == "Error")
                .count() as u64;
            errors == expected_errors
        })
}

fn wait_for_completed_rust_check(
    storage: &Path,
    expected_errors: u64,
    ready: impl FnOnce(),
) -> Result<PathBuf, String> {
    use notify::Watcher;

    let directory = storage.join("rust-completed-checks");
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if !matches!(&event, Ok(event) if event.kind.is_access()) {
            let _ = event_tx.send(event);
        }
    })
    .map_err(|error| format!("watch completed checks: {error}"))?;
    watcher
        .watch(&directory, notify::RecursiveMode::NonRecursive)
        .map_err(|error| format!("watch {}: {error}", directory.display()))?;
    ready();
    // Register before reading so the atomic rename cannot fall between the
    // absence check and the subscription. Events wake us; record contents prove
    // the expected check, not an older clean record, reached disk.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(record) = completed_rust_check_record(storage, expected_errors) {
            return Ok(record);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no completed check with {expected_errors} errors was persisted"
            ));
        }
        event_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| {
                format!("no completed check with {expected_errors} errors was persisted: {error}")
            })?
            .map_err(|error| format!("completed-check watch failed: {error}"))?;
    }
}

#[test]
fn rust_inspect_waits_for_gated_persistence_before_module_restart() {
    crate::helpers::disable_in_process_file_watcher();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    let storage = temp.path().join("storage");
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated-save\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(
        &root,
        "Cargo.lock",
        "version = 4\n[[package]]\nname = \"gated-save\"\nversion = \"0.1.0\"\n",
    );
    let new_context = || {
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        )
        .with_test_checkpoint_store();
        ctx.isolate_cold_build_limiter_for_test(2);
        let response = handle_configure(
            &request(json!({
                "id": "gated-configure", "command": "configure", "project_root": root,
                "storage_dir": storage, "harness": "opencode",
                "config": crate::helpers::user_config(json!({
                    "indexes": { "trigram": false, "semantic": false },
                    "inspect": { "diagnostics_timeout_ms": 40_000 }
                }))
            })),
            &ctx,
        );
        assert!(response.success, "{response:?}");
        ctx.lsp()
            .override_binary(ServerKind::Rust, fake_server_path());
        ctx
    };
    let directory = storage.join("rust-completed-checks");
    fs::create_dir_all(&directory).unwrap();
    for (source, expected_errors) in [
        ("pub fn answer() -> u8 { 42 }\n", 0),
        ("pub fn answer() -> u8 { unknown }\n", 1),
    ] {
        write_file(&root, "src/lib.rs", source);
        // Hold the writer's actual cross-process lock, so an authoritative in-memory
        // check cannot race ahead of the assertion about its persistence observer.
        let writer_gate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(".write-lock"))
            .unwrap();
        writer_gate.lock().unwrap();
        let ctx = new_context();
        let warm = scoped_diagnostics_inspect(&ctx, "gated-warm", "src");
        assert_eq!(
            warm["summary"]["diagnostics"]["errors"], expected_errors,
            "{warm:#}"
        );
        assert_eq!(
            warm["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
            "{warm:#}"
        );
        assert!(completed_rust_check_record(&storage, expected_errors).is_none());
        // The detached writer owns the snapshot and must finish even if the
        // module is torn down before its cache write completes.
        drop(ctx);
        thread::scope(|scope| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let storage = &storage;
            scope.spawn(move || {
                let result = wait_for_completed_rust_check(storage, expected_errors, || {
                    ready_tx.send(()).unwrap();
                });
                result_tx.send(result).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let result = result_rx.try_recv();
            assert!(
                matches!(result, Err(std::sync::mpsc::TryRecvError::Empty)),
                "persistence observer returned before the writer gate was released: {result:?}"
            );
            drop(writer_gate);
            let record = result_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(record.is_file());
        });
        let ctx = new_context();
        let cold = scoped_diagnostics_inspect(&ctx, "gated-cold", "src");
        assert_eq!(
            cold["summary"]["diagnostics"]["errors"], expected_errors,
            "{cold:#}"
        );
        assert!(
            cold["text"]
                .as_str()
                .unwrap()
                .contains("last completed check"),
            "{cold:#}"
        );
    }
}

#[test]
fn rust_inspect_restores_completed_check_after_module_restart_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "rust_inspect_restores_completed_check_after_module_restart_with_real_rust_analyzer",
    ) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    let storage = temp.path().join("storage");
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"saved-check\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&root, "src/lib.rs", "pub fn answer() -> u8 { 42 }\n");
    write_file(
        &root,
        "build.rs",
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
    );
    // Pre-create the lockfile: its creation during the first check changes an input.
    write_file(
        &root,
        "Cargo.lock",
        "version = 4\n[[package]]\nname = \"saved-check\"\nversion = \"0.1.0\"\n",
    );
    let new_context = || {
        crate::helpers::disable_in_process_file_watcher();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        )
        .with_test_checkpoint_store();
        ctx.isolate_cold_build_limiter_for_test(2);
        let response = handle_configure(
            &request(json!({
                "id": "saved-configure", "command": "configure", "project_root": root, "storage_dir": storage,
                "harness": "opencode", "config": crate::helpers::user_config(json!({
                    "indexes": { "trigram": false, "semantic": false },
                    "inspect": {"diagnostics_timeout_ms": 40_000}
                }))
            })),
            &ctx,
        );
        assert!(response.success, "{response:?}");
        ctx.lsp()
            .override_binary(ServerKind::Rust, fake_server_path());
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_PROXY", "rust-analyzer");
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_PROXY_CHECK_DELAY_MS", "45000");
        enable_automatic_rust_checks(&ctx);
        ctx
    };
    let ctx = new_context();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let warm = loop {
        let warm = scoped_diagnostics_inspect(&ctx, "saved-warm", "src");
        if warm["summary"]["diagnostics"]["errors"] == 0 {
            break warm;
        }
        assert!(std::time::Instant::now() < deadline, "{warm:#}");
    };
    assert_eq!(warm["summary"]["diagnostics"]["errors"], 0, "{warm:#}");
    // Inspect certifies the live result before its detached cache writer finishes.
    // A restart can restore only the record whose atomic write has completed.
    let record = wait_for_completed_rust_check(&storage, 0, || {}).unwrap();
    drop(ctx);
    let ctx = new_context();
    let started = std::time::Instant::now();
    let cold = scoped_diagnostics_inspect(&ctx, "saved-cold", "src");
    assert!(
        cold["text"]
            .as_str()
            .unwrap()
            .contains("last completed check"),
        "{cold:#}"
    );
    assert_eq!(cold["summary"]["diagnostics"]["errors"], 0, "{cold:#}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    println!(
        "real restart: cold inspect {} ms; record {} bytes at {}",
        started.elapsed().as_millis(),
        fs::metadata(&record).unwrap().len(),
        record.display()
    );
    // A cold request after an edit cannot reuse the previous clean check.
    drop(ctx);
    write_file(&root, "src/lib.rs", "pub fn answer() -> u8 { unknown }\n");
    let ctx = new_context();
    let config = ctx.config();
    let file = root.join("src/lib.rs");
    crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_server_for_file(&file, &config)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        ctx.lsp().drain_events();
        if ctx
            .lsp()
            .client_for_file(&file, &config)
            .is_some_and(|client| !client.diagnostics_are_provisional())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real analyzer did not finish indexing"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let changed = scoped_diagnostics_inspect(&ctx, "saved-changed", "src");
    assert!(
        !changed["text"]
            .as_str()
            .unwrap()
            .contains("no Rust inputs changed"),
        "{changed:#}"
    );
    assert!(reported_check_still_running(&changed), "{changed:#}");
    let errors = loop {
        let response = scoped_diagnostics_inspect(&ctx, "saved-errors-warm", "src");
        if response["summary"]["diagnostics"]["errors"]
            .as_u64()
            .is_some_and(|n| n > 0)
        {
            break response;
        }
        assert!(std::time::Instant::now() < deadline, "{response:#}");
    };
    wait_for_completed_rust_check(
        &storage,
        errors["summary"]["diagnostics"]["errors"].as_u64().unwrap(),
        || {},
    )
    .unwrap();
    drop(ctx);
    let ctx = new_context();
    let restored_errors = scoped_diagnostics_inspect(&ctx, "saved-errors-cold", "src");
    assert!(
        restored_errors["text"]
            .as_str()
            .unwrap()
            .contains("last completed check"),
        "{restored_errors:#}"
    );
    assert_eq!(
        restored_errors["summary"]["diagnostics"]["errors"],
        errors["summary"]["diagnostics"]["errors"],
        "{restored_errors:#}"
    );
}

fn assert_unsuccessful_rust_check_is_not_persisted(message: &str) {
    crate::helpers::disable_in_process_file_watcher();
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let records = || {
        fs::read_dir(storage.join("rust-completed-checks"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .count()
    };
    for (index, result) in [None, Some(message)].into_iter().enumerate() {
        let root = temp.path().join(format!("checkout-{index}"));
        write_file(
            &root,
            "Cargo.toml",
            "[package]\nname = \"saved-fake-check\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write_file(&root, "src/lib.rs", "pub fn f() {}\n");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        )
        .with_test_checkpoint_store();
        ctx.isolate_cold_build_limiter_for_test(2);
        let response = handle_configure(
            &request(
                json!({"id":"unsuccessful-configure", "command":"configure", "harness":"opencode", "project_root":root,"storage_dir":storage,
            "config":crate::helpers::user_config(json!({"indexes":{"trigram":false,"semantic":false},"inspect":{"diagnostics_timeout_ms":40_000}}))}),
            ),
            &ctx,
        );
        assert!(response.success, "{response:?}");
        configure_fake_rust_lsp(&ctx);
        // Default fake startup is already quiescent; its check therefore begins
        // after workspace load. The warming variant settles only on didOpen,
        // which would leave a second compiler run owed after its first run.
        // The successful control must begin after its freshly created inputs
        // are outside the completed-check timestamp ambiguity window.
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "3000");
        if let Some(message) = result {
            ctx.lsp()
                .set_extra_env("AFT_FAKE_LSP_CHECK_END_MESSAGE", message);
        }
        let response = scoped_diagnostics_inspect(&ctx, "unsuccessful-check", "src");
        assert_eq!(
            response["summary"]["diagnostics"]["errors"], 1,
            "{response:#}"
        );
        if result.is_none() {
            // The production library persists certified checks on a detached
            // writer. Observe the successful control's record before using it
            // as the baseline for the unsuccessful run.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while records() == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(
            records(),
            1,
            "control writes one record, but the {result:?} run must not write another"
        );
    }
}

#[test]
fn cancelled_rust_check_is_never_saved() {
    assert_unsuccessful_rust_check_is_not_persisted("cancelled");
}

#[test]
fn failed_rust_check_is_never_saved() {
    assert_unsuccessful_rust_check_is_not_persisted("failed");
}

/// rust-analyzer starts its first `cargo check` just after it reports
/// quiescence, but a server short of CPU (several of them sharing a CI
/// runner) can announce that check seconds later. A relay in front of the
/// real server holds the first check's announcement, and every diagnostics
/// report, back for three seconds while requests are still answered. The
/// compiler error (`use of moved value` comes only from the compiler) must be
/// reported, or the check named as still running; certifying the file from
/// rust-analyzer's own analysis alone is the stale answer this guards against.
#[test]
fn scoped_rust_inspect_waits_for_a_late_first_cargo_check_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "scoped_rust_inspect_waits_for_a_late_first_cargo_check_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, _s) = field_removal_crate();
    write_file(
        &root,
        "src/moves.rs",
        "pub fn twice(v: Vec<u8>) -> (Vec<u8>, Vec<u8>) {\n    (v, v)\n}\n",
    );
    write_file(
        &root,
        "src/lib.rs",
        "pub mod moves;\npub mod s;\npub mod user;\n",
    );
    let ctx = configured_context(&root);
    enable_automatic_rust_checks(&ctx);
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PROXY", "rust-analyzer");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PROXY_CHECK_DELAY_MS", "3000");

    let response = scoped_diagnostics_inspect(&ctx, "late-first-check", "src");
    if reported_check_still_running(&response) {
        return;
    }
    assert!(
        diagnostic_sources_for(&response, "src/moves.rs")
            .iter()
            .any(|(_, message)| message.contains("moved value")),
        "the compiler error of the late check is missing: {response:#}"
    );
}

/// The same late start against the fake, which emulates rust-analyzer's
/// check on save: it begins its first check three seconds after reporting
/// quiescence, longer than inspect used to wait for one, and the check finds
/// a compiler error. The inspect must wait for it and report the error.
///
/// These fake-check tests give inspect a 40 s budget: the scoped sweep gets
/// about half of it, which must outlast the waits they exercise (up to the
/// ten seconds rust-analyzer gets to begin its first check).
#[test]
fn scoped_rust_inspect_waits_for_a_first_cargo_check_that_begins_late() {
    let (_temp_dir, root, _lib) = single_crate_fixture("late-first-check");
    write_file(
        &root,
        "src/lib.rs",
        "// fake_compile_error\npub fn f() {}\n",
    );
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "3000");

    let response = scoped_diagnostics_inspect(&ctx, "late-first-check", "src/lib.rs");

    assert!(
        !reported_check_still_running(&response),
        "the check begins well within the budget: {response:#}"
    );
    assert!(
        diagnostic_messages_for(&response, "src/lib.rs")
            .iter()
            .any(|message| message == "fake compile error"),
        "the compiler error of the late check is missing: {response:#}"
    );
}

/// A save whose check never begins (the fake drops checks asked for by a
/// save) leaves the published compiler results describing the file before
/// the edit. Once inspect stops waiting for that check it must say the
/// diagnostics are unknown, not certify the old results.
#[test]
fn scoped_rust_inspect_never_certifies_a_save_that_started_no_check() {
    let (_temp_dir, root, lib) = single_crate_fixture("save-without-check");
    write_file(
        &root,
        "src/lib.rs",
        "// fake_compile_error\npub fn f() {}\n",
    );
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "0");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_DROP", "save");

    let before = scoped_diagnostics_inspect(&ctx, "save-without-check-before", "src/lib.rs");
    assert!(!reported_check_still_running(&before), "{before:#}");
    assert!(
        diagnostic_messages_for(&before, "src/lib.rs")
            .iter()
            .any(|message| message == "fake compile error"),
        "control: the first check reports the error: {before:#}"
    );

    let fix = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "save-without-check-fix",
            "command": "edit_match",
            "file": lib.display().to_string(),
            "match": "// fake_compile_error\n",
            "replacement": "",
        })),
        &ctx,
    );
    assert!(fix.success, "{fix:?}");

    let after = scoped_diagnostics_inspect(&ctx, "save-without-check-after", "src/lib.rs");
    assert!(
        reported_check_still_running(&after),
        "no check ran after the save, so the result must be unknown: {after:#}"
    );
}

/// rust-analyzer's events wait in AFT's queue until a request drains them. A
/// check that began before an edit's save, for the files before the edit, can
/// therefore be drained only after the save was sent; it must not count as
/// the save's check. Here the fake's first check runs (and finds the error)
/// while nothing drains its events, the error is then fixed by an edit whose
/// save starts no check, and inspect must report the result as unknown
/// instead of certifying the old check.
#[test]
fn scoped_rust_inspect_does_not_take_a_check_announced_before_a_save_for_the_save() {
    let (_temp_dir, root, lib) = single_crate_fixture("check-before-save");
    write_file(
        &root,
        "src/lib.rs",
        "// fake_compile_error\npub fn f() {}\n",
    );
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "1000");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_DROP", "save");

    let completed = root.join("initial-check-completed");
    ctx.lsp().set_extra_env(
        "AFT_FAKE_LSP_CHECK_COMPLETED_SIGNAL",
        completed.to_str().unwrap(),
    );
    // Keep the first check queued until after the edit. A server marker and
    // a request round trip prove it ran and the reader queued its events,
    // without draining those events through the manager before the save.
    let config = ctx.config().clone();
    let started = crate::test_helpers::settled_lsp(&mut ctx.lsp(), |startup_manager| {
        startup_manager.ensure_server_for_file(&lib, &config)
    });
    assert!(!started.is_empty(), "the fake rust server must start");
    wait_for_path_event(&completed, "initial check completed");
    ctx.lsp()
        .client_for_file_mut(&lib, &config)
        .unwrap()
        .send_request_with_timeout::<lsp_types::request::HoverRequest>(
            serde_json::from_value(json!({
                "textDocument": { "uri": file_uri(&lib) },
                "position": { "line": 0, "character": 0 },
            }))
            .unwrap(),
            Duration::from_secs(30),
        )
        .expect("reader barrier after initial check");

    let fix = aft::commands::edit_match::handle_edit_match(
        &request(json!({
            "id": "check-before-save-fix",
            "command": "edit_match",
            "file": lib.display().to_string(),
            "match": "// fake_compile_error\n",
            "replacement": "",
        })),
        &ctx,
    );
    assert!(fix.success, "{fix:?}");

    let after = scoped_diagnostics_inspect(&ctx, "check-before-save-after", "src/lib.rs");
    assert!(
        reported_check_still_running(&after),
        "only a check announced before the save ran, so the result must be unknown: {after:#}"
    );
}

/// rust-analyzer owes a check of the whole workspace once it is quiescent.
/// When that check never begins (the fake drops it), inspect stops waiting
/// at a deadline and reports the diagnostics as unknown instead of hanging
/// or certifying them. The next inspect asks for the check again
/// (`rust-analyzer/runFlycheck`), which the fake runs, and reports its
/// compiler error.
#[test]
fn scoped_rust_inspect_reports_an_unstarted_first_check_as_unknown_then_asks_again() {
    let (_temp_dir, root, _lib) = single_crate_fixture("first-check-dropped");
    write_file(
        &root,
        "src/lib.rs",
        "// fake_compile_error\npub fn f() {}\n",
    );
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_ON_SAVE", "0");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_CHECK_DROP", "load");

    let first = scoped_diagnostics_inspect(&ctx, "first-check-dropped", "src/lib.rs");
    assert!(
        reported_check_still_running(&first),
        "no check has run, so the result must be unknown: {first:#}"
    );

    let retried = scoped_diagnostics_inspect(&ctx, "first-check-retried", "src/lib.rs");
    assert!(!reported_check_still_running(&retried), "{retried:#}");
    assert!(
        diagnostic_messages_for(&retried, "src/lib.rs")
            .iter()
            .any(|message| message == "fake compile error"),
        "the check asked for again reports the error: {retried:#}"
    );
}

/// An unscoped inspect makes a whole-project claim, so it too must wait for
/// rust-analyzer's `cargo check`. The fake's check never finishes: the
/// answer names the running check instead of certifying the published
/// reports, and does not call the producer failed.
#[test]
fn unscoped_inspect_names_an_unfinished_cargo_check() {
    let (_temp_dir, root, _lib) = single_crate_fixture("unscoped-flycheck-slow");
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    // Enable only diagnostics so unrelated cold scanners cannot use the
    // inspection request budget and crowd the unfinished-check cause out
    // of the bounded status line.
    ctx.update_config(|config| {
        config.inspect.categories = aft::config::InspectCategories {
            diagnostics: true,
            todos: false,
            dead_code: false,
            unused_exports: false,
            duplicates: false,
            cycles: false,
            complexity: false,
        };
    });
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_FLYCHECK", "never");
    // Hold begin after the already-flushed quiescent status to expose the
    // idle Current observation without relying on machine contention.
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_FLYCHECK_BEGIN_DELAY_MS", "500");

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-unscoped-flycheck-slow",
            "command": "inspect",
            "sections": "diagnostics",
        }),
    );

    assert_eq!(response["complete"], false, "response: {response:#}");
    let gaps: Vec<&Value> = response["gaps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|gap| gap["producer"] == "rust")
        .collect();
    assert_eq!(gaps.len(), 1, "response: {response:#}");
    assert_eq!(gaps[0]["kind"], "checking_producer", "{response:#}");
    assert!(
        response["summary"]["diagnostics"]["errors"].is_null(),
        "{response:#}"
    );
    let text = response["text"].as_str().expect("rendered text");
    // The unscoped status line must name why Rust diagnostics are unknown.
    assert!(
        text.lines()
            .next()
            .is_some_and(|line| line.starts_with("PARTIAL — ")
                && line
                    .contains("diagnostics unknown: rust-analyzer @ .: cargo check still running")
                && line.ends_with("; retry aft_inspect.")),
        "{text}"
    );
    assert!(!text.contains("producer rust failed"), "{text}");
    assert!(!text.contains("unknown producer"), "{text}");
    assert!(response["summary"]["diagnostics"]["by_producer"]
        .as_object()
        .expect("producer counts")
        .contains_key("rust"));
}

/// With no opened files, a clean compiler check may publish no reports.
/// Its matching check-begin and check-end events still certify zero crate errors.
#[test]
fn unscoped_rust_inspect_certifies_a_clean_check_without_reports_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "unscoped_rust_inspect_certifies_a_clean_check_without_reports_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, _lib) = single_crate_fixture("unscoped-real-clean");
    generate_fixture_lockfile(&root);
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "unscoped-real-clean", "command": "inspect", "sections": "diagnostics"
        }),
    );
    eprintln!("real Rust clean unscoped: {response:#}");
    assert!(
        !ctx.lsp().has_any_diagnostic_reports(),
        "the clean zero-report authority path must execute"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 0,
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["rust"]["errors"], 0,
        "{response:#}"
    );
    assert_ne!(
        response["summary"]["diagnostics"]["complete"], false,
        "{response:#}"
    );
    assert_ne!(response["complete"], false, "{response:#}");
}

/// Unscoped inspect must collect a compiler error without relying on didOpen
/// diagnostics for the broken source file.
#[test]
fn unscoped_rust_inspect_reports_a_compile_error_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "unscoped_rust_inspect_reports_a_compile_error_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, lib) = single_crate_fixture("unscoped-real-error");
    fs::write(
        &lib,
        "pub fn twice(v: Vec<u8>) -> (Vec<u8>, Vec<u8>) { (v, v) }\n",
    )
    .unwrap();
    generate_fixture_lockfile(&root);
    let ctx = configured_context_with_callgraph_store(&root, true);
    tier2_run(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "unscoped-real-error", "command": "inspect", "sections": "diagnostics"
        }),
    );
    eprintln!("real Rust compile error unscoped: {response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert!(
        diagnostic_sources_for(&response, "src/lib.rs")
            .iter()
            // Explicit inspect labels its compiler reports separately from LSP reports.
            .any(|(source, message)| {
                (source == "rustc" || source == "cargo check (aft_inspect)")
                    && message.contains("moved value")
            }),
        "{response:#}"
    );
}

fn generate_fixture_lockfile(root: &Path) {
    let generated = std::process::Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(root)
        .output()
        .expect("cargo is required for real Rust fixtures");
    assert!(generated.status.success(), "{generated:?}");
}

#[test]
fn rust_analyzer_warning_keeps_diagnostics_and_reports_note() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-warning\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "warning");
    open_with_lsp(&ctx, &file, "fn main() {}\n");

    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "inspect-analyzer-warning",
            "command": "inspect",
            "scope": "src/main.rs",
            "sections": ["diagnostics"],
        }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    // Scoped inspects report Tier-2 categories as not computed, so only the
    // diagnostics category is expected to be complete here.
    assert!(
        response["summary"]["diagnostics"].get("complete").is_none(),
        "a rust-analyzer warning must not make diagnostics incomplete: {response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["warnings"], 1,
        "{response:#}"
    );
    assert_eq!(
        response["details"]["diagnostics"].as_array().unwrap().len(),
        2,
        "{response:#}"
    );
    let note = "rust-analyzer warning: proc-macro server failed to start";
    assert_eq!(
        response["summary"]["diagnostics"]["notes"],
        json!([note]),
        "{response:#}"
    );
    assert!(
        response["text"].as_str().unwrap().contains(note),
        "{response:#}"
    );
}

#[test]
fn blocking_inspect_keeps_provisional_scoped_diagnostics_on_indexing_timeout() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"partial-diag\"\n");
    let file = write_file(&root, "src/main.rs", "// TODO: finish\nfn main() {}\n");
    let ctx = configured_context_with_diagnostics_timeout(&root, 10_000);
    open_with_server_status_mode(&ctx, &file, "1");
    wait_for_lsp_report_state(&ctx, &file, true);
    let response = serde_json::to_value(handle_inspect_tool_call(
        &request(json!({
            "id": "inspect-partial-published", "command": "inspect", "scope": "src/main.rs", "sections": ["diagnostics", "todos"]
        })),
        &ctx,
    ))
    .unwrap();
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], false);
    // The TODO scan shares the remaining budget. On a slow runner (Windows CI)
    // it can run out too, which must surface as a named gap rather than a
    // silent zero; either outcome is honest.
    let todos = &response["summary"]["todos"];
    let todos_counted = todos["count"].as_u64().is_some_and(|n| n > 0);
    let todos_gap_named = todos["gaps"]
        .as_array()
        .is_some_and(|gaps| gaps.iter().any(|gap| gap["kind"] == "analysis_incomplete"));
    assert!(todos_counted || todos_gap_named, "{response:#}");
    let details = response["details"]["diagnostics"].as_array().unwrap();
    assert!(
        details.iter().any(|item| item["complete"] == false
            && item["message"]
                .as_str()
                .unwrap()
                .contains("analyzer warming")),
        "{response:#}"
    );
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
    assert_eq!(ctx.status_bar_count_values().errors, None);
}
