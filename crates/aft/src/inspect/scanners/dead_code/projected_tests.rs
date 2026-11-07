use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::inspect::scanners::dead_code::run_dead_code_scan;
use crate::inspect::{CallgraphSnapshot, InspectCategory, InspectJob, InspectScanSuccess, JobKey};
use crate::parser::SymbolCache;
use serde_json::json;

fn fixture_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");

    let paths = files
        .iter()
        .map(|(relative, contents)| {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, contents).expect("write fixture file");
            path
        })
        .collect::<Vec<_>>();

    (temp_dir, root, paths)
}

fn job(
    root: &Path,
    scope_files: Vec<PathBuf>,
    callgraph_snapshot: Option<CallgraphSnapshot>,
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
        callgraph_snapshot: callgraph_snapshot.map(Arc::new),
    }
}

fn scan(job: InspectJob) -> InspectScanSuccess {
    run_dead_code_scan(&job).outcome.expect("scan succeeds")
}

fn aggregate_has_item(success: &InspectScanSuccess, file: &str, symbol: &str) -> bool {
    success.aggregate["items"]
        .as_array()
        .expect("dead-code items")
        .iter()
        .any(|item| item["file"] == file && item["symbol"] == symbol)
}

fn aggregate_test_only_item<'a>(
    success: &'a InspectScanSuccess,
    file: &str,
    symbol: &str,
) -> Option<&'a serde_json::Value> {
    success.aggregate["test_only_items"]
        .as_array()
        .expect("test-only dead-code items")
        .iter()
        .find(|item| item["file"] == file && item["symbol"] == symbol)
}

fn aggregate_has_item_kind(
    success: &InspectScanSuccess,
    file: &str,
    symbol: &str,
    kind: &str,
) -> bool {
    success.aggregate["items"]
        .as_array()
        .expect("dead-code items")
        .iter()
        .any(|item| item["file"] == file && item["symbol"] == symbol && item["kind"] == kind)
}

#[test]
fn inspect_dead_code_keeps_type_match_constructor_live_without_rescuing_dead_new_symbols() {
    let (_temp_dir, root, _) = fixture_project(&[
        (
            "src/main.rs",
            r#"mod factory;
mod live_widget;
mod planted_dead;

fn main() {
    let _ = factory::make_live();
}
"#,
        ),
        (
            "src/factory.rs",
            r#"use crate::live_widget::LiveWidget;

pub fn make_live() -> LiveWidget {
    LiveWidget::new()
}
"#,
        ),
        (
            "src/live_widget.rs",
            r#"pub struct LiveWidget;

impl LiveWidget {
    pub fn new() -> Self { Self }
}
"#,
        ),
        (
            "src/planted_dead.rs",
            r#"pub fn orphan_function() {}

struct NeverConstructed;

impl NeverConstructed {
    pub fn new() -> Self { Self }
}
"#,
        ),
    ]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let first_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let first_scope_files = first_snapshot.files.clone();
    let first_success = scan(job(&project_root, first_scope_files, Some(first_snapshot)));

    let second_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let second_scope_files = second_snapshot.files.clone();
    let second_success = scan(job(
        &project_root,
        second_scope_files,
        Some(second_snapshot),
    ));

    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "dead-code count must be deterministic across cold builds"
    );

    assert!(
        !aggregate_has_item(&first_success, "src/live_widget.rs", "new"),
        "LiveWidget::new is reached only through a type_match edge and must not be dead: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/planted_dead.rs", "orphan_function"),
        "genuinely-dead pub fn should remain reported: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/planted_dead.rs", "new"),
        "genuinely-dead constructor should remain reported: {:#}",
        first_success.aggregate
    );
}

#[test]
fn rust_value_refs_and_cfg_test_callers_are_cold_deterministic() {
    let (_temp_dir, root, _) = fixture_project(&[
        (
            "src/main.rs",
            r#"mod handlers;

fn main() {
    run();
}

fn run() {
    let lease = Some(());
    let _ = lease.map(crate::handlers::value_live);
    let _ = crate::handlers::NotCallback;
}
"#,
        ),
        (
            "src/handlers.rs",
            r#"pub struct NotCallback;
pub fn value_live(_: ()) {}
pub fn planted_dead() {}
pub fn shadowed() {}
pub fn shadow_parameter(shadowed: fn()) { consume(shadowed); }
pub fn shadow_pattern(maybe: Option<fn()>) {
    if let Some(shadowed) = maybe { consume(shadowed); }
}
pub fn test_only_write() {}
pub fn test_file_only() {}
fn consume(_: fn()) {}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_only_in_tests() {
        crate::handlers::test_only_write();
    }
}
"#,
        ),
        (
            "src/handlers_test.rs",
            r#"fn test_file_caller() {
    crate::handlers::test_file_only();
}
"#,
        ),
    ]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let first_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let first_success = scan(job(
        &project_root,
        first_snapshot.files.clone(),
        Some(first_snapshot.clone()),
    ));

    let second_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let second_success = scan(job(
        &project_root,
        second_snapshot.files.clone(),
        Some(second_snapshot),
    ));

    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "headline dead-code count must be deterministic across cold builds"
    );
    assert_eq!(
        first_success.aggregate["test_only_count"], second_success.aggregate["test_only_count"],
        "test-only dead-code count must be deterministic across cold builds"
    );

    let handler_facts = first_success
        .contributions
        .iter()
        .find(|contribution| contribution.contribution["file"] == "src/handlers.rs")
        .expect("handler contribution");
    assert!(
        handler_facts.contribution["cfg_test_ranges"]
            .as_array()
            .is_some_and(|ranges| !ranges.is_empty()),
        "cfg(test) source ranges must be persisted: {:#}",
        handler_facts.contribution
    );

    assert!(
        !aggregate_has_item(&first_success, "src/handlers.rs", "value_live"),
        "value-position referenced function must be live: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/handlers.rs", "planted_dead"),
        "genuinely dead function is the planted negative control: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item(&first_success, "src/handlers.rs", "shadowed"),
        "a shadowing parameter must not create a value_ref edge to the function item: {:#}",
        first_success.aggregate
    );
    assert!(
        !aggregate_has_item(&first_success, "src/handlers.rs", "test_only_write"),
        "cfg(test)-only target must leave the headline bucket: {:#}",
        first_success.aggregate
    );
    let test_only = aggregate_test_only_item(&first_success, "src/handlers.rs", "test_only_write")
        .unwrap_or_else(|| {
            panic!(
                "missing cfg(test)-only finding: {:#}",
                first_success.aggregate
            )
        });
    assert_eq!(test_only["used_by"], json!(["src/handlers.rs:17"]));
    let test_file_only =
        aggregate_test_only_item(&first_success, "src/handlers.rs", "test_file_only")
            .unwrap_or_else(|| {
                panic!(
                    "missing test-file-only finding: {:#}",
                    first_success.aggregate
                )
            });
    assert_eq!(test_file_only["used_by"], json!(["src/handlers_test.rs"]));
}

#[test]
fn inspect_dead_code_keeps_go_sort_interface_methods_live() {
    let (_temp_dir, root, _) = fixture_project(&[(
        "src/main.go",
        r#"package main

import "sort"

type sortable []int

func main() {
	values := sortable{3, 1, 2}
	sort.Sort(values)
}

func (s sortable) Len() int { return len(s) }

func (s sortable) Less(i, j int) bool { return s[i] < s[j] }

func (s sortable) Swap(i, j int) { s[i], s[j] = s[j], s[i] }

func (s sortable) Unused() {}

func Less(i, j int) bool { return false }
"#,
    )]);
    let project_root = std::fs::canonicalize(&root).expect("canonical fixture root");

    let first_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let first_scope_files = first_snapshot.files.clone();
    let first_success = scan(job(&project_root, first_scope_files, Some(first_snapshot)));

    let second_snapshot =
        crate::callgraph_store::dead_code_projection::snapshot_for_test(&project_root);

    let second_scope_files = second_snapshot.files.clone();
    let second_success = scan(job(
        &project_root,
        second_scope_files,
        Some(second_snapshot),
    ));

    assert_eq!(
        first_success.aggregate["count"], second_success.aggregate["count"],
        "Go sort.Interface fixture dead-code count must be deterministic across cold builds"
    );

    for method in ["Len", "Less", "Swap"] {
        assert!(
            !aggregate_has_item_kind(&first_success, "src/main.go", method, "method"),
            "{method} is invoked by sort.Sort through sort.Interface and must be live: {:#}",
            first_success.aggregate
        );
    }
    assert!(
        aggregate_has_item_kind(&first_success, "src/main.go", "Unused", "method"),
        "unreferenced Go method should remain dead: {:#}",
        first_success.aggregate
    );
    assert!(
        aggregate_has_item_kind(&first_success, "src/main.go", "Less", "function"),
        "dispatch-name liveness must not rescue a same-named plain function: {:#}",
        first_success.aggregate
    );
}
