use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aft::config::Config;
use aft::inspect::scanners::metrics::run_metrics_scan;
use aft::inspect::{InspectCategory, InspectJob, JobKey, JobScope};
use aft::parser::{SharedSymbolCache, SymbolCache};
use aft::symbols::{Range, Symbol, SymbolKind};

fn write_file(root: &Path, relative_path: &str, content: &str) -> PathBuf {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(&path, content).expect("write fixture file");
    path
}

fn line_content(line_count: usize) -> String {
    (0..line_count)
        .map(|index| format!("line_{index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn shared_cache() -> SharedSymbolCache {
    Arc::new(RwLock::new(SymbolCache::new()))
}

fn symbol(name: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        range: Range {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 1,
        },
        signature: None,
        scope_chain: Vec::new(),
        exported: false,
        parent: None,
    }
}

fn insert_symbol_count(cache: &SharedSymbolCache, path: &Path, count: usize) {
    let content = fs::read(path).expect("read fixture for hash");
    let metadata = fs::metadata(path).expect("stat fixture");
    let symbols = (0..count)
        .map(|index| symbol(&format!("symbol_{index}")))
        .collect::<Vec<_>>();

    cache.write().expect("write symbol cache").insert(
        path.to_path_buf(),
        metadata.modified().expect("fixture mtime"),
        metadata.len(),
        blake3::hash(&content),
        symbols,
    );
}

fn metrics_job(root: &Path, files: Vec<PathBuf>, symbol_cache: SharedSymbolCache) -> InspectJob {
    let scope = JobScope::for_project(root.to_path_buf());
    InspectJob {
        job_id: 1,
        key: JobKey::for_category_scope(InspectCategory::Metrics, &scope),
        category: InspectCategory::Metrics,
        scope_files: files,
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".aft-cache").join("inspect"),
        config: Arc::new(Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        }),
        symbol_cache,
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    }
}

fn metrics_payload(job: &InspectJob) -> serde_json::Value {
    run_metrics_scan(job)
        .outcome
        .expect("metrics scan should succeed")
        .aggregate
}

#[test]
fn inspect_metrics_empty_project_reports_zero_files() {
    let project = crate::test_helpers::tempdir();
    let cache = shared_cache();
    let job = metrics_job(project.path(), Vec::new(), cache);

    let payload = metrics_payload(&job);

    assert_eq!(payload["files"], 0);
    assert_eq!(payload["symbols"], 0);
    assert_eq!(payload["loc"], 0);
    assert_eq!(payload["by_language"].as_object().unwrap().len(), 0);
    assert_eq!(payload["top_files_by_loc"].as_array().unwrap().len(), 0);
}

#[test]
fn inspect_metrics_mixed_rust_and_typescript_uses_cached_symbol_counts() {
    let project = crate::test_helpers::tempdir();
    let rust_file = write_file(
        project.path(),
        "src/lib.rs",
        "pub fn alpha() {}\nstruct Beta;\n",
    );
    let ts_file = write_file(
        project.path(),
        "web/app.ts",
        "export function gamma() {}\nexport class Delta {}\n",
    );
    let cache = shared_cache();
    insert_symbol_count(&cache, &rust_file, 2);
    insert_symbol_count(&cache, &ts_file, 3);
    let job = metrics_job(project.path(), vec![rust_file, ts_file], cache);

    let payload = metrics_payload(&job);

    assert_eq!(payload["files"], 2);
    assert_eq!(payload["symbols"], 5);
    assert_eq!(payload["by_language"]["rust"]["file_count"], 1);
    assert_eq!(payload["by_language"]["rust"]["symbol_count"], 2);
    assert_eq!(payload["by_language"]["typescript"]["file_count"], 1);
    assert_eq!(payload["by_language"]["typescript"]["symbol_count"], 3);
}

#[test]
fn inspect_metrics_loc_totals_match_known_fixture() {
    let project = crate::test_helpers::tempdir();
    let first = write_file(project.path(), "src/first.rs", "alpha\nbeta\n");
    let second = write_file(project.path(), "src/second.ts", "one\ntwo\nthree");
    let third = write_file(project.path(), "README.md", "single");
    let cache = shared_cache();
    let job = metrics_job(project.path(), vec![first, second, third], cache);

    let payload = metrics_payload(&job);

    assert_eq!(payload["files"], 3);
    assert_eq!(payload["loc"], 7);
}

#[test]
fn inspect_metrics_top_files_by_loc_is_sorted_descending_and_capped() {
    let project = crate::test_helpers::tempdir();
    let mut files = Vec::new();
    let cache = shared_cache();
    for line_count in 1..=25 {
        let path = write_file(
            project.path(),
            &format!("src/file_{line_count:02}.rs"),
            &line_content(line_count),
        );
        insert_symbol_count(&cache, &path, 0);
        files.push(path);
    }
    let job = metrics_job(project.path(), files, cache);

    let payload = metrics_payload(&job);
    let top_files = payload["top_files_by_loc"]
        .as_array()
        .expect("top files array");

    assert_eq!(top_files.len(), 20);
    assert_eq!(top_files.first().unwrap()["loc"], 25);
    assert_eq!(top_files.last().unwrap()["loc"], 6);
    for pair in top_files.windows(2) {
        let left = pair[0]["loc"].as_u64().unwrap();
        let right = pair[1]["loc"].as_u64().unwrap();
        assert!(
            left >= right,
            "top files should be sorted desc: {left} < {right}"
        );
    }
}
