#[path = "helpers/mod.rs"]
pub(crate) mod test_helpers;

use aft::callgraph::walk_project_files;
use aft::callgraph_store::{
    callgraph_refresh_worker_test_counts, clear_callgraph_refresh_worker_test_seam,
    install_callgraph_refresh_worker_test_gate, live_callgraph_edge_snapshot,
    project_dead_code_snapshot, set_callgraph_refresh_worker_test_open_failure,
    set_callgraph_refresh_worker_test_seam, CallGraphRead, CallGraphStore, StoredEdge,
};
use aft::commands::callgraph_store_adapter;
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::harness::Harness;
use aft::inspect::scanners::dead_code::run_dead_code_scan;
use aft::inspect::{InspectCategory, InspectJob, JobKey};
use aft::parser::{SymbolCache, TreeSitterProvider};
use filetime::FileTime;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc, RwLock,
};
use std::time::{Duration, Instant};
use tempfile::tempdir;

static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_800_000_000);

#[test]
fn writer_epoch_fence_rejects_usurped_callgraph_write() {
    let root_dir = tempdir().unwrap();
    let root = root_dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();

    let store_dir = tempdir().unwrap();
    let store = CallGraphStore::open(store_dir.path().to_path_buf(), root.to_path_buf()).unwrap();
    store.cold_build(&project_files(root)).unwrap();

    let lease_path = store_dir.path().join("writer.lease");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    metadata["writer_epoch"] = serde_json::Value::String("usurped-by-test".to_string());
    fs::write(&lease_path, serde_json::to_vec(&metadata).unwrap()).unwrap();

    let error = store
        .mark_files_stale(&[root.join("src/lib.rs")])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("lost epoch") || error.contains("unavailable"),
        "unexpected error: {error}"
    );
}

#[test]
fn store_trace_data_covers_callgraph_fixtures() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/callgraph");
    let root = std::fs::canonicalize(root).unwrap();
    let dir = tempdir().unwrap();
    let store =
        CallGraphStore::open(dir.path().join("trace-data-fixture-store"), root.clone()).unwrap();
    store.cold_build(&project_files(&root)).unwrap();

    assert_trace_data_parity(
        &root,
        &store,
        "trace_data fixture assignment plus cross-file parameter",
        "data_flow.ts",
        "transformData",
        "rawInput",
        5,
    );
    assert_trace_data_parity(
        &root,
        &store,
        "trace_data fixture destructuring approximation",
        "data_flow.ts",
        "complexFlow",
        "data",
        5,
    );
}

#[test]
fn store_trace_data_covers_edge_cases() {
    let dir = tempdir().unwrap();
    write_trace_data_parity_project(dir.path());
    let root = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let store = CallGraphStore::open(root.join(".trace-data-parity-store"), root.clone()).unwrap();
    store.cold_build(&project_files(&root)).unwrap();

    for (label, file, symbol, expression, depth) in [
        (
            "same-body assignment chain, cross-file parameter, unresolved callee",
            "src/flow.ts",
            "start",
            "raw",
            5,
        ),
        (
            "cross-file depth limiting",
            "src/flow.ts",
            "start",
            "raw",
            0,
        ),
        (
            "same-file local calls ignore depth like legacy fallback",
            "src/flow.ts",
            "depthStart",
            "raw",
            0,
        ),
        ("visited-set cycle", "src/flow.ts", "cycleA", "value", 5),
        ("direct self-recursion", "src/flow.ts", "f", "x", 5),
        (
            "mutual recursion through a self-call",
            "src/flow.ts",
            "mutualF",
            "x",
            5,
        ),
        (
            "scoped class method",
            "src/flow.ts",
            "Worker::run",
            "raw",
            5,
        ),
        (
            "spread argument approximation",
            "src/flow.ts",
            "spreadStart",
            "items",
            5,
        ),
        (
            "supplemental method-dispatch edge remains approximate",
            "src/flow.ts",
            "supplemental",
            "value",
            5,
        ),
    ] {
        assert_trace_data_parity(&root, &store, label, file, symbol, expression, depth);
    }
}

#[test]
fn store_trace_data_self_recursion_does_not_change_callers_or_impact() {
    let dir = tempdir().unwrap();
    write_trace_data_parity_project(dir.path());
    let root = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let store =
        CallGraphStore::open(root.join(".trace-data-self-recursion-store"), root.clone()).unwrap();
    store.cold_build(&project_files(&root)).unwrap();

    let file = root.join("src/flow.ts");
    let callers = serde_json::to_value(
        callgraph_store_adapter::callers_result(&store, &file, "f", 2, true).unwrap(),
    )
    .unwrap();
    assert_eq!(callers["total_callers"], json!(0));
    assert_eq!(callers["callers"], json!([]));

    let impact = serde_json::to_value(
        callgraph_store_adapter::impact_result(&store, &file, "f", 2, true).unwrap(),
    )
    .unwrap();
    assert_eq!(impact["total_affected"], json!(0));
    assert_eq!(impact["affected_files"], json!(0));
    assert_eq!(impact["callers"], json!([]));
}

#[test]
fn store_edges_match_live_callgraph_for_tier1_languages() {
    let dir = tempdir().unwrap();
    write_parity_project(dir.path());
    let files = project_files(dir.path());
    let store_dir = dir.path().join(".store");
    let store = CallGraphStore::open(store_dir, dir.path().to_path_buf()).unwrap();

    let stats = store.cold_build(&files).unwrap();
    assert!(
        stats.files >= 6,
        "expected mixed TS/JS/Rust files: {stats:?}"
    );

    let store_edges = store.edge_snapshot().unwrap();
    let live_edges = live_callgraph_edge_snapshot(dir.path(), &files).unwrap();
    assert_eq!(store_edges, live_edges);
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("main.ts")),
        "parity fixture should exercise TypeScript edges: {store_edges:#?}"
    );
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("app.js")),
        "parity fixture should exercise JavaScript edges: {store_edges:#?}"
    );
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("lib.rs")),
        "parity fixture should exercise Rust edges: {store_edges:#?}"
    );
}

#[test]
fn scenario_matrix_incremental_matches_cold_rebuild() {
    run_scenario(
        "rename symbol",
        setup_rename_symbol,
        edit_rename_symbol,
        None,
    );
    run_scenario("delete file", setup_delete_file, edit_delete_file, None);
    run_scenario(
        "delete reexport-only barrel",
        setup_barrel,
        edit_delete_barrel,
        None,
    );
    run_scenario(
        "add file satisfying prior unresolved import",
        setup_unresolved_import,
        edit_add_late_file,
        None,
    );
    run_scenario(
        "move symbol via reexport topology",
        setup_barrel_move,
        edit_move_reexport,
        None,
    );
    run_scenario(
        "barrel retarget while old target exists",
        setup_barrel,
        edit_retarget_barrel,
        None,
    );
    run_scenario(
        "file that both defines and calls",
        setup_defines_and_calls,
        edit_defines_and_calls,
        None,
    );
    run_scenario(
        "body-only edit does not invalidate fan-in",
        setup_body_only,
        edit_body_only,
        Some(|stats| {
            assert!(
                stats.surface_changed.is_empty(),
                "body-only edit should not change surface: {stats:?}"
            );
            assert_eq!(
                stats.dependency_selected_refs, 0,
                "body-only edit must not select fan-in refs: {stats:?}"
            );
        }),
    );
}

#[test]
fn scenario_matrix_op_outputs_incremental_match_cold_rebuild() {
    run_op_scenario(
        "rename symbol",
        setup_rename_symbol,
        edit_rename_symbol,
        ScenarioQuery::new("a.ts", "renamed", "a.ts", "outer", "renamed", Some("a.ts")),
    );
    run_op_scenario(
        "delete file",
        setup_delete_file,
        edit_delete_file,
        ScenarioQuery::new(
            "main.ts",
            "main",
            "main.ts",
            "main",
            "main",
            Some("main.ts"),
        ),
    );
    run_op_scenario(
        "delete reexport-only barrel",
        setup_barrel,
        edit_delete_barrel,
        ScenarioQuery::new(
            "main.ts",
            "main",
            "main.ts",
            "main",
            "main",
            Some("main.ts"),
        ),
    );
    run_op_scenario(
        "add file satisfying prior unresolved import",
        setup_unresolved_import,
        edit_add_late_file,
        ScenarioQuery::new(
            "late.ts",
            "late",
            "main.ts",
            "main",
            "late",
            Some("late.ts"),
        ),
    );
    run_op_scenario(
        "move symbol via reexport topology",
        setup_barrel_move,
        edit_move_reexport,
        ScenarioQuery::new("alt.ts", "foo", "main.ts", "main", "foo", Some("alt.ts")),
    );
    run_op_scenario(
        "barrel retarget while old target exists",
        setup_barrel,
        edit_retarget_barrel,
        ScenarioQuery::new("alt.ts", "foo", "main.ts", "main", "foo", Some("alt.ts")),
    );
    run_op_scenario(
        "file that both defines and calls",
        setup_defines_and_calls,
        edit_defines_and_calls,
        ScenarioQuery::new(
            "combo.ts",
            "next",
            "combo.ts",
            "caller",
            "next",
            Some("combo.ts"),
        ),
    );
    run_op_scenario(
        "body-only edit does not invalidate fan-in",
        setup_body_only,
        edit_body_only,
        ScenarioQuery::new("foo.ts", "foo", "main.ts", "main", "foo", Some("foo.ts")),
    );
}

#[test]
fn query_api_matches_hand_built_ts_expectations() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        r#"export function entry() {
  second();
  missing();
}

function second() {
  leaf();
}

function leaf() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-query"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let entry = store.node_for(Path::new("main.ts"), "entry").unwrap();
    assert_eq!(entry.symbol, "entry");
    assert!(entry.is_entry_point);

    let unresolved = store.unresolved_calls_of(&entry).unwrap();
    assert_eq!(
        unresolved
            .iter()
            .map(|call| call.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["missing"]
    );

    let tree = store.call_tree(Path::new("main.ts"), "entry", 2).unwrap();
    assert_eq!(tree.name, "entry");
    assert_eq!(
        tree.children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "missing"]
    );
    assert!(tree.children[0].resolved);
    assert!(!tree.children[1].resolved);
    assert_eq!(tree.children[0].children[0].name, "leaf");

    let reverse_targets = vec![
        ("main.ts".to_string(), "leaf".to_string()),
        ("main.ts".to_string(), "second".to_string()),
        ("main.ts".to_string(), "missing".to_string()),
    ];
    let batched_callers = store.direct_callers_for_symbols(&reverse_targets).unwrap();
    for (file, symbol) in &reverse_targets {
        assert_eq!(
            batched_callers
                .get(&(file.clone(), symbol.clone()))
                .unwrap(),
            &store.direct_callers_of(Path::new(file), symbol).unwrap()
        );
    }

    let callers = store.callers_of(Path::new("main.ts"), "leaf", 2).unwrap();
    assert_eq!(callers.target.symbol, "leaf");
    assert!(callers
        .callers
        .iter()
        .any(|site| site.caller.symbol == "second"));
    assert!(callers
        .callers
        .iter()
        .any(|site| site.caller.symbol == "entry"));

    let impact = store.impact_of(Path::new("main.ts"), "leaf", 1).unwrap();
    assert_eq!(impact.target.symbol, "leaf");
    assert_eq!(impact.callers[0].site.caller.symbol, "second");
    assert_eq!(
        impact.callers[0].call_expression.as_deref(),
        Some("leaf();")
    );

    let trace = store.trace_to(Path::new("main.ts"), "leaf", 4).unwrap();
    assert_eq!(trace.entry_points_found, 1);
    assert_eq!(
        trace.paths[0]
            .hops
            .iter()
            .map(|hop| hop.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["entry", "second", "leaf"]
    );

    let candidates = store.trace_to_symbol_candidates("leaf").unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].file, "main.ts");

    let path = store
        .trace_to_symbol(Path::new("main.ts"), "entry", "leaf", None, 4)
        .unwrap()
        .path
        .unwrap();
    assert_eq!(
        path.iter()
            .map(|hop| hop.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["entry", "second", "leaf"]
    );
}

#[test]
fn reverse_lookup_includes_orphan_target_edges() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        r#"export function orphanCaller() {}
export function leaf() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-orphan"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let source_node: String = conn
        .query_row(
            "SELECT id FROM nodes WHERE file_path = 'main.ts' AND scoped_name = 'orphanCaller'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO refs(ref_id, caller_node, caller_file, kind, short_name, full_ref, line, byte_start, byte_end, status, target_file, target_symbol, provenance)
         VALUES('orphan-ref', ?1, 'main.ts', 'call', 'leaf', 'leaf', 1, 1, 5, 'resolved', 'main.ts', 'leaf', 'test')",
        rusqlite::params![source_node],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
         VALUES('orphan-edge', 'orphan-ref', ?1, NULL, 'main.ts', 'leaf', 'call', 1, 'test')",
        rusqlite::params![source_node],
    )
    .unwrap();
    drop(conn);

    let callers = store.callers_of(Path::new("main.ts"), "leaf", 1).unwrap();
    assert!(
        callers
            .callers
            .iter()
            .any(|site| site.caller.symbol == "orphanCaller" && site.target.is_none()),
        "target_node=NULL edge should still be found by target file/symbol: {callers:#?}"
    );
}

#[test]
fn dead_code_projection_falls_back_to_top_level_for_stale_caller_node() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("src/index.ts"),
        "import { used } from './used';
used();
",
    );
    write_file(
        &dir.path().join("src/used.ts"),
        "export function used() { return 1; }
export function dead() { return 2; }
",
    );
    let root = fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let store = CallGraphStore::open(root.join(".store-projection-orphan"), root.clone()).unwrap();
    store.cold_build(&project_files(&root)).unwrap();

    {
        let conn = Connection::open(store.sqlite_path()).unwrap();
        let updated = conn
            .execute(
                "UPDATE refs SET caller_node = ?1 WHERE caller_file = ?2 AND kind = 'call' AND short_name = 'used'",
                params!["pos:missing-caller-node", "src/index.ts"],
            )
            .unwrap();
        assert_eq!(updated, 1, "fixture should have one top-level call ref");
    }

    let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect("projection succeeds");
    let call = snapshot
        .outbound_calls
        .iter()
        .find(|call| {
            call.caller_file.ends_with(Path::new("src/index.ts"))
                && call
                    .target
                    .replace('\\', "/")
                    .ends_with("src/used.ts::used")
        })
        .expect("projected used() call");
    assert_eq!(call.caller_symbol, "<top-level>");

    let success = run_dead_code_scan(&dead_code_job(&root, project_files(&root), snapshot))
        .outcome
        .expect("dead_code scan succeeds");
    assert_eq!(success.aggregate["callgraph_available"], true);
    assert!(
        success.aggregate["count"].as_u64().unwrap_or(0) > 0,
        "aggregate should be a real dead_code result, not callgraph_unavailable: {:#}",
        success.aggregate
    );
    assert_ne!(
        success.aggregate.get("notes"),
        Some(&json!(["callgraph_unavailable"]))
    );
}

#[test]
fn refresh_files_promotes_dependency_caller_when_fresh_nodes_drift_from_store() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("src/caller.ts"),
        "import { used } from './target';
export function caller() { used(); }
",
    );
    write_file(
        &dir.path().join("src/target.ts"),
        "export function used() { return 1; }
",
    );
    let root = fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let store = CallGraphStore::open(root.join(".store-refresh-drift"), root.clone()).unwrap();
    store.cold_build(&project_files(&root)).unwrap();

    let original_node = {
        let conn = Connection::open(store.sqlite_path()).unwrap();
        conn.query_row(
            "SELECT caller_node FROM refs WHERE caller_file = ?1 AND kind = 'call' AND short_name = 'used'",
            params!["src/caller.ts"],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .expect("call ref has caller_node")
    };
    let stale_node = format!("{original_node}:stale");
    {
        let conn = Connection::open(store.sqlite_path()).unwrap();
        assert_eq!(
            conn.execute(
                "UPDATE nodes SET id = ?1 WHERE id = ?2",
                params![&stale_node, &original_node],
            )
            .unwrap(),
            1
        );
        conn.execute(
            "UPDATE refs SET caller_node = ?1 WHERE caller_node = ?2",
            params![&stale_node, &original_node],
        )
        .unwrap();
        conn.execute(
            "UPDATE edges SET source_node = ?1 WHERE source_node = ?2",
            params![&stale_node, &original_node],
        )
        .unwrap();
    }
    assert_no_dangling_caller_nodes(store.sqlite_path());

    // Changing the target's kind changes its identity and selects its incoming
    // caller for resolution. The stored caller ID was corrupted above, so the
    // refresh must also promote that caller and repair its owned node rows.
    let target = root.join("src/target.ts");
    write_file(
        &target,
        "export function extra() { return 2; }
export class used {}
",
    );
    bump_mtime(&target);

    let stats = store.refresh_files(std::slice::from_ref(&target)).unwrap();
    assert_eq!(stats.refreshed_own_files, 2);
    assert_no_dangling_caller_nodes(store.sqlite_path());

    let conn = Connection::open(store.sqlite_path()).unwrap();
    let refreshed_node = conn
        .query_row(
            "SELECT caller_node FROM refs WHERE caller_file = ?1 AND kind = 'call' AND short_name = 'used'",
            params!["src/caller.ts"],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .expect("refreshed call ref has caller_node");
    assert_eq!(refreshed_node, original_node);
    let matching_nodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = ?1",
            params![refreshed_node],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(matching_nodes, 1);
}

#[test]
fn cold_build_skips_failed_extracts_and_marks_them_stale() {
    let dir = tempdir().unwrap();
    let good = dir.path().join("good.ts");
    let bad = dir.path().join("bad.txt");
    write_file(&good, "export function good() {}\n");
    write_file(&bad, "not a callgraph language\n");

    let store = CallGraphStore::open(
        dir.path().join(".store-best-effort"),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let stats = store.cold_build(&[good, bad.clone()]).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.failed_files, vec!["bad.txt"]);
    assert_eq!(
        store.backend_status_for_file(&bad).unwrap().as_deref(),
        Some("stale")
    );
    assert!(store.node_for(Path::new("good.ts"), "good").is_ok());
}

#[test]
fn scoped_entry_point_flag_matches_legacy_scoped_input() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("suite.ts"),
        r#"class Suite {
  setup() {}
}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-entry"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let setup = store
        .node_for(Path::new("suite.ts"), "Suite::setup")
        .unwrap();
    assert!(
        !setup.is_entry_point,
        "scoped Suite::setup must not be treated like bare setup"
    );
}

#[test]
fn outgoing_calls_are_returned_in_source_order() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("order.ts"),
        r#"export function entry() { beta(); alpha(); }
function alpha() {}
function beta() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-order"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let entry = store.node_for(Path::new("order.ts"), "entry").unwrap();
    let calls = store.outgoing_calls_of(&entry).unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call.target_symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["beta", "alpha"]
    );
}

#[test]
fn store_re_roots_relative_metadata_after_project_move() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("root-a");
    fs::create_dir_all(&root_a).unwrap();
    write_file(
        &root_a.join("main.ts"),
        "export function entry() { leaf(); }\nfunction leaf() {}\n",
    );
    let root_a = fs::canonicalize(&root_a).unwrap_or(root_a);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root_a.clone(),
        &project_files(&root_a),
    )
    .unwrap();
    let source_sqlite = store.sqlite_path().to_path_buf();
    drop(store);

    let root_b_raw = dir.path().join("root-b");
    fs::rename(&root_a, &root_b_raw).unwrap();
    let root_b = fs::canonicalize(&root_b_raw).unwrap_or(root_b_raw);
    let moved_sqlite = copy_sqlite_file_set_to_legacy_root(&source_sqlite, &store_dir, &root_b);
    assert_eq!(
        backend_workspace_roots(&moved_sqlite),
        vec![root_a.display().to_string()]
    );

    let reopened = CallGraphStore::open(store_dir, root_b.clone()).unwrap();
    assert_eq!(
        backend_workspace_roots(reopened.sqlite_path()),
        vec![root_b.display().to_string()]
    );
    let snapshot = project_dead_code_snapshot(reopened.sqlite_path()).unwrap();
    // Projection paths carry the normalized (verbatim-stripped) canonical
    // form, so the expectation must not use bare fs::canonicalize (verbatim
    // on Windows).
    assert!(
        snapshot
            .files
            .contains(&canonicalize_like_projection(&root_b.join("main.ts"))),
        "projection should serve paths under moved root: {:#?}",
        snapshot.files
    );
}

#[test]
fn store_recovers_dual_root_poisoned_metadata_after_project_move() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("root-a");
    fs::create_dir_all(&root_a).unwrap();
    write_file(
        &root_a.join("main.ts"),
        "export function entry() { leaf(); }\nfunction leaf() {}\n",
    );
    let root_a = fs::canonicalize(&root_a).unwrap_or(root_a);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root_a.clone(),
        &project_files(&root_a),
    )
    .unwrap();
    let source_sqlite = store.sqlite_path().to_path_buf();
    drop(store);

    let root_b_raw = dir.path().join("root-b");
    fs::rename(&root_a, &root_b_raw).unwrap();
    let root_b = fs::canonicalize(&root_b_raw).unwrap_or(root_b_raw);
    let moved_sqlite = copy_sqlite_file_set_to_legacy_root(&source_sqlite, &store_dir, &root_b);
    duplicate_backend_workspace_root(&moved_sqlite, &root_b);
    assert_eq!(
        backend_workspace_roots(&moved_sqlite),
        vec![root_a.display().to_string(), root_b.display().to_string()]
    );

    let reopened = CallGraphStore::open(store_dir, root_b.clone()).unwrap();
    assert_eq!(
        backend_workspace_roots(reopened.sqlite_path()),
        vec![root_b.display().to_string()]
    );
    let snapshot = project_dead_code_snapshot(reopened.sqlite_path()).unwrap();
    assert!(snapshot
        .exported_symbols
        .iter()
        .any(|symbol| symbol.symbol == "entry"));
}

#[test]
fn store_cold_rebuilds_when_moved_database_has_absolute_data_paths() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("root-a");
    fs::create_dir_all(&root_a).unwrap();
    write_file(
        &root_a.join("main.ts"),
        "export function entry() { leaf(); }\nfunction leaf() {}\n",
    );
    let root_a = fs::canonicalize(&root_a).unwrap_or(root_a);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root_a.clone(),
        &project_files(&root_a),
    )
    .unwrap();
    let source_sqlite = store.sqlite_path().to_path_buf();
    drop(store);

    let root_b_raw = dir.path().join("root-b");
    fs::rename(&root_a, &root_b_raw).unwrap();
    let root_b = fs::canonicalize(&root_b_raw).unwrap_or(root_b_raw);
    let moved_sqlite = copy_sqlite_file_set_to_legacy_root(&source_sqlite, &store_dir, &root_b);
    poison_files_path_with_absolute_root(&moved_sqlite, &root_a.join("main.ts"));

    let reopened = CallGraphStore::open(store_dir, root_b.clone()).unwrap();
    assert_ne!(
        reopened.sqlite_path(),
        moved_sqlite.as_path(),
        "absolute data rows should publish a fresh generation instead of serving the copied DB"
    );
    assert_eq!(
        backend_workspace_roots(reopened.sqlite_path()),
        vec![root_b.display().to_string()]
    );
    assert!(!store_has_absolute_data_paths(reopened.sqlite_path()));
    let tree = reopened
        .call_tree(Path::new("main.ts"), "entry", 1)
        .unwrap();
    assert_eq!(tree.children[0].name, "leaf");
}

#[test]
fn live_poisoned_callgraph_db_copy_recovers_projection_if_available() {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("skipping live poisoned DB copy test: HOME unset");
        return;
    };
    let source = PathBuf::from(home)
        .join(".local/share/cortexkit/aft/opencode/callgraph/90ff783f3f4c5cf2.sqlite");
    if !source.is_file() {
        eprintln!(
            "skipping live poisoned DB copy test: {} is not present",
            source.display()
        );
        return;
    }

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("repo root from CARGO_MANIFEST_DIR");
    let repo_root = fs::canonicalize(repo_root).expect("canonical repo root");
    let dir = tempdir().unwrap();
    let store_dir = dir.path().join("callgraph");
    let copied = legacy_sqlite_path_for_root(&store_dir, &repo_root);
    copy_sqlite_file_set(&source, &copied);

    let store = CallGraphStore::open_ready_repairing(store_dir, repo_root.clone())
        .unwrap()
        .expect("copied ready DB should open");
    let snapshot = project_dead_code_snapshot(store.sqlite_path()).unwrap();
    assert!(
        !snapshot.files.is_empty(),
        "live-ish copied DB should recover to a serving projection"
    );
    assert_eq!(
        backend_workspace_roots(store.sqlite_path()),
        vec![repo_root.display().to_string()]
    );
}

#[test]
#[ignore]
fn measure_current_worktree_cold_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf();
    let files = project_files(&root);
    let dir = tempdir().unwrap();
    let store = CallGraphStore::open(dir.path().join("callgraph"), root.clone()).unwrap();
    let stats = store.cold_build(&files).unwrap();
    let rss = peak_rss_bytes();
    eprintln!(
        "callgraph_store_measure root={} files={} nodes={} refs={} edges={} elapsed_ms={} peak_rss_bytes={}",
        root.display(),
        stats.files,
        stats.nodes,
        stats.refs,
        stats.edges,
        stats.elapsed_ms,
        rss.unwrap_or(0)
    );
}

#[derive(Clone, Copy)]
struct ScenarioQuery {
    target_file: &'static str,
    target_symbol: &'static str,
    tree_file: &'static str,
    tree_symbol: &'static str,
    to_symbol: &'static str,
    to_file: Option<&'static str>,
}

impl ScenarioQuery {
    fn new(
        target_file: &'static str,
        target_symbol: &'static str,
        tree_file: &'static str,
        tree_symbol: &'static str,
        to_symbol: &'static str,
        to_file: Option<&'static str>,
    ) -> Self {
        Self {
            target_file,
            target_symbol,
            tree_file,
            tree_symbol,
            to_symbol,
            to_file,
        }
    }
}

fn run_op_scenario(
    name: &str,
    setup: fn(&Path),
    edit: fn(&Path) -> Vec<PathBuf>,
    query: ScenarioQuery,
) {
    let dir = tempdir().unwrap();
    setup(dir.path());

    let root = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let files_before = project_files(&root);
    let incremental_store =
        CallGraphStore::open(root.join(".store-op-incremental"), root.clone()).unwrap();
    incremental_store.cold_build(&files_before).unwrap();

    let changed = edit(&root);
    incremental_store.refresh_files(&changed).unwrap();
    let incremental = scenario_op_snapshot(&incremental_store, &root, query);

    let cold_store = CallGraphStore::open(root.join(".store-op-cold"), root.clone()).unwrap();
    cold_store.cold_build(&project_files(&root)).unwrap();
    let cold = scenario_op_snapshot(&cold_store, &root, query);

    assert_eq!(
        incremental, cold,
        "scenario {name} op-layer output after incremental refresh must match cold rebuild"
    );
}

fn scenario_op_snapshot(store: &CallGraphStore, root: &Path, query: ScenarioQuery) -> Value {
    let target_file = root.join(query.target_file);
    let tree_file = root.join(query.tree_file);
    let to_file = query.to_file.map(|file| root.join(file));
    json!({
        "callers": serde_json::to_value(
            callgraph_store_adapter::callers_result(store, &target_file, query.target_symbol, 2, true)
                .unwrap()
        )
        .unwrap(),
        "call_tree": serde_json::to_value(
            callgraph_store_adapter::call_tree_result(store, &tree_file, query.tree_symbol, 2, true)
                .unwrap()
        )
        .unwrap(),
        "impact": serde_json::to_value(
            callgraph_store_adapter::impact_result(store, &target_file, query.target_symbol, 2, true)
                .unwrap()
        )
        .unwrap(),
        "trace_to": serde_json::to_value(
            callgraph_store_adapter::trace_to_result(store, &target_file, query.target_symbol, 4, true)
                .unwrap()
        )
        .unwrap(),
        "trace_to_symbol": serde_json::to_value(
            callgraph_store_adapter::trace_to_symbol_result(
                store,
                &tree_file,
                query.tree_symbol,
                query.to_symbol,
                to_file.as_deref(),
                4,
                true,
            )
            .unwrap()
        )
        .unwrap(),
    })
}

fn assert_trace_data_parity(
    root: &Path,
    store: &CallGraphStore,
    label: &str,
    file: &str,
    symbol: &str,
    expression: &str,
    depth: usize,
) {
    let file_path = root.join(file);
    let symbol_cache = Arc::new(RwLock::new(SymbolCache::new()));
    let result = callgraph_store_adapter::trace_data_result(
        store,
        &file_path,
        symbol,
        expression,
        depth,
        symbol_cache,
    )
    .unwrap_or_else(|error| panic!("{label} trace_data should succeed: {error}"));

    assert_eq!(result.origin_symbol, symbol, "{label}: origin symbol");
    assert_eq!(result.expression, expression, "{label}: expression");
}

fn run_scenario(
    name: &str,
    setup: fn(&Path),
    edit: fn(&Path) -> Vec<PathBuf>,
    extra_assert: Option<fn(&aft::callgraph_store::IncrementalStats)>,
) {
    let dir = tempdir().unwrap();
    setup(dir.path());

    let files_before = project_files(dir.path());
    let store = CallGraphStore::open(
        dir.path().join(".store-incremental"),
        dir.path().to_path_buf(),
    )
    .unwrap();
    store.cold_build(&files_before).unwrap();

    let changed = edit(dir.path());
    let stats = store.refresh_files(&changed).unwrap();
    if let Some(assertion) = extra_assert {
        assertion(&stats);
    }
    let incremental = store.edge_snapshot().unwrap();

    let cold = cold_edges(dir.path());
    assert_eq!(
        incremental, cold,
        "scenario {name} incremental graph must match cold rebuild"
    );
}

fn cold_edges(root: &Path) -> BTreeSet<StoredEdge> {
    let store = CallGraphStore::open(root.join(".store-cold"), root.to_path_buf()).unwrap();
    let files = project_files(root);
    store.cold_build(&files).unwrap();
    store.edge_snapshot().unwrap()
}

fn root_keyed_test_context(root: &Path, storage: &Path, worktree: bool) -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            indexes: aft::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
    ctx.set_harness(Harness::Opencode);
    ctx.set_canonical_cache_root(root.to_path_buf());
    let project_key = artifact_cache_key_for_test(root);
    aft::root_cache::configure_artifact_access(root, &project_key, worktree);
    ctx.set_cache_role(worktree, None);
    ctx
}

fn entry_leaf_from_store(store: &impl CallGraphRead) -> String {
    store
        .call_tree(Path::new("main.ts"), "entry", 1)
        .unwrap()
        .children[0]
        .name
        .clone()
}

fn wait_for_callgraph_background(ctx: &AppContext, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        aft::runtime_drain::drain_callgraph_store_events(ctx);
        if ctx.callgraph_store_rx().lock().is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "callgraph background work did not finish in time"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_root_keyed_callgraph(ctx: &AppContext, budget: Duration) {
    wait_for_callgraph_background(ctx, budget);
    assert!(
        ctx.callgraph_store()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|store| !store.is_legacy_fallback()),
        "background legacy migration did not publish and install a root-keyed store"
    );
}

/// Drop `-wal`/`-shm` sidecars from a snapshot: SQLite rewrites them on any
/// connection open/close, including strictly read-only ones, so byte-equality
/// assertions about source preservation must compare only the data files.
fn without_wal_sidecars(files: Vec<(PathBuf, Vec<u8>)>) -> Vec<(PathBuf, Vec<u8>)> {
    files
        .into_iter()
        .filter(|(path, _)| {
            let name = path.to_string_lossy();
            !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect()
}

fn directory_file_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(base: &Path, dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(base, &entry.path(), files);
            } else if entry.file_type().unwrap().is_file() {
                files.push((
                    entry.path().strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    collect(dir, dir, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn project_files(root: &Path) -> Vec<PathBuf> {
    walk_project_files(root).collect()
}

const TEST_STORE_DATA_PATH_COLUMNS: &[(&str, &str)] = &[
    ("files", "path"),
    ("nodes", "file_path"),
    ("refs", "caller_file"),
    ("refs", "target_file"),
    ("file_dependencies", "file_path"),
    ("file_dependencies", "dep_file"),
    ("edges", "target_file"),
    ("dispatch_hints", "file"),
    ("backend_file_state", "file_path"),
];

fn artifact_cache_key_for_test(root: &Path) -> String {
    let _git_env = crate::test_helpers::hermetic_git_env_guard();
    aft::search_index::artifact_cache_key(root)
}

fn init_git_repo(root: &Path) {
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        assert!(
            crate::test_helpers::apply_hermetic_git_env(command.current_dir(root))
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {:?} in {}",
            args,
            root.display()
        );
    };
    git(&["init"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=AFT Tests",
        "-c",
        "user.email=aft-tests@example.com",
        "commit",
        "-m",
        "initial",
    ]);
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        }
        // Sockets, fifos, and other special files (e.g. git's fsmonitor
        // daemon socket) cannot be fs::copy'd and are irrelevant to the
        // clone fixture.
    }
    Ok(())
}

fn legacy_sqlite_path_for_root(store_dir: &Path, root: &Path) -> PathBuf {
    store_dir.join(format!("{}.sqlite", artifact_cache_key_for_test(root)))
}

fn copy_sqlite_file_set_to_legacy_root(
    source_sqlite: &Path,
    store_dir: &Path,
    root: &Path,
) -> PathBuf {
    let destination = legacy_sqlite_path_for_root(store_dir, root);
    copy_sqlite_file_set(source_sqlite, &destination);
    destination
}

fn copy_sqlite_file_set(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    remove_sqlite_file_set_for_test(destination);
    fs::copy(source, destination).unwrap();
    for suffix in ["-wal", "-shm"] {
        let source_sidecar = sqlite_sidecar_path(source, suffix);
        if source_sidecar.exists() {
            fs::copy(source_sidecar, sqlite_sidecar_path(destination, suffix)).unwrap();
        }
    }
}

fn remove_sqlite_file_set_for_test(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(sqlite_sidecar_path(path, "-wal"));
    let _ = fs::remove_file(sqlite_sidecar_path(path, "-shm"));
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn backend_workspace_roots(sqlite_path: &Path) -> Vec<String> {
    let conn = Connection::open(sqlite_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT workspace_root
             FROM backend_file_state
             ORDER BY workspace_root",
        )
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn duplicate_backend_workspace_root(sqlite_path: &Path, root: &Path) {
    let conn = Connection::open(sqlite_path).unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO backend_file_state(
            backend, workspace_root, file_path, content_hash, status, updated_at
         )
         SELECT backend, ?1, file_path, content_hash, status, updated_at
         FROM backend_file_state",
        params![root.display().to_string()],
    )
    .unwrap();
}

fn poison_files_path_with_absolute_root(sqlite_path: &Path, absolute_path: &Path) {
    let conn = Connection::open(sqlite_path).unwrap();
    conn.execute(
        "UPDATE files SET path = ?1 WHERE path = 'main.ts'",
        params![absolute_path.display().to_string()],
    )
    .unwrap();
}

fn store_has_absolute_data_paths(sqlite_path: &Path) -> bool {
    let conn = Connection::open(sqlite_path).unwrap();
    for (table, column) in TEST_STORE_DATA_PATH_COLUMNS {
        let sql = format!(
            "SELECT DISTINCT {column} FROM {table} WHERE {column} IS NOT NULL AND {column} <> ''"
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let value: String = row.get(0).unwrap();
            if stored_path_looks_absolute(&value) {
                return true;
            }
        }
    }
    false
}

fn stored_path_looks_absolute(value: &str) -> bool {
    if Path::new(value).is_absolute() || value.starts_with('/') {
        return true;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
        && bytes[0].is_ascii_alphabetic()
    {
        return true;
    }
    value.starts_with("\\\\") || value.starts_with("//")
}

fn dead_code_job(
    root: &Path,
    scope_files: Vec<PathBuf>,
    snapshot: aft::inspect::CallgraphSnapshot,
) -> InspectJob {
    InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::DeadCode),
        category: InspectCategory::DeadCode,
        scope_files,
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".aft-cache").join("inspect"),
        config: Arc::new(Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        }),
        symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: Some(Arc::new(snapshot)),
    }
}

fn assert_no_dangling_caller_nodes(sqlite_path: &Path) {
    let conn = Connection::open(sqlite_path).unwrap();
    let dangling: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM refs r
             LEFT JOIN nodes n ON n.id = r.caller_node
             WHERE r.caller_node IS NOT NULL AND n.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0, "refs.caller_node must match a nodes row");
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    bump_mtime(path);
}

fn bump_mtime(path: &Path) {
    let secs = NEXT_MTIME.fetch_add(1, Ordering::SeqCst);
    filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).unwrap();
}

fn remove_file(path: &Path) {
    fs::remove_file(path).unwrap();
}

fn write_trace_data_parity_project(root: &Path) {
    write_file(
        &root.join("package.json"),
        r#"{"name":"trace-data-parity-fixture","type":"module"}"#,
    );
    write_file(
        &root.join("src/flow.ts"),
        r#"import { externalSink } from "./sink";

export function start(raw: string): string {
  const first = raw;
  const second = first;
  externalSink(second);
  missingCall(second);
  return second;
}

export function depthStart(raw: string): void {
  depthMiddle(raw);
}

export function depthMiddle(input: string): void {
  depthLeaf(input);
}

export function depthLeaf(value: string): void {}

export function cycleA(value: string): void {
  cycleB(value);
}

export function cycleB(value: string): void {
  cycleA(value);
}

export function f(x: string): void {
  f(x);
}

export function mutualF(x: string): void {
  mutualF(x);
  mutualG(x);
}

export function mutualG(x: string): void {
  mutualF(x);
}

export class Worker {
  run(raw: string): string {
    const copy = raw;
    return copy;
  }
}

export function spreadStart(items: string[]): void {
  externalSink(...items);
}

export class Service {
  handle(value: string): void {}
}

export function supplemental(value: string, service: Service): void {
  service.handle(value);
}
"#,
    );
    write_file(
        &root.join("src/sink.ts"),
        r#"export function externalSink(input: string): void {
  const local = input;
  console.log(local);
}
"#,
    );
}

fn write_parity_project(root: &Path) {
    write_file(
        &root.join("package.json"),
        r#"{"name":"callgraph-store-fixture","type":"module"}"#,
    );
    write_file(
        &root.join("Cargo.toml"),
        r#"[package]
name = "callgraph_store_fixture"
version = "0.1.0"
edition = "2021"
"#,
    );
    write_file(
        &root.join("src/main.ts"),
        r#"import { foo as renamed } from "./foo";
import runDefault from "./def";
import * as ns from "./ns";

export function main() {
  renamed();
  runDefault();
  ns.member();
  localOnly();
}

function localOnly() {}
"#,
    );
    write_file(
        &root.join("src/foo.ts"),
        r#"export function foo() {}
"#,
    );
    write_file(
        &root.join("src/def.ts"),
        r#"export default function runDefault() {}
"#,
    );
    write_file(
        &root.join("src/ns.ts"),
        r#"export function member() {}
"#,
    );
    write_file(
        &root.join("src/app.js"),
        r#"import { jsHelper } from "./js_helper.js";

export function jsEntry() {
  jsHelper();
}
"#,
    );
    write_file(
        &root.join("src/js_helper.js"),
        r#"export function jsHelper() {}
"#,
    );
    write_file(
        &root.join("src/lib.rs"),
        r#"mod util;
use crate::util::rust_helper;

pub fn rust_entry() {
    rust_helper();
}
"#,
    );
    write_file(
        &root.join("src/util.rs"),
        r#"pub fn rust_helper() {}
"#,
    );
}

fn setup_rename_symbol(root: &Path) {
    write_file(
        &root.join("a.ts"),
        r#"export function outer() {
  inner();
}

export function inner() {}
"#,
    );
}

fn edit_rename_symbol(root: &Path) -> Vec<PathBuf> {
    let path = root.join("a.ts");
    write_file(
        &path,
        r#"export function outer() {
  renamed();
}

export function renamed() {}
"#,
    );
    vec![path]
}

fn setup_delete_file(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
}

fn edit_delete_file(root: &Path) -> Vec<PathBuf> {
    let path = root.join("foo.ts");
    remove_file(&path);
    vec![path]
}

fn setup_barrel(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("barrel.ts"),
        r#"export { foo } from "./foo";
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
    write_file(&root.join("alt.ts"), "export function foo() {}\n");
}

fn edit_delete_barrel(root: &Path) -> Vec<PathBuf> {
    let path = root.join("barrel.ts");
    remove_file(&path);
    vec![path]
}

fn setup_unresolved_import(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { late } from "./late";
export function main() { late(); }
"#,
    );
}

fn edit_add_late_file(root: &Path) -> Vec<PathBuf> {
    let path = root.join("late.ts");
    write_file(&path, "export function late() {}\n");
    vec![path]
}

fn setup_barrel_move(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("barrel.ts"),
        r#"export { foo } from "./foo";
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
    write_file(&root.join("alt.ts"), "export function foo() {}\n");
}

fn edit_move_reexport(root: &Path) -> Vec<PathBuf> {
    let barrel = root.join("barrel.ts");
    let source_file = root.join("foo.ts");
    write_file(
        &barrel,
        r#"export { foo } from "./alt";
"#,
    );
    write_file(&source_file, "export function oldFoo() {}\n");
    vec![barrel, source_file]
}

fn edit_retarget_barrel(root: &Path) -> Vec<PathBuf> {
    let path = root.join("barrel.ts");
    write_file(
        &path,
        r#"export { foo } from "./alt";
"#,
    );
    vec![path]
}

fn setup_defines_and_calls(root: &Path) {
    write_file(
        &root.join("combo.ts"),
        r#"export function caller() {
  callee();
}

export function callee() {}
"#,
    );
}

fn edit_defines_and_calls(root: &Path) -> Vec<PathBuf> {
    let path = root.join("combo.ts");
    write_file(
        &path,
        r#"export function caller() {
  next();
}

export function next() {}
"#,
    );
    vec![path]
}

fn setup_body_only(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("foo.ts"),
        r#"export function foo() {
  return 1;
}
"#,
    );
}

fn edit_body_only(root: &Path) -> Vec<PathBuf> {
    let path = root.join("foo.ts");
    write_file(
        &path,
        r#"export function foo() {
  return 2;
}
"#,
    );
    vec![path]
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    unsafe {
        let mut usage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            Some(usage.ru_maxrss as u64)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Some(usage.ru_maxrss as u64 * 1024)
        }
    }
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

/// Canonicalize the way the dead-code projection reports paths: canonical
/// form with the Windows verbatim (`\\?\`) prefix stripped.
fn canonicalize_like_projection(path: &std::path::Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).expect("canonicalize projection expectation");
    let display = canonical.display().to_string();
    #[cfg(windows)]
    {
        if let Some(stripped) = display.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = display.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(stripped);
        }
    }
    std::path::PathBuf::from(display)
}

#[test]
fn layered_diamond_call_tree_has_total_node_budget() {
    let dir = tempdir().unwrap();
    let mut source = String::new();
    for level in 0..14 {
        source.push_str(&format!(
            "function layer{level}() {{ layer{}(); layer{}(); }}\n",
            level + 1,
            level + 1
        ));
    }
    source.push_str("function layer14() {}\n");
    write_file(&dir.path().join("main.ts"), &source);
    let store =
        CallGraphStore::open(dir.path().join(".store-budget"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();
    fn count(node: &aft::callgraph::CallTreeNode) -> usize {
        1 + node.children.iter().map(count).sum::<usize>()
    }
    let started = std::time::Instant::now();
    let tree = store.call_tree(Path::new("main.ts"), "layer0", 20).unwrap();
    eprintln!(
        "diamond: nodes={}, elapsed={:?}",
        count(&tree),
        started.elapsed()
    );
    assert!(
        count(&tree) <= 1000,
        "total expansion must be bounded independently of depth"
    );
    assert!(tree.truncated > 0);
}

#[test]
fn adapter_call_tree_shares_store_node_budget_and_gap() {
    let dir = tempdir().unwrap();
    let mut source = String::new();
    for level in 0..14 {
        source.push_str(&format!(
            "function layer{level}() {{\n layer{}();\n layer{}();\n }}\n",
            level + 1,
            level + 1
        ));
    }
    source.push_str("function layer14() {}\n");
    write_file(&dir.path().join("main.ts"), &source);
    let store = CallGraphStore::open(
        dir.path().join(".store-adapter-budget"),
        dir.path().to_path_buf(),
    )
    .unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();
    let tree = aft::commands::callgraph_store_adapter::call_tree_result(
        &store,
        Path::new("main.ts"),
        "layer0",
        20,
        true,
    )
    .unwrap();
    fn count(node: &aft::commands::callgraph_store_adapter::StoreCallTreeNode) -> usize {
        1 + node.children.iter().map(count).sum::<usize>()
    }
    assert!(count(&tree) <= 1000);
    let adapter = serde_json::to_value(tree).unwrap();
    let direct =
        serde_json::to_value(store.call_tree(Path::new("main.ts"), "layer0", 20).unwrap()).unwrap();
    assert_eq!(adapter["complete"], false);
    assert_eq!(adapter["gap"], direct["gap"]);
    assert_eq!(adapter["nodes_examined"], 1000);
    let response = aft::protocol::Response::success("budget", adapter);
    let text = aft::subc_format::format_response("call_tree", &response, false);
    assert!(text.contains(aft::callgraph::CALL_TREE_WORK_GAP));
}
