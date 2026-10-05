//! Linked checkouts without a configured owner refuse honestly, but can read
//! their own already-published view without the owner's legacy store.

use std::path::{Path, PathBuf};
use std::process::Command;

use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::views::assembly::{head_tree_fingerprint, publish_checkout, AssemblyRequest};
use serde_json::{json, Value};

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Callgraph Test",
            "-c",
            "user.email=callgraph@example.test",
            "commit",
            "-qm",
            "callers",
        ],
    );
}

fn source(caller: &str) -> String {
    format!(
        "export function target(value: number) {{ return value; }}\n\
         export function {caller}(value: number) {{ return target(value); }}\n"
    )
}

fn linked_checkout(parent: &Path) -> PathBuf {
    let owner = parent.join("owner");
    std::fs::create_dir_all(&owner).unwrap();
    git(&owner, &["init", "-q"]);
    std::fs::write(owner.join("index.ts"), source("ownerCaller")).unwrap();
    commit(&owner);
    let checkout = parent.join("checkout");
    git(
        &owner,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            checkout.to_str().unwrap(),
        ],
    );
    std::fs::canonicalize(checkout).unwrap()
}

fn request(value: Value) -> RawRequest {
    serde_json::from_value(value).unwrap()
}

fn configure(checkout: &Path, storage: &Path, views: bool) -> AppContext {
    crate::test_helpers::disable_in_process_file_watcher();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
    let response = aft::commands::configure::handle_configure(
        &request(json!({
            "id": "configure-worktree-callgraph",
            "command": "configure",
            "harness": "opencode",
            "project_root": checkout,
            "storage_dir": storage,
            "config": crate::test_helpers::user_config(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": true,
                "views": { "enabled": views }
            }))
        })),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    assert!(ctx.is_worktree_bridge());
    assert!(ctx.shared_artifacts_read_only());
    assert!(!ctx.callgraph_writer());
    ctx
}

const OPERATIONS: [&str; 6] = [
    "callers",
    "impact",
    "call_tree",
    "trace_to",
    "trace_to_symbol",
    "trace_data",
];

fn query(ctx: &AppContext, checkout: &Path, operation: &str, caller: &str) -> Response {
    let symbol = match operation {
        "call_tree" | "trace_to_symbol" | "trace_data" => caller,
        _ => "target",
    };
    let req = request(json!({
        "id": operation,
        "command": operation,
        "file": checkout.join("index.ts"),
        "symbol": symbol,
        "toSymbol": "target",
        "expression": "value"
    }));
    match operation {
        "callers" => aft::commands::callers::handle_callers(&req, ctx),
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        "call_tree" => aft::commands::call_tree::handle_call_tree(&req, ctx),
        "trace_to" => aft::commands::trace_to::handle_trace_to(&req, ctx),
        "trace_to_symbol" => aft::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "trace_data" => aft::commands::trace_data::handle_trace_data(&req, ctx),
        _ => unreachable!(),
    }
}

#[test]
fn callgraph_worktree_without_owner_store_offers_local_fallback() {
    for views in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let checkout = linked_checkout(fixture.path());
        let storage = fixture.path().join("storage");
        // Only the linked checkout is configured: no owner route or store has
        // ever existed, including when views are enabled.
        let ctx = configure(&checkout, &storage, views);
        for operation in OPERATIONS {
            let response = query(&ctx, &checkout, operation, "ownerCaller");
            assert!(!response.success, "views={views}: {response:?}");
            assert_eq!(response.data["code"], "callgraph_unavailable");
            assert_eq!(response.data["results"], Value::Null);
            assert_eq!(response.data["index"]["callgraph"]["status"], "unavailable");
            assert_eq!(
                response.data["index"]["callgraph"]["reason"],
                "read_only_store_not_built"
            );
            assert_eq!(
                response.data["message"],
                format!("{operation}: no call graph has been built for this repository yet; it is built when the main checkout is opened; use grep or aft_search with pattern to find references in this checkout")
            );
        }
        assert!(
            aft::callgraph_store::CallGraphStore::open_readonly(
                ctx.callgraph_store_dir(),
                checkout.clone()
            )
            .unwrap()
            .is_none(),
            "a borrower must not build the owner's legacy store"
        );
    }
}

#[test]
fn callgraph_worktree_own_view_serves_all_operations_without_owner_store() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let storage = fixture.path().join("storage");
    std::fs::write(checkout.join("index.ts"), source("checkoutCaller")).unwrap();
    commit(&checkout);

    // Seed a durable checkout generation as fixture data, not through an owner
    // session. Its different caller proves the reader selects this checkout's
    // graph rather than an owner's graph or an empty legacy database.
    let family = aft::search_index::artifact_cache_key(&checkout);
    let scope = aft::path_identity::project_scope_key(&checkout);
    let head = aft::alias::head_tree_entries(&checkout).unwrap();
    let published = publish_checkout(&AssemblyRequest {
        storage: storage.clone(),
        project_root: checkout.clone(),
        family,
        scope,
        desired_head: head_tree_fingerprint(&head),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    })
    .unwrap();
    assert!(published.published);

    let ctx = configure(&checkout, &storage, true);
    for operation in OPERATIONS {
        let response = query(&ctx, &checkout, operation, "checkoutCaller");
        assert!(response.success, "{operation}: {response:?}");
        let data = response.data.to_string();
        assert!(data.contains("checkoutCaller"), "{operation}: {data}");
        assert!(!data.contains("ownerCaller"), "{operation}: {data}");
        assert!(response.data.get("borrowed_coverage").is_none(), "{data}");
        match operation {
            "callers" => assert_eq!(response.data["total_callers"], 1),
            "impact" => assert_eq!(response.data["total_affected"], 1),
            "call_tree" => assert_eq!(response.data["children"][0]["name"], "target"),
            "trace_to_symbol" => {
                assert_eq!(response.data["complete"], true);
                assert_eq!(response.data["path"].as_array().unwrap().len(), 2);
            }
            "trace_data" => assert_eq!(response.data["hops"][0]["flow_type"], "parameter"),
            "trace_to" => assert!(response.data["total_paths"].as_u64().unwrap() > 0),
            _ => unreachable!(),
        }
    }
    assert!(
        aft::callgraph_store::CallGraphStore::open_readonly(ctx.callgraph_store_dir(), checkout)
            .unwrap()
            .is_none(),
        "own-view queries must not create a legacy store"
    );
}
