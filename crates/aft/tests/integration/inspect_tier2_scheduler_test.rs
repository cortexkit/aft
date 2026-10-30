use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use aft::callgraph_store::CallGraphStore;
use aft::commands::configure::handle_configure;
use aft::commands::inspect::handle_inspect_tier2_run;
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::inspect::tier2_scheduler::{TIER2_REFRESH_COLD_CACHE_DELAY, TIER2_REFRESH_DEBOUNCE};
use aft::inspect::{InspectCache, InspectCategory, InspectSnapshot, Tier2TriggerReason};
use aft::lsp::registry::ServerKind;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::runtime_drain::{drain_watcher_events_bounded, WATCHER_PATH_DRAIN_BATCH_CAP};
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

fn fixture_project() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");
    (temp_dir, root)
}

fn write_file(root: &Path, relative_path: &str, contents: &str) -> PathBuf {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(&path, contents).expect("write fixture file");
    path
}

fn request(payload: Value) -> RawRequest {
    serde_json::from_value(payload).expect("request parses")
}

fn configured_context(root: &Path) -> AppContext {
    configured_context_with_storage(root, &root.join(".aft-test-storage"), true)
}

fn configured_context_with_storage(
    root: &Path,
    storage_dir: &Path,
    callgraph_store: bool,
) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.to_path_buf()),
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": callgraph_store }
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    // Every test in this file asserts scheduler decisions (queued vs deferred
    // categories), and the deferral branch fires whenever the process-wide
    // ColdBuildLimiter's slots happen to be held by PARALLEL NEIGHBOR tests -
    // manual_dead_code_query_without_callgraph_store_reports_unavailable failed
    // 3/3 under the full parallel suite and passed isolated on the same tree
    // (2026-08-17). Give each context its own capacity so assertions observe
    // this file's scheduling decisions, not the suite's load (issue #1190's
    // isolation seam).
    ctx.isolate_cold_build_limiter_for_test(2);
    // These fixtures exercise scheduler behavior rather than LSP startup. Seed a
    // checked-clean report so blocking inspect can require a complete diagnostics
    // prerequisite without changing the scheduler subject under test.
    ctx.lsp()
        .diagnostics_store_mut_for_test()
        .publish_with_kind(
            ServerKind::Rust,
            root.join(".aft-test-authoritative-diagnostics"),
            Vec::new(),
        );
    if callgraph_store {
        ensure_callgraph_store_ready(&ctx);
    }
    ctx
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let worktree_root = temp_dir.path().join("linked-worktree");
    let storage_dir = temp_dir.path().join("storage");
    fs::create_dir_all(&root).expect("create project root");
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    write_file(
        &root,
        "src/copy.ts",
        "export function unusedCopy() { return 1; }\n",
    );
    git(&root, &["init"]);
    git(&root, &["config", "user.name", "AFT Test"]);
    git(&root, &["config", "user.email", "aft-test@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "fixture"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "linked-inspect-test",
            worktree_root.to_str().expect("UTF-8 worktree path"),
        ],
    );
    (temp_dir, root, worktree_root, storage_dir)
}

fn tier2_aggregate_bytes(ctx: &AppContext, root: &Path) -> BTreeMap<String, Vec<u8>> {
    let cache = InspectCache::open_readonly(ctx.inspect_dir(), root.to_path_buf())
        .expect("open inspect cache")
        .expect("inspect cache exists");
    [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
        InspectCategory::Cycles,
    ]
    .into_iter()
    .map(|category| {
        let aggregate = cache
            .latest_aggregate_any_hash(category)
            .expect("read Tier-2 aggregate")
            .expect("Tier-2 aggregate exists");
        (
            category.as_str().to_string(),
            serde_json::to_vec(&aggregate).expect("serialize aggregate"),
        )
    })
    .collect()
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
    // A cold call-graph build on a loaded CI runner (Windows especially) can
    // take well over 10 s; a build that never finishes still fails here.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match ctx.callgraph_store_for_ops() {
            // A published store is served while its build is still settling,
            // and Tier-2 dispatch waits for that build to finish. These tests
            // start from a finished build, so keep draining until the build's
            // completion has been adopted.
            CallgraphStoreAccess::Ready(_) if !ctx.callgraph_cold_build_active() => return,
            CallgraphStoreAccess::Ready(_) | CallgraphStoreAccess::Building => {
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

fn inspect(ctx: &AppContext) -> Value {
    crate::helpers::inspect_reasking_tier1_deadline(
        ctx,
        json!({
            "id": "inspect",
            "command": "inspect",
        }),
    )
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
    serde_json::to_value(response).expect("Tier-2 run response serializes")
}

fn automatic_tier2_category_names(ctx: &AppContext) -> Vec<&'static str> {
    ctx.automatic_tier2_refresh_categories_for_test()
        .into_iter()
        .map(InspectCategory::as_str)
        .collect()
}

fn wait_for_tier2(ctx: &AppContext, categories: &[&str]) -> Value {
    let response = inspect(ctx);
    assert_eq!(
        response["success"], true,
        "inspect should wait for fresh Tier-2 categories {categories:?}: {response:#}"
    );
    response
}

#[test]
fn watcher_tick_after_quiet_gap_triggers_tier2_refresh() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);

    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 1),
        None
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + TIER2_REFRESH_COLD_CACHE_DELAY, 0),
        Some(Tier2TriggerReason::Debounce)
    );

    let response = wait_for_tier2(
        &ctx,
        &[
            "dead_code",
            "unused_exports",
            "duplicates",
            "cycles",
            "complexity",
        ],
    );
    assert_eq!(
        response["scanner_state"]["tier2_trigger_reason"].as_str(),
        Some("debounce"),
        "inspect should expose the watcher debounce trigger reason: {response:#}"
    );
}

#[test]
fn automatic_refresh_without_callgraph_store_skips_dead_code_only() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context_with_storage(&root, &root.join(".aft-test-storage"), false);
    let expected = vec!["unused_exports", "duplicates", "cycles", "complexity"];

    assert_eq!(automatic_tier2_category_names(&ctx), expected);

    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 1),
        None
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + TIER2_REFRESH_COLD_CACHE_DELAY, 0),
        Some(Tier2TriggerReason::Debounce)
    );
    assert_eq!(
        ctx.inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        expected.len() as u64,
        "automatic scheduling should submit every callgraph-independent Tier-2 category"
    );
}

#[test]
fn manual_dead_code_query_without_callgraph_store_reports_unavailable() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context_with_storage(&root, &root.join(".aft-test-storage"), false);

    let queued = enqueue_tier2_run(&ctx, &["dead_code"]);
    assert_eq!(
        queued["success"], true,
        "manual Tier-2 run failed: {queued:#}"
    );
    assert_eq!(queued["queued_categories"], json!(["dead_code"]));

    let response = wait_for_tier2(&ctx, &["dead_code"]);
    let dead_code = response["summary"]["dead_code"]
        .as_object()
        .expect("dead_code summary");
    assert_eq!(
        dead_code
            .get("callgraph_available")
            .and_then(Value::as_bool),
        Some(false),
        "manual dead_code must disclose the unavailable callgraph: {response:#}"
    );
    assert!(
        !dead_code.contains_key("count"),
        "unavailable dead_code must not be represented as zero: {response:#}"
    );
}

#[test]
fn automatic_refresh_reincludes_dead_code_after_callgraph_store_reconfigure() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let storage_dir = root.join(".aft-test-storage");
    let ctx = configured_context_with_storage(&root, &storage_dir, false);

    let response = handle_configure(
        &request(json!({
            "id": "reconfigure-callgraph-on",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "storage_dir": storage_dir.to_string_lossy(),
            "config": crate::helpers::user_config(serde_json::json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": true }
            })),
        })),
        &ctx,
    );
    let response = serde_json::to_value(response).expect("reconfigure response serializes");
    assert_eq!(
        response["success"], true,
        "reconfigure failed: {response:#}"
    );
    assert_eq!(
        automatic_tier2_category_names(&ctx),
        vec![
            "dead_code",
            "unused_exports",
            "duplicates",
            "cycles",
            "complexity",
        ]
    );

    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 1),
        None
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + TIER2_REFRESH_COLD_CACHE_DELAY, 0),
        Some(Tier2TriggerReason::Debounce)
    );
    assert_eq!(
        ctx.inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        5,
        "the next automatic refresh should include dead_code and complexity after reconfigure"
    );
}

#[test]
fn direct_inspect_mid_watcher_quiet_window_computes_immediately() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
    ctx.reset_tier2_refresh_scheduler_at(base);
    assert_eq!(ctx.tick_tier2_refresh_scheduler_at(change, 1), None);

    let response = inspect(&ctx);

    assert_eq!(
        response["success"], true,
        "direct inspect should compute fresh Tier-2 data during the watcher quiet window: {response:#}"
    );
    assert_eq!(
        ctx.inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        0,
        "the direct inspect path must not wait for or enqueue an automatic refresh"
    );
}

#[test]
fn direct_inspect_cold_tier2_computes_without_scheduler_pull() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);

    let response = inspect(&ctx);

    assert_eq!(
        response["success"], true,
        "direct inspect should return fresh Tier-2 data without a scheduler pull: {response:#}"
    );
    assert!(
        !ctx.tier2_pull_demand_pending(),
        "fresh direct inspect should not leave a scheduler pull demand"
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 0),
        None
    );
}

#[test]
fn linked_worktree_skips_automatic_tier2_and_leaves_parent_gate_open() {
    let (_temp_dir, root, worktree_root, storage_dir) = linked_worktree_fixture();
    let parent_ctx = configured_context_with_storage(&root, &storage_dir, false);
    let worktree_ctx = configured_context_with_storage(&worktree_root, &storage_dir, false);

    assert!(!parent_ctx.is_worktree_bridge());
    assert!(worktree_ctx.is_worktree_bridge());
    assert!(
        worktree_ctx.build_status_snapshot()["status_bar"].is_null(),
        "an unscanned worktree must not fabricate Tier-2 status counts"
    );

    let worktree_base = Instant::now();
    worktree_ctx.reset_tier2_refresh_scheduler_at(worktree_base);
    assert!(!worktree_ctx.request_tier2_refresh_pull());
    assert_eq!(
        worktree_ctx
            .tick_tier2_refresh_scheduler_at(worktree_base + TIER2_REFRESH_COLD_CACHE_DELAY, 0,),
        None
    );
    let manager_submission = worktree_ctx
        .inspect_manager()
        .submit_tier2_run_with_reuse_background(
            InspectSnapshot::new_with_capabilities(
                worktree_root.clone(),
                worktree_ctx.inspect_dir(),
                worktree_ctx.config(),
                worktree_ctx.symbol_cache(),
                true,
                false,
            ),
            InspectCategory::Duplicates,
        )
        .expect("manager worktree gate is not an error");
    assert!(manager_submission.is_none());

    let warm_response = serde_json::to_value(handle_inspect_tier2_run(
        &request(json!({
            "id": "worktree-tier2-warm",
            "command": "inspect_tier2_run",
            "categories": ["dead_code", "unused_exports", "duplicates", "cycles"],
        })),
        &worktree_ctx,
    ))
    .expect("Tier-2 warm response serializes");
    assert_eq!(warm_response["success"], true);
    assert_eq!(warm_response["queued_categories"], json!([]));
    assert_eq!(
        worktree_ctx
            .inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        0,
        "no automatic Tier-2 scheduling path may reach the manager for a linked worktree"
    );

    assert!(
        parent_ctx
            .inspect_manager()
            .automatic_tier2_refresh_allowed(),
        "linked-worktree detection must not close the parent root's scheduling gate"
    );
}

#[test]
fn linked_worktree_explicit_inspect_keeps_parent_aggregates_byte_identical() {
    let (_temp_dir, root, worktree_root, storage_dir) = linked_worktree_fixture();
    let parent_ctx = configured_context_with_storage(&root, &storage_dir, false);
    let worktree_ctx = configured_context_with_storage(&worktree_root, &storage_dir, false);

    let parent_response = inspect(&parent_ctx);
    assert_eq!(
        parent_response["success"], true,
        "parent inspect failed: {parent_response:#}"
    );
    let parent_before = tier2_aggregate_bytes(&parent_ctx, &root);
    assert!(
        worktree_ctx.build_status_snapshot()["status_bar"].is_null(),
        "worktree status must remain absent until explicit demand produces real counts"
    );

    let worktree_response = inspect(&worktree_ctx);
    assert_eq!(
        worktree_response["success"], true,
        "worktree inspect failed: {worktree_response:#}"
    );
    assert!(
        worktree_response["summary"]["unused_exports"]["count"].is_number(),
        "explicit worktree inspect should return computed Tier-2 data: {worktree_response:#}"
    );

    let parent_after = tier2_aggregate_bytes(&parent_ctx, &root);
    assert_eq!(
        parent_after, parent_before,
        "a worktree demand scan must not alter any parent Tier-2 aggregate bytes"
    );
    assert_ne!(
        parent_ctx.inspect_dir(),
        worktree_ctx.inspect_dir(),
        "absolute root identity must keep parent and worktree inspect generations separate"
    );
    let parent_cache = InspectCache::open_readonly(parent_ctx.inspect_dir(), root)
        .expect("open parent inspect cache")
        .expect("parent inspect cache exists");
    let worktree_cache = InspectCache::open_readonly(worktree_ctx.inspect_dir(), worktree_root)
        .expect("open worktree inspect cache")
        .expect("worktree inspect cache exists");
    assert_ne!(parent_cache.project_key(), worktree_cache.project_key());
}

/// A root that falls quiet after its last edit must still get the Tier-2
/// refresh its scheduler promised.
///
/// The refresh scheduler is ticked only from the watcher drain, and every tick
/// that arrives with paths in hand restarts the debounce window it is measured
/// against. The drain pass that finds an empty queue IS the quiet window, so it
/// is the only place a deadline that came due in the silence can be observed.
/// Without that pass the refresh waits for the next file change — which starts
/// the wait over — and health keeps publishing a deadline nothing will honour.
#[test]
fn quiet_watcher_drain_dispatches_a_refresh_that_came_due_in_the_silence() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);

    // A live daemon root always has a watcher receiver installed. Without one
    // the drain takes its no-watcher shortcut, which is not the path a real
    // root runs.
    let (_watcher_tx, watcher_rx) = crossbeam_channel::unbounded::<WatcherDispatchEvent>();
    *ctx.watcher_rx().lock() = Some(watcher_rx);

    // Configure, one watcher batch, then silence: the scheduler state of a root
    // whose last edit is older than the debounce window.
    let configured_at = Instant::now()
        .checked_sub(
            TIER2_REFRESH_COLD_CACHE_DELAY + TIER2_REFRESH_DEBOUNCE + Duration::from_secs(5),
        )
        .expect("monotonic clock is older than the scheduler's windows");
    ctx.reset_tier2_refresh_scheduler_at(configured_at);
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(configured_at + Duration::from_secs(1), 3),
        None,
        "a tick that carries changes restarts the debounce instead of dispatching"
    );
    assert_eq!(
        ctx.tier2_trigger_reason(),
        None,
        "no refresh has dispatched yet"
    );
    assert!(
        ctx.tier2_refresh_dispatch_due(),
        "the deadline health publishes as next_refresh_at_ms has already passed"
    );

    let outcome = drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

    assert!(!outcome.has_more, "the watcher queue is empty");
    assert_eq!(
        ctx.tier2_trigger_reason(),
        Some("debounce"),
        "a drain pass over an empty queue must dispatch the overdue refresh"
    );
    assert_eq!(
        ctx.inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        5,
        "the dispatched refresh should cover every automatic Tier-2 category"
    );
    assert!(
        !ctx.tier2_refresh_dispatch_due(),
        "a dispatched refresh clears the deadline until the min interval elapses"
    );
}

/// A failed Tier-2 build pauses automatic refreshes of its category, but a
/// change to an input file ends the pause so a fix is picked up by the next
/// automatic refresh. A change the scope walk can never include does not, and
/// an explicit `aft_inspect` is never held back by the pause.
#[cfg(unix)]
#[test]
fn failed_tier2_build_pause_ends_on_in_scope_change_and_never_blocks_inspect() {
    use std::os::unix::fs::PermissionsExt;

    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let manager = ctx.inspect_manager();
    // An unreadable source fails the complexity build on every attempt until
    // it is fixed, standing in for any cause the user has to repair.
    let locked = write_file(&root, "src/locked.ts", "export const locked = 1;\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    if fs::read(&locked).is_ok() {
        // Running as a user that ignores file modes; nothing can fail.
        return;
    }
    let wait_idle = || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while manager.tier2_any_in_flight() {
            assert!(Instant::now() < deadline, "background build did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    };
    let paused = |response: &Value| {
        response["skipped_categories"]
            .as_array()
            .is_some_and(|skipped| {
                skipped.iter().any(|entry| {
                    entry["category"] == "complexity" && entry["reason"] == "last_build_failed"
                })
            })
    };

    let first = enqueue_tier2_run(&ctx, &["complexity"]);
    assert_eq!(
        first["queued_categories"],
        json!(["complexity"]),
        "{first:#}"
    );
    wait_idle();
    let starts_after_failure = manager.reuse_start_count_for_test();
    assert!(
        manager
            .tier2_retry_pause(InspectCategory::Complexity)
            .is_some(),
        "the failed build pauses automatic retries"
    );

    // Unchanged inputs: the next automatic run is skipped, naming the failure.
    let unchanged = enqueue_tier2_run(&ctx, &["complexity"]);
    assert!(paused(&unchanged), "{unchanged:#}");

    // A change outside the walked scope does not end the pause.
    ctx.add_pending_tier2_paths([
        root.join("node_modules/dep/index.ts"),
        root.join(".aft-test-storage/state.json"),
    ]);
    let out_of_scope = enqueue_tier2_run(&ctx, &["complexity"]);
    assert!(paused(&out_of_scope), "{out_of_scope:#}");
    assert_eq!(manager.reuse_start_count_for_test(), starts_after_failure);

    // The user fixes the file; the watcher reports it through the real drain;
    // the next automatic run rebuilds without waiting for the pause to elapse.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod 644");
    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded::<WatcherDispatchEvent>();
    *ctx.watcher_rx().lock() = Some(watcher_rx);
    let drain_all = || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
            assert!(Instant::now() < deadline, "watcher drain did not settle");
        }
    };
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![locked.clone()]))
        .expect("send watcher change");
    drain_all();
    let in_scope = enqueue_tier2_run(&ctx, &["complexity"]);
    assert_eq!(
        in_scope["queued_categories"],
        json!(["complexity"]),
        "{in_scope:#}"
    );
    wait_idle();
    assert_eq!(
        manager.reuse_start_count_for_test(),
        starts_after_failure + 1
    );
    assert!(manager
        .tier2_retry_pause(InspectCategory::Complexity)
        .is_none());

    // Break it again (new content, so the cached result is stale) and pause
    // it, then fix it WITHOUT a watcher report: an explicit aft_inspect still
    // computes complexity despite the pause.
    fs::write(&locked, "export const locked = 22;\n").expect("edit locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    ctx.add_pending_tier2_paths([locked.clone()]);
    let broken = enqueue_tier2_run(&ctx, &["complexity"]);
    assert_eq!(
        broken["queued_categories"],
        json!(["complexity"]),
        "{broken:#}"
    );
    wait_idle();
    assert!(manager
        .tier2_retry_pause(InspectCategory::Complexity)
        .is_some());
    // A watcher rescan drops the individual paths, so it ends the pause too.
    watcher_tx
        .send(WatcherDispatchEvent::RescanRequired(
            aft::watcher_filter::RescanReason::Unknown,
        ))
        .expect("send rescan");
    drain_all();
    assert!(manager
        .tier2_retry_pause(InspectCategory::Complexity)
        .is_none());
    let rebroken = enqueue_tier2_run(&ctx, &["complexity"]);
    assert_eq!(
        rebroken["queued_categories"],
        json!(["complexity"]),
        "{rebroken:#}"
    );
    wait_idle();
    assert!(manager
        .tier2_retry_pause(InspectCategory::Complexity)
        .is_some());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod 644");
    let response = inspect(&ctx);
    assert_eq!(response["success"], true, "{response:#}");
    assert!(
        response["summary"]["complexity"]["count"].is_number(),
        "explicit inspect must compute complexity while the automatic retry is paused: {response:#}"
    );
    assert!(
        manager
            .tier2_retry_pause(InspectCategory::Complexity)
            .is_none(),
        "the explicit run's success clears the pause"
    );
}
