//! Views-on linked checkouts wait for their own generation, never for the
//! owner's legacy store. Views-off borrowers retain the legacy refusal.

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
    let ctx = configure_deferred(checkout, storage, views);
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    ctx
}

fn configure_deferred(checkout: &Path, storage: &Path, views: bool) -> AppContext {
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
    assert!(ctx.is_worktree_bridge());
    assert!(ctx.shared_artifacts_read_only());
    assert!(!ctx.callgraph_writer());
    ctx
}

fn publish(root: &Path, storage: &Path) {
    let root = std::fs::canonicalize(root).unwrap();
    let head = aft::alias::head_tree_entries(&root).unwrap();
    let report = publish_checkout(&AssemblyRequest {
        storage: storage.to_path_buf(),
        family: aft::search_index::artifact_cache_key(&root),
        scope: aft::path_identity::project_scope_key(&root),
        project_root: root,
        desired_head: head_tree_fingerprint(&head),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    })
    .unwrap();
    assert!(report.published);
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
    query_file(ctx, &checkout.join("index.ts"), operation, caller)
}

fn query_file(ctx: &AppContext, file: &Path, operation: &str, caller: &str) -> Response {
    let symbol = match operation {
        "call_tree" | "trace_to_symbol" | "trace_data" => caller,
        _ => "target",
    };
    let req = request(json!({
        "id": operation,
        "command": operation,
        "file": file,
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
fn callgraph_ignored_nested_worktree_is_not_indexed_not_symbol_missing() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let storage = fixture.path().join("storage");
    std::fs::write(checkout.join(".gitignore"), ".cortexkit/\n").unwrap();
    commit(&checkout);
    let nested = checkout.join(".cortexkit/alfonso/implementation-worktrees/nested");
    std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            nested.to_str().unwrap(),
        ],
    );
    let file = nested.join("packages/core/src/pool-authority.ts");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, source("recordNativeMigrationExpectations")).unwrap();
    publish(&checkout, &storage);
    let ctx = configure(&checkout, &storage, true);

    let outline = aft::commands::outline::handle_outline(
        &request(json!({
            "id": "nested-outline", "command": "outline", "file": file
        })),
        &ctx,
    );
    assert!(outline.success, "{outline:?}");
    assert!(outline
        .data
        .to_string()
        .contains("recordNativeMigrationExpectations"));
    let response = aft::commands::callers::handle_callers(
        &request(json!({
            "id": "nested-callers", "command": "callers", "file": file,
            "symbol": "recordNativeMigrationExpectations"
        })),
        &ctx,
    );
    assert_eq!(response.data["code"], "not_indexed", "{response:?}");
    for operation in OPERATIONS {
        let response = query_file(&ctx, &file, operation, "recordNativeMigrationExpectations");
        assert!(!response.success, "{operation}: {response:?}");
        assert_eq!(
            response.data["code"], "not_indexed",
            "{operation}: {response:?}"
        );
        assert_eq!(response.data["reason"], "ignored");
        let message = response.data["message"].as_str().unwrap();
        assert!(
            message.contains("ignored by the project's ignore rules"),
            "{message}"
        );
        assert!(
            message.contains("grep or aft_search with pattern"),
            "{message}"
        );
    }
    let response = aft::commands::trace_to_symbol::handle_trace_to_symbol(
        &request(json!({
            "id": "nested-target", "command": "trace_to_symbol", "file": checkout.join("index.ts"),
            "symbol": "ownerCaller", "toSymbol": "recordNativeMigrationExpectations", "toFile": file
        })),
        &ctx,
    );
    assert_eq!(response.data["code"], "not_indexed", "{response:?}");
    assert_eq!(response.data["reason"], "ignored");
}

#[test]
fn callgraph_unindexed_path_differs_from_missing_symbol() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let storage = fixture.path().join("storage");
    let empty = checkout.join("empty.ts");
    std::fs::write(&empty, "// An indexed file with no symbols.\n").unwrap();
    commit(&checkout);
    publish(&checkout, &storage);
    let ctx = configure(&checkout, &storage, true);
    let unindexed = checkout.join("not-yet-indexed.ts");
    std::fs::write(&unindexed, source("newCaller")).unwrap();
    for operation in OPERATIONS {
        let response = query_file(&ctx, &unindexed, operation, "newCaller");
        assert_eq!(
            response.data["code"], "not_indexed",
            "{operation}: {response:?}"
        );
        assert_eq!(response.data["reason"], "not_in_generation");
    }
    let response = aft::commands::callers::handle_callers(
        &request(json!({
            "id": "missing-symbol", "command": "callers", "file": checkout.join("index.ts"),
            "symbol": "absentSymbol"
        })),
        &ctx,
    );
    assert_eq!(response.data["code"], "symbol_not_found", "{response:?}");
    for operation in OPERATIONS {
        let response = query_file(&ctx, &empty, operation, "absentSymbol");
        assert_eq!(
            response.data["code"], "symbol_not_found",
            "{operation}: {response:?}"
        );
        let outside = fixture.path().join("owner/index.ts");
        let response = query_file(&ctx, &outside, operation, "ownerCaller");
        assert_eq!(
            response.data["code"], "path_outside_project_root",
            "{operation}: {response:?}"
        );
        let message = response.data["message"].as_str().unwrap();
        assert!(
            message.contains("not indexed") && message.contains("grep or aft_search with pattern"),
            "{message}"
        );
    }
}

#[test]
fn callgraph_worktree_published_owner_routes_pending_then_own_view() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let owner = fixture.path().join("owner");
    let storage = fixture.path().join("storage");
    // The main checkout has published only shared blobs and its own view, not
    // a legacy store. The linked checkout has not assembled a generation yet.
    publish(&owner, &storage);
    std::fs::write(checkout.join("index.ts"), source("checkoutCaller")).unwrap();
    commit(&checkout);
    let ctx = configure_deferred(&checkout, &storage, true);
    let response = query(&ctx, &checkout, "callers", "checkoutCaller");
    assert_eq!(response.data["code"], "callgraph_building", "{response:?}");
    assert_eq!(response.data["index"]["callgraph"]["status"], "building");
    assert_eq!(response.data["progress"]["phase"], "configure_maintenance");
    assert_eq!(response.data["progress"]["pending_paths"], Value::Null);
    let message = response.data["message"].as_str().unwrap();
    assert!(
        message.contains("this worktree") && message.contains("assembling"),
        "{message}"
    );
    assert!(
        message.contains("grep or aft_search with pattern"),
        "{message}"
    );
    assert!(!message.contains("main checkout"), "{message}");

    // A different branch cannot assemble until its missing blob arrives. Even
    // after maintenance has run, it must not fall back to the legacy refusal.
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let response = query(&ctx, &checkout, "callers", "checkoutCaller");
    assert_eq!(response.data["code"], "callgraph_building", "{response:?}");
    assert_eq!(response.data["progress"]["phase"], "view_assembly");
    assert_eq!(response.data["progress"]["pending_paths"], 1);
    drop(ctx);

    // Once shared blobs cover this branch, configure assembles the worktree's
    // own generation. No query needs to walk or rebuild the tree.
    std::fs::write(owner.join("index.ts"), source("checkoutCaller")).unwrap();
    commit(&owner);
    publish(&owner, &storage);
    let ctx = configure(&checkout, &storage, true);
    let response = query(&ctx, &checkout, "callers", "checkoutCaller");
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["total_callers"], 1);
    assert!(response.data.to_string().contains("checkoutCaller"));
    assert!(!response.data.to_string().contains("ownerCaller"));
    assert!(response.data.get("borrowed_callgraph").is_none());
    match ctx.callgraph_store_for_ops() {
        aft::context::CallgraphStoreAccess::Ready(store) => {
            assert_eq!(store.reader_kind(), "view");
            assert_eq!(store.project_root(), checkout);
        }
        _ => panic!("expected this checkout's view reader"),
    }
    assert!(
        aft::callgraph_store::CallGraphStore::open_readonly(ctx.callgraph_store_dir(), checkout)
            .unwrap()
            .is_none(),
        "a borrower must not create a legacy store"
    );
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
            if views {
                // Without shared blobs, assembly remains pending. That is not
                // a legacy-store refusal, and opening the main checkout is not
                // a useful instruction to a worker already in this checkout.
                assert_eq!(response.data["code"], "callgraph_building");
                assert_eq!(response.data["index"]["callgraph"]["status"], "building");
                assert_eq!(response.data["results"], Value::Null);
                assert_eq!(response.data["progress"]["phase"], "view_assembly");
                assert_eq!(response.data["progress"]["pending_paths"], 1);
                let message = response.data["message"].as_str().unwrap();
                assert!(message.contains("waiting for shared blobs"), "{message}");
                assert!(
                    message.contains("grep or aft_search with pattern"),
                    "{message}"
                );
                assert!(!message.contains("main checkout"), "{message}");
                continue;
            }
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
