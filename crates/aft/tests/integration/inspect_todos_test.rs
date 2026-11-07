use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aft::config::Config;
use aft::inspect::scanners::todos::run_todos_scan;
use aft::inspect::{InspectCategory, InspectJob, InspectScanSuccess, JobKey, JobScope};
use aft::parser::SymbolCache;
use serde_json::json;

use crate::test_helpers::AftProcess;

fn inspect_todos_job(project_root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    let scope = JobScope::for_project(project_root.to_path_buf());
    InspectJob {
        job_id: 1,
        key: JobKey::for_category_scope(InspectCategory::Todos, &scope),
        category: InspectCategory::Todos,
        scope_files,
        project_root: project_root.to_path_buf(),
        inspect_dir: project_root.join(".aft-cache").join("inspect"),
        config: Arc::new(config),
        symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    }
}

fn run_job(job: &InspectJob) -> InspectScanSuccess {
    run_todos_scan(job).outcome.expect("todos scan succeeds")
}

#[test]
fn inspect_todos_empty_directory_returns_zero_count() {
    let temp_dir = crate::test_helpers::tempdir();
    let job = inspect_todos_job(temp_dir.path(), Vec::new());

    let success = run_job(&job);

    assert_eq!(success.aggregate["count"], 0);
    assert_eq!(success.aggregate["items"].as_array().unwrap().len(), 0);
    assert_eq!(success.aggregate["drill_down_capped"], false);
}

#[test]
fn inspect_todos_counts_mixed_markers_and_items() {
    let temp_dir = crate::test_helpers::tempdir();
    let src = temp_dir.path().join("src");
    fs::create_dir_all(&src).expect("create src");
    let rust_file = src.join("app.rs");
    fs::write(
        &rust_file,
        "// TODO(alice): implement retry\n\
         fn main() {\n\
         /* FIXME handle errors */\n\
         /**\n\
          * HACK: temporary shim\n\
          * XXX: doc block item\n\
          */\n\
         }\n",
    )
    .expect("write Rust fixture");
    let python_file = src.join("worker.py");
    fs::write(&python_file, "# BUG: hash-style bug\n").expect("write Python fixture");
    let job = inspect_todos_job(temp_dir.path(), vec![rust_file, python_file]);

    let success = run_job(&job);

    assert_eq!(success.aggregate["count"], 5);
    assert_eq!(success.aggregate["by_kind"]["TODO"], 1);
    assert_eq!(success.aggregate["by_kind"]["FIXME"], 1);
    assert_eq!(success.aggregate["by_kind"]["HACK"], 1);
    assert_eq!(success.aggregate["by_kind"]["XXX"], 1);
    assert_eq!(success.aggregate["by_kind"]["BUG"], 1);

    let items = success.aggregate["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[0]["file"], "src/app.rs");
    assert_eq!(items[0]["line"], 1);
    assert_eq!(items[0]["marker"], "TODO");
    assert_eq!(items[0]["author"], "alice");
    assert_eq!(items[0]["text"], "implement retry");
    assert_eq!(items[2]["marker"], "HACK");
    assert_eq!(items[2]["text"], "temporary shim");
}

#[test]
fn inspect_todos_requires_comment_syntax_before_marker() {
    let temp_dir = crate::test_helpers::tempdir();
    let file = temp_dir.path().join("strings.rs");
    fs::write(
        &file,
        "let message = \"TODO not a real todo\";\n\
         let hash = \"# FIXME also not a real todo\";\n\
         // BUG: real comment\n",
    )
    .expect("write fixture");
    let job = inspect_todos_job(temp_dir.path(), vec![file]);

    let success = run_job(&job);

    // Parser-supported files accept markers only from syntax-tree comment nodes;
    // marker text in code or string literals must not contribute findings.
    assert_eq!(success.aggregate["count"], 1);
    let items = success.aggregate["items"].as_array().unwrap();
    assert_eq!(items[0]["marker"], "BUG");
    assert_eq!(items[0]["text"], "real comment");
}

#[test]
fn inspect_todos_caps_drill_down_at_one_hundred_items() {
    let temp_dir = crate::test_helpers::tempdir();
    let file = temp_dir.path().join("many.rs");
    let content = (0..200)
        .map(|index| format!("// TODO item {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file, content).expect("write fixture");
    let job = inspect_todos_job(temp_dir.path(), vec![file]);

    let success = run_job(&job);

    assert_eq!(success.aggregate["count"], 200);
    assert_eq!(success.aggregate["by_kind"]["TODO"], 200);
    assert_eq!(success.aggregate["items"].as_array().unwrap().len(), 100);
    assert_eq!(success.aggregate["drill_down_capped"], true);
}

#[test]
fn inspect_todos_skips_binary_files_without_panic() {
    let temp_dir = crate::test_helpers::tempdir();
    let file = temp_dir.path().join("image.bin");
    fs::write(&file, [0, 159, 146, 150]).expect("write binary fixture");
    let job = inspect_todos_job(temp_dir.path(), vec![file]);

    let success = run_job(&job);

    assert!(success.scanned_files.is_empty());
    assert_eq!(success.aggregate["count"], 0);
    assert_eq!(success.aggregate["items"].as_array().unwrap().len(), 0);
}

#[test]
fn inspect_todos_unsupported_text_fallback_ignores_quoted_comment_tokens() {
    let temp_dir = crate::test_helpers::tempdir();
    let file = temp_dir.path().join("notes.txt");
    fs::write(
        &file,
        "plain = \" // TODO: not a comment\"\n\
         escaped = \"\\\" // FIXME: not a comment\"\n\
         raw = r#\"// HACK: not a comment\"#\n\
         template = `-- XXX: not a comment`\n\
         # BUG: real fallback comment\n",
    )
    .expect("write fallback fixture");
    let job = inspect_todos_job(temp_dir.path(), vec![file]);

    let success = run_job(&job);

    assert_eq!(success.aggregate["count"], 1);
    assert_eq!(success.aggregate["by_kind"]["BUG"], 1);
    assert_eq!(
        success.aggregate["items"][0]["text"],
        "real fallback comment"
    );
}

#[test]
fn inspect_todos_binary_uses_comment_nodes_without_regressing_real_comments() {
    let temp_dir = crate::test_helpers::tempdir();
    let src = temp_dir.path().join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("strings.rs"),
        "fn main() {\n\
             let plain = \" // TODO: not a comment\";\n\
             let escaped = \"\\\" // FIXME: not a comment\";\n\
             let raw = r#\"// TODO: not a comment\"#;\n\
         }\n\
         // TODO: real Rust line comment\n\
         /* FIXME: real Rust block comment */\n",
    )
    .expect("write Rust fixture");
    fs::write(
        src.join("template.ts"),
        "const fake = `// HACK: not a comment`;\n",
    )
    .expect("write TypeScript fixture");
    fs::write(src.join("worker.py"), "# HACK: real hash comment\n").expect("write Python fixture");
    fs::write(
        src.join("page.html"),
        "<script>const fake = \"<!-- TODO: not a comment -->\";</script>\n\
         <!-- XXX: real HTML comment -->\n",
    )
    .expect("write HTML fixture");
    fs::write(
        temp_dir.path().join("README.md"),
        "<!-- BUG: real Markdown comment -->\n",
    )
    .expect("write Markdown fixture");
    fs::write(src.join("script.lua"), "-- BUG: real Lua comment\n").expect("write Lua fixture");

    let mut aft = AftProcess::spawn();
    let configured = aft.configure(temp_dir.path());
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let response = aft.send(
        &json!({
            "id": "inspect-todos-comments",
            "command": "tool_call",
            "name": "inspect",
            "arguments": { "sections": ["todos"] }
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:?}");
    assert_eq!(response["summary"]["todos"]["count"], 6, "{response:?}");
    assert_eq!(response["summary"]["todos"]["by_kind"]["TODO"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["FIXME"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["HACK"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["XXX"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["BUG"], 2);

    let items = response["details"]["todos"]
        .as_array()
        .expect("TODO details array");
    assert!(items.iter().all(|item| {
        !item["text"]
            .as_str()
            .is_some_and(|text| text.contains("not a comment"))
    }));
}
