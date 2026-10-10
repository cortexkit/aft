use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aft::config::Config;
use aft::inspect::scanners::unused_exports::run_unused_exports_scan;
use aft::inspect::{InspectCategory, InspectJob, JobKey};
use aft::parser::SymbolCache;
use serde_json::Value;

fn write_file(path: &Path, contents: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture file");
    path.to_path_buf()
}

fn scan(project_root: &Path, scope_files: Vec<PathBuf>) -> aft::inspect::InspectScanSuccess {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    let job = InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::UnusedExports),
        category: InspectCategory::UnusedExports,
        scope_files,
        project_root: project_root.to_path_buf(),
        inspect_dir: project_root.join(".aft-cache").join("inspect"),
        config: Arc::new(config),
        symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    };

    run_unused_exports_scan(&job)
        .outcome
        .expect("scan succeeds")
}

fn symbols(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("items array")
        .iter()
        .filter_map(|item| item.get("symbol").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[test]
fn inspect_unused_exports_empty_project_reports_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let success = scan(temp.path(), Vec::new());

    assert_eq!(success.aggregate["count"], 0);
    assert_eq!(success.aggregate["scanned_files"], 0);
    // An empty project has no rows to page through. The stored aggregate keeps
    // every row and leaves cutting to render-time paging, so it carries no
    // drill-down cap marker at all.
    assert_eq!(success.aggregate["items"], serde_json::json!([]));
    assert!(success.aggregate.get("drill_down_capped").is_none());
}

#[test]
fn inspect_unused_exports_imported_named_export_is_not_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let exported = write_file(
        &root.join("src/a.ts"),
        "export function used() { return 1; }\n",
    );
    let importer = write_file(
        &root.join("src/b.ts"),
        "import { used } from './a';\nconsole.log(used());\n",
    );

    let success = scan(root, vec![exported, importer]);

    assert_eq!(success.aggregate["count"], 0);
    assert!(success.aggregate["items"].as_array().unwrap().is_empty());
}

#[test]
fn inspect_unused_exports_unimported_function_is_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let file = write_file(
        &root.join("src/a.ts"),
        "export function unused_helper() { return 1; }\n",
    );

    let success = scan(root, vec![file]);

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(success.aggregate["items"][0]["file"], "src/a.ts");
    assert_eq!(success.aggregate["items"][0]["symbol"], "unused_helper");
    assert_eq!(success.aggregate["items"][0]["kind"], "function");
    assert_eq!(success.aggregate["items"][0]["line"], 1);
}

#[test]
fn inspect_unused_exports_default_export_matches_default_import() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let exported = write_file(
        &root.join("src/a.ts"),
        "export default function() { return 1; }\n",
    );
    let importer = write_file(
        &root.join("src/b.ts"),
        "import value from './a';\nconsole.log(value());\n",
    );

    let success = scan(root, vec![exported, importer]);

    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_unused_exports_package_main_entry_is_public_api() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write_file(
        &root.join("package.json"),
        r#"{"name":"fixture","main":"src/index.ts"}"#,
    );
    let entry = write_file(
        &root.join("src/index.ts"),
        "export function public_api() { return 1; }\n",
    );

    let success = scan(root, vec![entry]);

    assert_eq!(success.aggregate["count"], 0);
    assert!(success.aggregate["items"].as_array().unwrap().is_empty());
}

#[test]
fn inspect_unused_exports_package_bin_is_not_public_api_but_main_is() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write_file(
        &root.join("package.json"),
        r#"{"name":"fixture","main":"src/index.ts","bin":"src/cli.ts"}"#,
    );
    let public_entry = write_file(
        &root.join("src/index.ts"),
        "export function public_api() { return 1; }\n",
    );
    let bin_entry = write_file(
        &root.join("src/cli.ts"),
        "export function unused_cli_helper() { return 1; }\n",
    );

    let success = scan(root, vec![public_entry, bin_entry]);

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(success.aggregate["items"][0]["file"], "src/cli.ts");
    assert_eq!(success.aggregate["items"][0]["symbol"], "unused_cli_helper");
}

#[test]
fn inspect_unused_exports_namespace_import_uses_only_accessed_members() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let exported = write_file(
        &root.join("src/a.ts"),
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let importer = write_file(
        &root.join("src/b.ts"),
        "import * as api from './a';\nconsole.log(api.used());\n",
    );

    let success = scan(root, vec![exported, importer]);

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(
        symbols(&success.aggregate["items"]),
        vec!["unused".to_string()]
    );
    assert_eq!(success.aggregate["uncertain_count"], 0);
}

#[test]
fn inspect_unused_exports_namespace_import_used_as_value_marks_all_exports_uncertain() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let exported = write_file(
        &root.join("src/a.ts"),
        "export function first() { return 1; }\nexport function second() { return 2; }\n",
    );
    let importer = write_file(
        &root.join("src/b.ts"),
        "import * as api from './a';\nconsume(api);\n",
    );

    let success = scan(root, vec![exported, importer]);

    assert_eq!(success.aggregate["count"], 0, "{:#}", success.aggregate);
    assert_eq!(success.aggregate["uncertain_count"], 2);
    assert_eq!(
        symbols(&success.aggregate["uncertain_items"]),
        vec!["first".to_string(), "second".to_string()]
    );
}

#[test]
fn inspect_unused_exports_non_js_ts_files_contribute_empty_and_are_skipped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let rust_file = write_file(&root.join("src/lib.rs"), "pub fn ignored() {}\n");

    let success = scan(root, vec![rust_file]);

    assert_eq!(success.aggregate["count"], 0);
    assert_eq!(success.aggregate["languages_skipped"][0], "rust");
    assert_eq!(success.contributions.len(), 1);
    assert!(success.contributions[0].contribution["exports"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(success.contributions[0].contribution["imports"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn inspect_unused_exports_skips_kotlin_export_like_declarations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let kotlin_file = write_file(
        &root.join("src/Sample.kt"),
        "class UnreferencedService { fun run() = Unit }\n",
    );

    let success = scan(root, vec![kotlin_file]);

    assert_eq!(success.aggregate["count"], 0);
    assert!(success.aggregate["items"].as_array().unwrap().is_empty());
    assert_eq!(
        success.aggregate["languages_skipped"],
        serde_json::json!(["kotlin"])
    );
}

/// The scanner stores every unused export past the former 100-row drill-down
/// cap, so render-time paging (`topK` plus `offset`) can reach all of them.
/// `inspect_command_test::inspect_unused_exports_pages_101_items_past_the_former_cap`
/// checks the paging itself.
#[test]
fn inspect_unused_exports_stores_every_item_past_one_hundred_for_paging() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let source = (0..101)
        .map(|index| format!("export function unused_{index}() {{ return {index}; }}\n"))
        .collect::<String>();
    let file = write_file(&root.join("src/a.ts"), &source);

    let success = scan(root, vec![file]);

    assert_eq!(success.aggregate["count"], 101);
    let stored = symbols(&success.aggregate["items"]);
    assert_eq!(stored.len(), 101);
    for symbol in ["unused_0", "unused_100"] {
        assert!(stored.contains(&symbol.to_string()), "missing {symbol}");
    }
    assert!(success.aggregate.get("drill_down_capped").is_none());
}
