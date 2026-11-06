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

