use aft::callgraph_store::CallGraphStore;
use aft::commands::callgraph_store_adapter;
use rusqlite::params;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

#[test]
fn rust_param_receiver_type_match_surfaces_precise_edges_for_store_ops() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/context.rs"),
        r#"pub struct AppContext;

impl AppContext {
    pub fn callgraph_store_for_ops(&self) -> usize {
        1
    }
}
"#,
    );
    for name in [
        "callers",
        "call_tree",
        "impact",
        "trace_to",
        "trace_to_symbol",
    ] {
        write_file(
            &root.join(format!("src/commands/{name}.rs")),
            &format!(
                r#"use crate::context::AppContext;

pub fn handle_{name}(ctx: &AppContext) -> usize {{
    ctx.callgraph_store_for_ops()
}}
"#
            ),
        );
    }

    let store = build_store(&root, "rust-type-match", &project_files(&root));
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/context.rs"),
        "AppContext::callgraph_store_for_ops",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 5, "callers output: {callers:#}");
    assert!(
        entries
            .iter()
            .all(|entry| entry["approximate"] == false && entry["resolved_by"] == "type_match"),
        "all callers should be marked as precise type_match: {callers:#}"
    );

    let impact = json(callgraph_store_adapter::impact_result(
        &store,
        &root.join("src/context.rs"),
        "AppContext::callgraph_store_for_ops",
        1,
        true,
    ));
    let impact_callers = impact["callers"].as_array().unwrap();
    assert_eq!(impact_callers.len(), 5, "impact output: {impact:#}");
    assert!(impact_callers
        .iter()
        .all(|caller| { caller["approximate"] == false && caller["resolved_by"] == "type_match" }));

    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/commands/callers.rs"),
        "handle_callers",
        1,
        true,
    ));
    let child = tree["children"].as_array().unwrap().first().unwrap();
    assert_eq!(child["name"], "AppContext::callgraph_store_for_ops");
    assert_eq!(child["approximate"], false, "call_tree output: {tree:#}");
    assert_eq!(child["resolved_by"], "type_match");

    let trace = json(callgraph_store_adapter::trace_to_result(
        &store,
        &root.join("src/context.rs"),
        "AppContext::callgraph_store_for_ops",
        2,
        true,
    ));
    let target_hop = trace["paths"][0]["hops"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(
        target_hop["approximate"], false,
        "trace_to output: {trace:#}"
    );
    assert_eq!(target_hop["resolved_by"], "type_match");

    let path = json(callgraph_store_adapter::trace_to_symbol_result(
        &store,
        &root.join("src/commands/callers.rs"),
        "handle_callers",
        "callgraph_store_for_ops",
        Some(&root.join("src/context.rs")),
        2,
        true,
    ));
    let target_hop = path["path"]
        .as_array()
        .unwrap_or_else(|| panic!("trace_to_symbol output: {path:#}"))
        .last()
        .unwrap();
    assert_eq!(
        target_hop["approximate"], false,
        "trace_to_symbol output: {path:#}"
    );
    assert_eq!(target_hop["resolved_by"], "type_match");

    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let persisted: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE e.provenance = 'type_match'
               AND r.status = 'unresolved'
               AND e.target_symbol = 'AppContext::callgraph_store_for_ops'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, 5, "type_match edges must leave refs unresolved");

    let name_matches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges
             WHERE provenance = 'name_match'
               AND target_symbol = 'AppContext::callgraph_store_for_ops'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        name_matches, 0,
        "typed receiver should not fall back to name_match"
    );
}

#[test]
fn rust_self_receiver_type_match_resolves_precisely() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"pub struct Foo;

impl Foo {
    pub fn method(&self) -> usize {
        1
    }

    pub fn caller(&self) -> usize {
        self.method()
    }
}
"#,
    );

    let store = build_store(&root, "rust-self-type-match", &project_files(&root));
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/lib.rs"),
        "Foo::method",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 1, "self callers output: {callers:#}");
    assert_eq!(entries[0]["symbol"], "Foo::caller");
    assert_eq!(entries[0]["approximate"], false);
    assert_eq!(entries[0]["resolved_by"], "type_match");
}

#[test]
fn rust_unknown_stdlib_expect_does_not_name_match_project_expect() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"pub struct Parser;

impl Parser {
    pub fn expect(&self) {}
}

pub fn noisy_stdlib_calls() {
    let result: Result<&str, &str> = Ok("ok");
    let _ = result.expect("expected ok");
    let other: Result<usize, &str> = Ok(1);
    let _ = other.expect("expected number");
    let optional = Some("value");
    let _ = optional.expect("expected value");
}
"#,
    );

    let store = build_store(&root, "rust-expect-denylist", &project_files(&root));
    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let expect_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE target_symbol = 'Parser::expect'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        expect_edges, 0,
        "unknown local Result/Option::expect calls must not edge to Parser::expect"
    );
}

#[test]
fn rust_direct_self_field_with_stdlib_type_does_not_name_match_project_method() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("main.rs"),
        r#"mod other;

use std::path::PathBuf;

struct Holder {
    path: PathBuf,
}

impl Holder {
    fn check(&self) {
        let _ = self. path.as_path();
    }
}

fn main() {
    let holder = Holder {
        path: PathBuf::from("."),
    };
    holder.check();
}
"#,
    );
    write_file(
        &root.join("other.rs"),
        r#"pub struct PrinterPath;

impl PrinterPath {
    pub fn as_path(&self) {}
}
"#,
    );

    let store = build_store(&root, "rust-stdlib-field-method", &project_files(&root));
    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("main.rs"),
        "Holder::check",
        1,
        true,
    ));
    assert!(
        tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|child| child["name"] != "PrinterPath::as_path"),
        "PathBuf::as_path must not resolve to the unrelated project method: {tree:#}"
    );

    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("other.rs"),
        "PrinterPath::as_path",
        1,
        true,
    ));
    assert!(
        flattened_callers(&callers)
            .iter()
            .all(|entry| entry["symbol"] != "Holder::check"),
        "PrinterPath::as_path must not report Holder::check as a caller: {callers:#}"
    );
}

#[test]
fn rust_direct_self_field_resolves_by_declared_type() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"struct Engine;

impl Engine {
    fn start(&self) {}
}

struct Unrelated;

impl Unrelated {
    fn start(&self) {}
}

struct Car {
    engine: Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#,
    );

    let store = build_store(&root, "rust-direct-self-field", &project_files(&root));
    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/lib.rs"),
        "Car::run",
        1,
        true,
    ));
    let children = tree["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "call_tree: {tree:#}");
    let child = &children[0];
    assert_eq!(child["name"], "Engine::start", "call_tree: {tree:#}");
    assert_eq!(child["approximate"], false, "call_tree: {tree:#}");
    assert_eq!(child["resolved_by"], "type_match", "call_tree: {tree:#}");

    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/lib.rs"),
        "Engine::start",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 1, "callers: {callers:#}");
    assert_eq!(entries[0]["symbol"], "Car::run");
    assert_eq!(entries[0]["approximate"], false, "callers: {callers:#}");
    assert_eq!(
        entries[0]["resolved_by"], "type_match",
        "callers: {callers:#}"
    );
}

#[test]
fn rust_scoped_field_type_does_not_match_unrelated_same_named_method() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/actual.rs"),
        r#"pub struct Engine;
"#,
    );
    write_file(
        &root.join("src/unrelated.rs"),
        r#"pub struct Engine;

impl Engine {
    pub fn start(&self) {}
}
"#,
    );
    write_file(
        &root.join("src/lib.rs"),
        r#"mod actual;
mod unrelated;

struct Car {
    engine: actual::Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#,
    );

    let store = build_store(&root, "rust-scoped-field-type", &project_files(&root));
    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/lib.rs"),
        "Car::run",
        1,
        true,
    ));
    assert!(
        tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|child| child["name"] != "Engine::start"),
        "unresolved actual::Engine must not match unrelated Engine::start: {tree:#}"
    );

    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/unrelated.rs"),
        "Engine::start",
        1,
        true,
    ));
    assert!(
        flattened_callers(&callers)
            .iter()
            .all(|entry| entry["symbol"] != "Car::run"),
        "unrelated Engine::start must not report Car::run as a caller: {callers:#}"
    );
}

#[test]
fn rust_imported_field_type_does_not_match_unrelated_same_named_method() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/actual.rs"),
        r#"pub struct Engine;
"#,
    );
    write_file(
        &root.join("src/unrelated.rs"),
        r#"pub struct Engine;

impl Engine {
    pub fn start(&self) {}
}
"#,
    );
    write_file(
        &root.join("src/lib.rs"),
        r#"mod actual;
mod unrelated;

use actual::Engine;

struct Car {
    engine: Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#,
    );

    let store = build_store(&root, "rust-imported-field-type", &project_files(&root));
    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/lib.rs"),
        "Car::run",
        1,
        true,
    ));
    assert!(
        tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|child| child["name"] != "Engine::start"),
        "imported Engine must not match unrelated Engine::start: {tree:#}"
    );

    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/unrelated.rs"),
        "Engine::start",
        1,
        true,
    ));
    assert!(
        flattened_callers(&callers)
            .iter()
            .all(|entry| entry["symbol"] != "Car::run"),
        "unrelated Engine::start must not report Car::run as a caller: {callers:#}"
    );
}

#[test]
fn rust_known_self_type_without_method_does_not_fall_back_to_name_match() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"pub struct Foo;
pub struct Parser;

impl Foo {
    pub fn caller(&self) {
        self.bespoke_missing();
    }
}

impl Parser {
    pub fn bespoke_missing(&self) {}
}
"#,
    );

    let store = build_store(&root, "rust-self-no-fallback", &project_files(&root));
    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let parser_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE target_symbol = 'Parser::bespoke_missing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        parser_edges, 0,
        "known Foo receiver must not fall back to Parser::bespoke_missing"
    );

    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/lib.rs"),
        "Foo::caller",
        1,
        true,
    ));
    let child = tree["children"].as_array().unwrap().first().unwrap();
    assert_eq!(
        child["name"], "bespoke_missing",
        "call_tree output: {tree:#}"
    );
    assert_eq!(child["resolved"], false);
    assert!(child.get("approximate").is_none());
    assert!(child.get("resolved_by").is_none());
}

#[test]
fn rust_distinctive_unknown_receiver_still_uses_name_match() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"pub struct Parser;

impl Parser {
    pub fn bespoke_project_action(&self) {}
}

pub fn entry() {
    let service = Parser;
    service.bespoke_project_action();
}
"#,
    );

    let store = build_store(&root, "rust-distinctive-name-match", &project_files(&root));
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/lib.rs"),
        "Parser::bespoke_project_action",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 1, "distinctive callers output: {callers:#}");
    assert_eq!(entries[0]["symbol"], "entry");
    assert_eq!(entries[0]["approximate"], true);
    assert_eq!(entries[0]["resolved_by"], "name_match");
}

#[test]
fn typescript_class_method_name_match_is_language_agnostic() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("worker.ts"),
        r#"export class Worker {
  run() {
    return 1;
  }
}
"#,
    );
    write_file(
        &root.join("entry.ts"),
        r#"import { Worker } from './worker';

export function entry(worker: Worker) {
  return worker.run();
}
"#,
    );

    let store = build_store(&root, "ts-name-match", &project_files(&root));
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("worker.ts"),
        "Worker::run",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 1, "TS callers output: {callers:#}");
    assert_eq!(entries[0]["symbol"], "entry");
    assert_eq!(entries[0]["approximate"], true);
    assert_eq!(entries[0]["resolved_by"], "name_match");
}

#[test]
fn name_match_keeps_unknown_external_methods_as_noise() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"pub fn noisy(input: Option<String>) -> String {
    let cloned = input.clone();
    cloned.unwrap()
}
"#,
    );

    let store = build_store(&root, "noise", &project_files(&root));
    assert_eq!(count_name_match_edges(&store), 0);
}

#[test]
fn ambiguous_methods_below_score_threshold_do_not_create_spurious_edges() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("ambiguous.ts"),
        r#"class Alpha {
  handle() {}
}

class Beta {
  handle() {}
}

export function entry(service: { handle(): void }) {
  service.handle();
}
"#,
    );

    let store = build_store(&root, "ambiguous", &project_files(&root));
    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let handle_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE provenance = 'name_match' AND target_symbol LIKE '%::handle'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        handle_edges, 0,
        "ambiguous receiver should not pick an arbitrary handle"
    );

    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("ambiguous.ts"),
        "entry",
        1,
        true,
    ));
    let child = tree["children"].as_array().unwrap().first().unwrap();
    assert_eq!(child["name"], "handle", "call_tree output: {tree:#}");
    assert_eq!(child["resolved"], false);
    assert!(child.get("approximate").is_none());
}

#[test]
fn scored_ambiguous_methods_pick_receiver_matching_candidate() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("engines.ts"),
        r#"class PermissionRuleEngine {
  evaluate() { return true; }
}

class BillingRuleEngine {
  evaluate() { return false; }
}

export function entry(permissionRuleEngine: PermissionRuleEngine) {
  return permissionRuleEngine.evaluate();
}
"#,
    );

    let store = build_store(&root, "scored", &project_files(&root));
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("engines.ts"),
        "PermissionRuleEngine::evaluate",
        1,
        true,
    ));
    let entries = flattened_callers(&callers);
    assert_eq!(entries.len(), 1, "scored callers output: {callers:#}");
    assert_eq!(entries[0]["symbol"], "entry");
    assert_eq!(entries[0]["approximate"], true);

    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let billing_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE provenance = 'name_match' AND target_symbol = ?1",
            params!["BillingRuleEngine::evaluate"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        billing_edges, 0,
        "receiver scoring should not cross-edge to BillingRuleEngine"
    );
}

#[test]
fn rust_nested_same_named_type_does_not_match_root_field_type() {
    let dir = tempdir().unwrap();
    let root = canonical_root(dir.path());
    write_file(
        &root.join("src/lib.rs"),
        r#"struct Engine;

struct Car {
    engine: Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}

mod nested {
    pub struct Engine;

    impl Engine {
        pub fn start(&self) {}
    }
}
"#,
    );

    let store = build_store(
        &root,
        "rust-nested-same-named-field-type",
        &project_files(&root),
    );
    let tree = json(callgraph_store_adapter::call_tree_result(
        &store,
        &root.join("src/lib.rs"),
        "Car::run",
        1,
        true,
    ));
    let precise_tree_edge = tree["children"].as_array().unwrap().iter().any(|child| {
        child["name"] == "Engine::start"
            && child["resolved_by"] == "type_match"
            && child["approximate"] == false
    });
    let callers = json(callgraph_store_adapter::callers_result(
        &store,
        &root.join("src/lib.rs"),
        "Engine::start",
        1,
        true,
    ));
    let precise_reverse_edge = flattened_callers(&callers).iter().any(|entry| {
        entry["symbol"] == "Car::run"
            && entry["resolved_by"] == "type_match"
            && entry["approximate"] == false
    });
    assert!(
        !precise_tree_edge && !precise_reverse_edge,
        "root Engine field must not precisely match nested Engine::start; call_tree: {tree:#}; callers: {callers:#}"
    );
}

fn canonical_root(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn build_store(root: &Path, name: &str, files: &[PathBuf]) -> CallGraphStore {
    let store =
        CallGraphStore::open(root.join(format!(".{name}-store")), root.to_path_buf()).unwrap();
    store.cold_build(files).unwrap();
    store
}

fn json<T: serde::Serialize>(value: Result<T, aft::callgraph_store::CallGraphStoreError>) -> Value {
    serde_json::to_value(value.unwrap()).unwrap()
}

fn flattened_callers(result: &Value) -> Vec<&Value> {
    result["callers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["callers"].as_array().unwrap().iter())
        .collect()
}

fn count_name_match_edges(store: &CallGraphStore) -> i64 {
    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE provenance = 'name_match'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn project_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_project_files(root, &mut files);
    files.sort();
    files
}

fn collect_project_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            collect_project_files(&path, files);
            continue;
        }
        if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("rs" | "ts")
        ) {
            files.push(path);
        }
    }
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}
