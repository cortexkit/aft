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

use crate::helpers::callgraph_when_ready;

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
    let ctx = configure_checkout_deferred(checkout, storage, views);
    assert!(ctx.is_worktree_bridge());
    assert!(ctx.shared_artifacts_read_only());
    assert!(!ctx.callgraph_writer());
    ctx
}

fn configure_checkout_deferred(checkout: &Path, storage: &Path, views: bool) -> AppContext {
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": true },
                "views": { "enabled": views },
                "worktree": { "ram_overlay": false }
            }))
        })),
        &ctx,
    );
    assert!(response.success, "{response:?}");
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

#[test]
fn callgraph_restart_reconciles_source_dirtied_before_configure() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("index.ts"), source("committedCaller")).unwrap();
    commit(&root);
    let storage = fixture.path().join("storage");
    publish(&root, &storage);
    std::fs::write(
        root.join("index.ts"),
        format!(
            "{}export function beforeRestart(value: number) {{ return dirtyTarget(value); }}\n\
             export function dirtyTarget(value: number) {{ return value; }}\n",
            source("dirtyCaller")
        ),
    )
    .unwrap();

    // No watcher is installed, and no source write occurs after the new context.
    let ctx = configure_checkout_deferred(&root, &storage, true);
    assert!(!ctx.is_worktree_bridge());
    let zoom = aft::commands::zoom::handle_zoom(
        &request(json!({
            "id": "restart-zoom", "command": "zoom",
            "file": root.join("index.ts"), "symbol": "dirtyTarget"
        })),
        &ctx,
    );
    assert!(
        zoom.success,
        "zoom should read live source independently: {zoom:?}"
    );
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let req = request(json!({
        "id": "restart-callers", "command": "callers",
        "file": root.join("index.ts"), "symbol": "dirtyTarget"
    }));
    let response = callgraph_when_ready("callers", || {
        aft::commands::callers::handle_callers(&req, &ctx)
    });
    assert!(
        response.success,
        "pre-restart dirty symbol must be found: {response:?}"
    );
    assert_eq!(response.data["total_callers"], 1);
    assert!(response.data.to_string().contains("beforeRestart"));
}

#[test]
fn callgraph_restart_reconciles_a_dirty_view_after_git_restore() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let storage = fixture.path().join("storage");
    std::fs::write(checkout.join("index.ts"), source("dirtyCaller")).unwrap();
    publish(&checkout, &storage);
    git(&checkout, &["restore", "index.ts"]);
    let ctx = configure(&checkout, &storage, true);
    let response = query(&ctx, &checkout, "callers", "ownerCaller");
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["total_callers"], 1);
    assert!(
        response.data.to_string().contains("ownerCaller"),
        "{response:?}"
    );
    assert!(
        !response.data.to_string().contains("dirtyCaller"),
        "{response:?}"
    );
}

#[test]
fn callgraph_worktree_publishes_unique_source_without_an_owner_writer_or_overlay() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let storage = fixture.path().join("storage");
    let worker_source = source("workerCaller");
    std::fs::write(checkout.join("index.ts"), &worker_source).unwrap();
    std::fs::write(
        checkout.join("added.ts"),
        "import { target } from './index';\nexport function addedCaller(value: number) { return target(value); }\n",
    )
    .unwrap();
    commit(&checkout);

    let ctx = configure(&checkout, &storage, true);
    assert!(ctx.shared_artifacts_read_only());
    assert!(!ctx.callgraph_writer());
    assert!(!ctx.ram_overlay_active());
    let response = query(&ctx, &checkout, "callers", "workerCaller");
    assert!(
        response.success,
        "worktree-only source must publish without an owner writer: {response:?}"
    );
    assert_eq!(response.data["total_callers"], 2);
    assert!(response.data.to_string().contains("workerCaller"));
    assert!(response.data.to_string().contains("addedCaller"));
    assert!(!response.data.to_string().contains("ownerCaller"));
    let key = aft::blob_store::CallgraphKey::from_bytes(
        worker_source.as_bytes(),
        "typescript",
        aft::views::callgraph::PRODUCER,
    )
    .full_key();
    let blobs = aft::blob_store::BlobStore::open(
        &storage,
        aft::search_index::artifact_cache_key(&checkout),
        aft::blob_store::BlobPlane::Callgraph,
    )
    .unwrap();
    assert!(
        blobs.contains(&key).unwrap(),
        "worktree must put its own immutable blob"
    );
    assert!(
        aft::callgraph_store::CallGraphStore::open_readonly(ctx.callgraph_store_dir(), checkout)
            .unwrap()
            .is_none(),
        "worktree must not acquire a legacy store writer"
    );
}

#[test]
fn callgraph_worktree_new_markdown_does_not_wait_for_unrelated_planes() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let owner = fixture.path().join("owner");
    let storage = fixture.path().join("storage");
    publish(&owner, &storage);

    // Only the worktree has these documents. No owner will ever produce a
    // shared callgraph blob for them, and semantic fill has not run yet.
    let documents = ["docs/audit/BRIEF.md", "docs/audit/audit.md"];
    std::fs::create_dir_all(checkout.join("docs/audit")).unwrap();
    for document in documents {
        std::fs::write(
            checkout.join(document),
            format!("# {document}\nNew audit.\n"),
        )
        .unwrap();
    }
    commit(&checkout);
    let head = aft::alias::head_tree_entries(&checkout).unwrap();
    let report = publish_checkout(&AssemblyRequest {
        storage: storage.clone(),
        family: aft::search_index::artifact_cache_key(&checkout),
        scope: aft::path_identity::project_scope_key(&checkout),
        project_root: checkout.clone(),
        desired_head: head_tree_fingerprint(&head),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: true,
        allow_blob_put: false,
        callgraph: true,
    })
    .unwrap();
    assert!(
        report.published,
        "new documents must not block callgraph publication: {report:?}"
    );
    assert_eq!(report.blob_puts, 0);
    assert_eq!(
        report.pending_paths,
        std::collections::BTreeSet::from([b"index.ts".to_vec()])
    );
    assert_eq!(
        report.pending_inputs[b"index.ts".as_slice()].plane,
        "semantic"
    );
    let manifest = report.manifest.as_ref().unwrap();
    for document in documents {
        let path = aft::views::RelPath::from_os_path(Path::new(document)).unwrap();
        let entry = manifest
            .get(&path)
            .expect("document must remain a manifest member");
        let aft::views::ManifestEntry::Regular { planes, .. } = entry else {
            panic!("document is not a regular member: {entry:?}");
        };
        assert!(planes.callgraph.is_none(), "document has a callgraph key");
        assert!(planes.semantic.is_none(), "semantic fill has not run");
    }
    assert!(!aft::views::assembly::manifest_lacks_callgraph(manifest));

    let ctx = configure(&checkout, &storage, true);
    let response = callgraph_when_ready("callers", || {
        query(&ctx, &checkout, "callers", "ownerCaller")
    });
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["total_callers"], 1);
    assert!(response.data.to_string().contains("ownerCaller"));
}

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
            // Git for Windows cannot create a worktree through the verbatim
            // prefix returned by canonicalize. Use the same relative destination
            // from this checkout without changing the ignored-worktree fixture.
            ".cortexkit/alfonso/implementation-worktrees/nested",
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
    let response = callgraph_when_ready("callers", || {
        aft::commands::callers::handle_callers(
            &request(json!({
                "id": "nested-callers", "command": "callers", "file": file,
                "symbol": "recordNativeMigrationExpectations"
            })),
            &ctx,
        )
    });
    assert_eq!(response.data["code"], "not_indexed", "{response:?}");
    for operation in OPERATIONS {
        let response = callgraph_when_ready(operation, || {
            query_file(&ctx, &file, operation, "recordNativeMigrationExpectations")
        });
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
    let response = callgraph_when_ready("trace_to_symbol", || {
        aft::commands::trace_to_symbol::handle_trace_to_symbol(
            &request(json!({
                "id": "nested-target", "command": "trace_to_symbol", "file": checkout.join("index.ts"),
                "symbol": "ownerCaller", "toSymbol": "recordNativeMigrationExpectations", "toFile": file
            })),
            &ctx,
        )
    });
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
        let response = callgraph_when_ready(operation, || {
            query_file(&ctx, &unindexed, operation, "newCaller")
        });
        assert_eq!(
            response.data["code"], "not_indexed",
            "{operation}: {response:?}"
        );
        assert_eq!(response.data["reason"], "not_in_generation");
    }
    let response = callgraph_when_ready("callers", || {
        aft::commands::callers::handle_callers(
            &request(json!({
                "id": "missing-symbol", "command": "callers", "file": checkout.join("index.ts"),
                "symbol": "absentSymbol"
            })),
            &ctx,
        )
    });
    assert_eq!(response.data["code"], "symbol_not_found", "{response:?}");
    for operation in OPERATIONS {
        let response = callgraph_when_ready(operation, || {
            query_file(&ctx, &empty, operation, "absentSymbol")
        });
        assert_eq!(
            response.data["code"], "symbol_not_found",
            "{operation}: {response:?}"
        );
        let outside = fixture.path().join("owner/index.ts");
        let response = callgraph_when_ready(operation, || {
            query_file(&ctx, &outside, operation, "ownerCaller")
        });
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
fn callgraph_worktree_published_owner_assembles_its_own_changed_source() {
    let fixture = tempfile::tempdir().unwrap();
    let checkout = linked_checkout(fixture.path());
    let owner = fixture.path().join("owner");
    let storage = fixture.path().join("storage");
    // The main checkout has published only shared blobs and its own view, not
    // a legacy store. The linked checkout has not assembled a generation yet.
    publish(&owner, &storage);
    let owner_view =
        aft::views::ViewStore::open(&storage, &aft::path_identity::project_scope_key(&owner))
            .unwrap();
    let owner_generation = owner_view.current_generation().unwrap();
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

    // The worktree contributes its own source blob during maintenance. The
    // owner keeps its original source and is never asked to build this branch.
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let response = query(&ctx, &checkout, "callers", "checkoutCaller");
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["total_callers"], 1);
    assert!(response.data.to_string().contains("checkoutCaller"));
    assert!(!response.data.to_string().contains("ownerCaller"));
    assert!(response.data.get("borrowed_callgraph").is_none());
    assert_eq!(
        std::fs::read_to_string(owner.join("index.ts")).unwrap(),
        source("ownerCaller")
    );
    assert!(ctx.shared_artifacts_read_only());
    assert_eq!(
        owner_view.current_generation().unwrap(),
        owner_generation,
        "worktree publication must not replace the owner's view"
    );
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
fn callgraph_worktree_without_owner_store_uses_own_view_or_legacy_refusal() {
    for views in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let checkout = linked_checkout(fixture.path());
        let storage = fixture.path().join("storage");
        // Only the linked checkout is configured: no owner route or store has
        // ever existed, including when views are enabled.
        let ctx = configure(&checkout, &storage, views);
        for operation in OPERATIONS {
            let response = query(&ctx, &checkout, operation, "ownerCaller");
            if views {
                assert!(response.success, "views={views}: {response:?}");
                assert!(response.data.get("borrowed_callgraph").is_none());
                assert!(ctx.shared_artifacts_read_only());
                continue;
            }
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
        let response = callgraph_when_ready(operation, || {
            query(&ctx, &checkout, operation, "checkoutCaller")
        });
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
