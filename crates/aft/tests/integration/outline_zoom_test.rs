use super::helpers::AftProcess;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&request.to_string())
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) {
    std::os::unix::fs::symlink(src, dst).expect("create symlink");
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) {
    std::os::windows::fs::symlink_dir(src, dst).expect("create symlink");
}

#[test]
fn outline_single_file_returns_tree_text_with_signatures_and_variables() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "single.ts",
        r#"export function greet(name: string): string {
  return name;
}

class Worker {
  run(task: string): void {
    console.log(task);
  }
}

export const answer = 42;
let localCount = 0;
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "outline-single", "command": "outline", "file": file}),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);
    assert!(
        resp.get("symbols").is_none(),
        "outline should not return JSON symbols"
    );

    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.starts_with("single.ts\n"),
        "unexpected header: {text:?}"
    );
    assert!(
        text.contains("E fn   function greet(name: string): string 1:3"),
        "single-file outline should include function signature: {text}"
    );
    assert!(
        text.contains("- cls  class Worker 5:9"),
        "single-file outline should include class signature: {text}"
    );
    assert!(
        text.contains(".- mth  run(task: string): void 6:8"),
        "single-file outline should include nested method signature: {text}"
    );
    assert!(
        text.contains("E var  const answer = 42; 11:11"),
        "top-level const should render as variable: {text}"
    );
    assert!(
        text.contains("- var  let localCount = 0; 12:12"),
        "top-level let should render as variable: {text}"
    );
    assert!(
        !text.trim_start().starts_with('{'),
        "outline text should not be JSON: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_directory_skips_symlink_loops() {
    let dir = TempDir::new().unwrap();
    write_file(
        dir.path(),
        "src/main.ts",
        "export function reachable(): void {}\n",
    );
    create_dir_symlink(dir.path(), &dir.path().join("src/loop"));

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "outline-symlink-loop", "command": "outline", "directory": dir.path()}),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {resp:?}");
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("reachable"),
        "outline missed real file: {text}"
    );
    assert!(
        !text.contains("loop/src"),
        "outline followed symlink loop: {text}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn outline_multi_file_returns_relative_tree_text_without_signatures_for_multiple_languages() {
    let dir = TempDir::new().unwrap();
    let ts = write_file(
        dir.path(),
        "src/service.ts",
        "export function greet(name: string): string { return name; }\nexport const answer = 42;\n",
    );
    let rs = write_file(
        dir.path(),
        "core/model.rs",
        "pub struct Config {}\npub fn compute() -> i32 { 1 }\n",
    );
    let py = write_file(
        dir.path(),
        "scripts/tool.py",
        "class Worker:\n    def run(self):\n        return 1\n",
    );
    let md = write_file(dir.path(), "docs/readme.md", "# Title\n\n## Details\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-multi",
            "command": "outline",
            "files": [ts, rs, py, md],
        }),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);
    let text = resp["text"].as_str().expect("outline text");

    assert!(
        text.contains("core/\n  model.rs\n"),
        "should use relative Rust path: {text}"
    );
    assert!(
        text.contains("docs/\n  readme.md\n"),
        "should use relative Markdown path: {text}"
    );
    assert!(
        text.contains("scripts/\n  tool.py\n"),
        "should use relative Python path: {text}"
    );
    assert!(
        text.contains("src/\n  service.ts\n"),
        "should use relative TypeScript path: {text}"
    );
    assert!(
        !text.contains(dir.path().to_str().unwrap()),
        "multi-file outline should not contain absolute paths: {text}"
    );
    assert!(
        text.contains("E fn   greet 1:1") && text.contains("E var  answer 2:2"),
        "TypeScript symbols should render without signatures: {text}"
    );
    assert!(
        text.contains("st") && text.contains("Config") && text.contains("compute"),
        "Rust symbols should be present: {text}"
    );
    assert!(
        text.contains("cls") && text.contains("Worker") && text.contains("run"),
        "Python symbols should be present: {text}"
    );
    assert!(
        text.contains(" h ") && text.contains("Title") && text.contains("Details"),
        "Markdown headings should be present: {text}"
    );
    assert!(
        !text.contains("function greet(name: string): string")
            && !text.contains("const answer = 42;"),
        "multi-file outline should omit signatures: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_multi_file_batches_nested_paths_when_directory_mode_is_not_in_protocol() {
    let dir = TempDir::new().unwrap();
    let top = write_file(dir.path(), "src/a.ts", "export function alpha() {}\n");
    let nested = write_file(dir.path(), "src/nested/b.ts", "export function beta() {}\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-nested",
            "command": "outline",
            "files": [top, nested],
        }),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);
    let text = resp["text"].as_str().expect("outline text");

    assert!(
        text.contains("src/\n"),
        "should render src directory: {text}"
    );
    assert!(
        text.contains("  a.ts\n"),
        "should render top-level file under src: {text}"
    );
    assert!(
        text.contains("  nested/\n"),
        "should render nested directory: {text}"
    );
    assert!(
        text.contains("    b.ts\n"),
        "should render nested file: {text}"
    );
    assert!(
        text.contains("alpha") && text.contains("beta"),
        "symbols should be present: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_multi_file_truncates_when_output_exceeds_30kb() {
    let dir = TempDir::new().unwrap();
    let mut files = Vec::new();

    for file_idx in 0..24 {
        let mut content = String::new();
        for symbol_idx in 0..120 {
            content.push_str(&format!(
                "export function symbol_{file_idx:02}_{symbol_idx:03}(): number {{ return {symbol_idx}; }}\n"
            ));
        }
        files.push(write_file(
            dir.path(),
            &format!("src/file_{file_idx:02}.ts"),
            &content,
        ));
    }

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "outline-trunc", "command": "outline", "files": files}),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("... truncated (") && text.contains("30KB limit"),
        "outline should include truncation marker: {text}"
    );
    assert!(
        text.contains("Narrow scope with a more specific directory path"),
        "outline should include narrowing hint: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_symbol_lookup_returns_content_and_call_graph_annotations() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "calls.ts",
        r#"function helper(x: number): number {
  return x * 2;
}

function compute(a: number, b: number): number {
  const doubled = helper(a);
  return doubled + b;
}

function orchestrate(): number {
  const x = compute(1, 2);
  const y = helper(3);
  return x + y;
}

function unused(): void {
  console.log("nobody calls me");
}
"#,
    );

    let mut aft = AftProcess::spawn();
    let resp = send(
        &mut aft,
        json!({"id": "zoom-compute", "command": "zoom", "file": file, "symbol": "compute"}),
    );

    assert_eq!(resp["success"], true, "zoom should succeed: {:?}", resp);
    assert_eq!(resp["name"], "compute");
    assert_eq!(resp["kind"], "function");

    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("function compute"),
        "content should include symbol body: {content}"
    );
    assert!(
        content.contains("helper(a)"),
        "content should include outgoing call: {content}"
    );

    let calls_out = resp["annotations"]["calls_out"].as_array().unwrap();
    assert_eq!(
        calls_out.len(),
        1,
        "compute should have one known outgoing call: {calls_out:?}"
    );
    assert_eq!(calls_out[0]["name"], "helper");
    assert_eq!(calls_out[0]["line"], 6);

    let called_by = resp["annotations"]["called_by"].as_array().unwrap();
    assert_eq!(
        called_by.len(),
        1,
        "compute should have one known caller: {called_by:?}"
    );
    assert_eq!(called_by[0]["name"], "orchestrate");
    assert_eq!(called_by[0]["line"], 11);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_symbol_not_found_returns_error() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "missing.ts",
        "export function greet(name: string): string { return name; }\n",
    );

    let mut aft = AftProcess::spawn();
    let resp = send(
        &mut aft,
        json!({"id": "zoom-missing", "command": "zoom", "file": file, "symbol": "doesNotExist"}),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");
    assert!(
        resp["message"].as_str().unwrap().contains("doesNotExist"),
        "error should mention missing symbol: {:?}",
        resp
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_follows_reexport_chains_to_the_resolved_symbol_source() {
    let dir = TempDir::new().unwrap();
    let config = write_file(
        dir.path(),
        "config.ts",
        "export class Config {}\nexport default class DefaultConfig {}\n",
    );
    let _barrel1 = write_file(
        dir.path(),
        "barrel1.ts",
        "export { Config } from './config';\nexport { default as NamedDefault } from './config';\n",
    );
    let _barrel2 = write_file(
        dir.path(),
        "barrel2.ts",
        "export { Config as RenamedConfig } from './barrel1';\n",
    );
    let _barrel3 = write_file(
        dir.path(),
        "barrel3.ts",
        "export * from './barrel2';\nexport * from './barrel1';\n",
    );
    let index = write_file(
        dir.path(),
        "index.ts",
        "export class LocalConfig {}\nexport { RenamedConfig as FinalConfig } from './barrel3';\nexport * from './barrel3';\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-reexport", "command": "zoom", "file": index, "symbol": "FinalConfig"}),
    );

    assert_eq!(
        resp["success"], true,
        "zoom should resolve barrel re-exports: {:?}",
        resp
    );
    assert_eq!(resp["name"], "Config");
    assert_eq!(resp["kind"], "class");
    assert_eq!(resp["range"]["start_line"], 1);

    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("export class Config {}"),
        "zoom should read resolved source file: {content}"
    );
    assert!(
        !content.contains("FinalConfig") && !content.contains("LocalConfig"),
        "zoom content should come from resolved file, not barrel/index file: {content}"
    );
    assert_eq!(resp["annotations"]["calls_out"], json!([]));
    assert_eq!(resp["annotations"]["called_by"], json!([]));

    assert!(config.exists(), "fixture source file should exist");

    let status = aft.shutdown();
    assert!(status.success());
}
