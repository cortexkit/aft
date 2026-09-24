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

fn large_ts_class_source() -> String {
    let mut source = String::from(
        r#"class BigContainer {
  methodOne(): number {
    const visibleMethodBodyLine = 1;
"#,
    );
    for i in 0..155 {
        source.push_str(&format!("    const filler{i} = {i};\n"));
    }
    source.push_str(
        r#"    return visibleMethodBodyLine;
  }

  methodTwo(): void {
    console.log("second");
  }
}
"#,
    );
    source
}

fn large_ts_interface_source() -> String {
    let mut source = String::from("interface BigInterface {\n  primary(): number;\n");
    for i in 0..155 {
        source.push_str(&format!("  field{i}: string;\n"));
    }
    source.push_str("  callback: (value: number) => void;\n}\n");
    source
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
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
        text.contains("E function greet(name: string): string 1:3"),
        "single-file outline should include function signature: {text}"
    );
    assert!(
        text.contains("class Worker 5:9"),
        "single-file outline should include class signature: {text}"
    );
    assert!(
        text.contains(".run(task: string): void 6:8"),
        "single-file outline should include nested method signature: {text}"
    );
    assert!(
        text.contains("E const answer = 42; 11:11"),
        "top-level const should render as variable: {text}"
    );
    assert!(
        text.contains("let localCount = 0; 12:12"),
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
    if let Err(error) = create_dir_symlink(dir.path(), &dir.path().join("src/loop")) {
        eprintln!(
            "skipping symlink loop outline test: directory symlink unavailable in this environment: {error}"
        );
        return;
    }

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
fn outline_directory_hides_tests_and_renders_top_level_symbols_only() {
    let dir = TempDir::new().unwrap();
    write_file(
        dir.path(),
        "src/service.ts",
        r#"export class Worker {
  run(): void {}
}

export function makeWorker(): Worker {
  return new Worker();
}
"#,
    );
    let test_file = write_file(
        dir.path(),
        "src/service.test.ts",
        r#"export function testAlpha(): void {}
export function testBeta(): void {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let default_resp = send(
        &mut aft,
        json!({"id": "outline-dir-no-tests", "command": "outline", "directory": dir.path()}),
    );
    assert_eq!(
        default_resp["success"], true,
        "outline should succeed: {default_resp:?}"
    );
    let default_text = default_resp["text"].as_str().expect("outline text");
    assert!(
        default_text.contains("service.ts"),
        "non-test file missing: {default_text}"
    );
    assert!(
        default_text.contains("Worker"),
        "top-level class missing: {default_text}"
    );
    assert!(
        default_text.contains("makeWorker"),
        "top-level function missing: {default_text}"
    );
    assert!(
        !default_text.contains("service.test.ts"),
        "test file should be hidden by default: {default_text}"
    );
    assert!(
        !default_text.contains("run"),
        "directory outline should omit nested methods: {default_text}"
    );

    let with_tests = send(
        &mut aft,
        json!({
            "id": "outline-dir-with-tests",
            "command": "outline",
            "directory": dir.path(),
            "includeTests": true,
        }),
    );
    assert_eq!(
        with_tests["success"], true,
        "outline should succeed: {with_tests:?}"
    );
    let with_tests_text = with_tests["text"].as_str().expect("outline text");
    assert!(
        with_tests_text.contains("service.test.ts"),
        "includeTests should include test file: {with_tests_text}"
    );

    let explicit_test = send(
        &mut aft,
        json!({"id": "outline-explicit-test", "command": "outline", "file": test_file}),
    );
    assert_eq!(
        explicit_test["success"], true,
        "explicit test outline should succeed: {explicit_test:?}"
    );
    let explicit_text = explicit_test["text"].as_str().expect("outline text");
    assert!(
        explicit_text.contains("testAlpha") && explicit_text.contains("testBeta"),
        "explicit single test file should still be outlined: {explicit_text}"
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
        text.contains("cls") && text.contains("Worker") && !text.contains("run"),
        "multi-file outline should show top-level Python class without nested methods: {text}"
    );
    assert!(
        text.contains(" h ") && text.contains("Title") && !text.contains("Details"),
        "multi-file outline should show top-level Markdown headings only: {text}"
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
fn outline_and_zoom_resolve_typescript_callable_members() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "issue-329.ts",
        r#"export interface Shape {
  area(): number;
  scale(f: number): Shape;
}
export const helper = {
  compute: function doCompute(x: number) { return x * 2; },
  arrow: (y: number) => y + 1,
};
export function plain(n: number) { return n; }
"#,
    );

    let mut aft = AftProcess::spawn();
    let outline = send(
        &mut aft,
        json!({"id": "outline-issue-329", "command": "outline", "file": &file}),
    );
    assert_eq!(
        outline["success"], true,
        "outline should succeed: {outline:?}"
    );
    let text = outline["text"].as_str().expect("outline text");
    assert_eq!(
        text.lines().skip(1).count(),
        8,
        "fixture should produce exactly eight outline symbols: {text}"
    );
    for name in [
        "Shape",
        "area",
        "scale",
        "helper",
        "compute",
        "doCompute",
        "arrow",
        "plain",
    ] {
        assert!(text.contains(name), "outline should contain {name}: {text}");
    }

    let area = send(
        &mut aft,
        json!({"id": "zoom-area", "command": "zoom", "file": &file, "symbol": "area"}),
    );
    assert_eq!(area["success"], true, "area zoom should succeed: {area:?}");
    assert_eq!(area["kind"], "method");
    assert_eq!(area["content"], "  area(): number;");

    let named_expression = send(
        &mut aft,
        json!({"id": "zoom-do-compute", "command": "zoom", "file": &file, "symbol": "doCompute"}),
    );
    assert_eq!(
        named_expression["success"], true,
        "named expression zoom should succeed: {named_expression:?}"
    );
    assert!(named_expression["content"]
        .as_str()
        .unwrap()
        .contains("function doCompute"));

    assert!(aft.shutdown().success());
}

#[test]
fn outline_existing_large_interface_is_a_nested_member_menu() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("packages/aft-bridge/src/subc-transport.ts");
    let mut aft = AftProcess::spawn();
    let outline = send(
        &mut aft,
        json!({"id": "outline-existing-interface", "command": "outline", "file": file}),
    );

    assert_eq!(
        outline["success"], true,
        "existing interface outline should succeed: {outline:?}"
    );
    let text = outline["text"].as_str().expect("outline text");
    let interface_line = text
        .lines()
        .find(|line| line.contains("interface SubcTransportPoolOptions"))
        .expect("SubcTransportPoolOptions interface entry");
    assert!(
        interface_line.starts_with("  ") && !interface_line.starts_with("    "),
        "interface should remain a top-level menu entry: {interface_line}"
    );
    for member in ["connect", "onBgEventsNudge", "lifecycleDemandCheck"] {
        let member_line = text
            .lines()
            .find(|line| line.trim_start().starts_with(&format!(".{member}")))
            .unwrap_or_else(|| panic!("missing nested {member} member: {text}"));
        assert!(
            member_line.starts_with("    ."),
            "{member} should remain nested under its interface: {member_line}"
        );
    }
    assert!(
        !text.contains("Called when an idle bg-completion WAKE arrives"),
        "outline should render a member menu, not dump interface source: {text}"
    );

    assert!(aft.shutdown().success());
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
        json!({"id": "zoom-compute", "command": "zoom", "file": file, "symbol": "compute", "callgraph": true}),
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

    // No callgraph index is observed in this bare process, so the ordinary zoom
    // comes back with an explicitly unavailable callgraph field instead of
    // call lists (the lists themselves are covered by the zoom unit tests over
    // a ready callgraph).
    assert_eq!(resp["annotations"]["status"], "unavailable");
    assert_eq!(resp["annotations"]["code"], "callgraph_unavailable");
    assert_eq!(resp["annotations"]["index"]["status"], "unavailable");
    assert!(resp["annotations"]["calls_out"].is_null());
    assert!(resp["annotations"]["called_by"].is_null());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_large_container_returns_member_signature_menu() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "large.ts", &large_ts_class_source());

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-large-container", "command": "zoom", "file": file, "symbol": "BigContainer"}),
    );

    assert_eq!(resp["success"], true, "large container zoom: {resp:?}");
    assert_eq!(resp["kind"], "class");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("member-signature menu; zoom a member for its body"),
        "large container should explain menu output: {content}"
    );
    assert!(
        content.contains("BigContainer.methodOne(): number"),
        "menu should include qualified method signature: {content}"
    );
    assert!(
        content.contains("BigContainer.methodTwo(): void"),
        "menu should include second method signature: {content}"
    );
    assert!(
        !content.contains("visibleMethodBodyLine"),
        "menu must not include method bodies: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_large_interface_returns_member_signature_menu() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "large-interface.ts",
        &large_ts_interface_source(),
    );

    let mut aft = AftProcess::spawn();
    let resp = send(
        &mut aft,
        json!({"id": "zoom-large-interface", "command": "zoom", "file": file, "symbol": "BigInterface"}),
    );

    assert_eq!(resp["success"], true, "large interface zoom: {resp:?}");
    assert_eq!(resp["kind"], "interface");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("member-signature menu; zoom a member for its body"),
        "large interface should remain a member menu: {content}"
    );
    assert!(content.contains("BigInterface.primary(): number"));
    assert!(content.contains("BigInterface.callback: (value: number) => void"));
    assert!(
        !content.contains("field154"),
        "non-callable fields must not be dumped into the member menu: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_small_container_returns_whole_body() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "small.ts",
        r#"class SmallContainer {
  run(): string {
    const smallBodyLine = "small";
    return smallBodyLine;
  }
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-small-container", "command": "zoom", "file": file, "symbol": "SmallContainer"}),
    );

    assert_eq!(resp["success"], true, "small container zoom: {resp:?}");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("smallBodyLine"),
        "small container should still return full body: {content}"
    );
    assert!(
        !content.contains("member-signature menu"),
        "small container should not be converted to a menu: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_leaf_qualified_method_returns_full_body_even_when_large() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "large.ts", &large_ts_class_source());

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-qualified-leaf", "command": "zoom", "file": file, "symbol": "BigContainer.methodOne"}),
    );

    assert_eq!(resp["success"], true, "qualified leaf zoom: {resp:?}");
    assert_eq!(resp["name"], "methodOne");
    assert_eq!(resp["kind"], "method");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("visibleMethodBodyLine"),
        "leaf zoom should include the full method body: {content}"
    );
    assert!(
        content.contains("return visibleMethodBodyLine"),
        "leaf zoom should preserve the method return body: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_ambiguous_bare_name_returns_candidate_signatures() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "ambiguous.ts",
        r#"class First {
  execute(): string {
    const firstBodyLine = "first";
    return firstBodyLine;
  }
}

class Second {
  execute(): string {
    const secondBodyLine = "second";
    return secondBodyLine;
  }
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-ambiguous", "command": "zoom", "file": file, "symbol": "execute"}),
    );

    assert_eq!(
        resp["success"], true,
        "ambiguous zoom should not be an error: {resp:?}"
    );
    assert_eq!(resp["kind"], "ambiguous_symbol");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("First.execute(): string"),
        "candidate menu should include first qualified signature: {content}"
    );
    assert!(
        content.contains("Second.execute(): string"),
        "candidate menu should include second qualified signature: {content}"
    );
    assert!(
        !content.contains("firstBodyLine") && !content.contains("secondBodyLine"),
        "ambiguous disambiguation should not include bodies: {content}"
    );
    assert_eq!(resp["candidates"].as_array().unwrap().len(), 2);

    let qualified = send(
        &mut aft,
        json!({"id": "zoom-qualified-candidate", "command": "zoom", "file": file, "symbol": "First.execute"}),
    );
    assert_eq!(
        qualified["success"], true,
        "qualified candidate zoom: {qualified:?}"
    );
    assert!(qualified["content"]
        .as_str()
        .unwrap()
        .contains("firstBodyLine"));

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_rust_type_counts_impl_method_spans_for_menu() {
    let dir = TempDir::new().unwrap();
    let mut source = String::from(
        r#"pub struct Widget {
    value: i32,
}

impl Widget {
    pub fn heavy(&self) -> i32 {
        let mut total = self.value;
"#,
    );
    for i in 0..155 {
        source.push_str(&format!("        total += {i};\n"));
    }
    source.push_str(
        r#"        total
    }
}
"#,
    );
    let file = write_file(dir.path(), "widget.rs", &source);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "zoom-rust-type", "command": "zoom", "file": file, "symbol": "Widget"}),
    );

    assert_eq!(resp["success"], true, "rust type zoom: {resp:?}");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("member-signature menu; zoom a member for its body"),
        "large Rust type should return a menu: {content}"
    );
    assert!(
        content.contains("Widget.heavy — pub fn heavy(&self) -> i32"),
        "Rust impl method should be qualified under the type: {content}"
    );
    assert!(
        !content.contains("total += 154"),
        "Rust menu must not include impl method body lines: {content}"
    );

    assert!(aft.shutdown().success());
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

#[test]
fn zoom_whitespace_separated_symbol_string_resolves_multiple_code_symbols() {
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
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-multi-ws",
            "command": "zoom",
            "file": file,
            "symbols": "helper compute orchestrate",
        }),
    );

    assert_eq!(resp["success"], true, "batch zoom should succeed: {resp:?}");
    assert_eq!(resp["complete"], true);
    let entries = resp["symbols"].as_array().expect("symbols batch");
    assert_eq!(entries.len(), 3);
    for (name, expected) in [
        ("helper", "function helper"),
        ("compute", "function compute"),
        ("orchestrate", "function orchestrate"),
    ] {
        let entry = entries
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("missing entry for {name}: {entries:?}"));
        assert_eq!(entry["response"]["success"], true, "{name}: {entry:?}");
        assert!(
            entry["response"]["content"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{name} body: {entry:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_whitespace_separated_symbol_reports_per_symbol_not_found() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "calls.ts",
        "function helper(x: number): number { return x * 2; }\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-multi-partial",
            "command": "zoom",
            "file": file,
            "symbol": "helper missingSymbol",
        }),
    );

    assert_eq!(resp["success"], true);
    assert_eq!(resp["complete"], false);
    let entries = resp["symbols"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let helper = entries.iter().find(|e| e["name"] == "helper").unwrap();
    let missing = entries
        .iter()
        .find(|e| e["name"] == "missingSymbol")
        .unwrap();
    assert_eq!(helper["response"]["success"], true);
    assert_eq!(missing["response"]["success"], false);
    assert_eq!(missing["response"]["code"], "symbol_not_found");

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_markdown_heading_with_spaces_not_split() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "readme.md",
        "# Project Title\n\n## Getting Started\n\nIntro here.\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-md-heading",
            "command": "zoom",
            "file": file,
            "symbols": "Getting Started",
        }),
    );

    assert_eq!(resp["success"], true, "single heading zoom: {resp:?}");
    assert_eq!(resp["name"], "Getting Started");
    assert!(
        resp["content"].as_str().unwrap().contains("Intro here"),
        "{resp:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_html_heading_with_spaces_not_split() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "page.html",
        "<html><body><h2>Getting Started</h2><p>Body text</p></body></html>\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-html-heading",
            "command": "zoom",
            "file": file,
            "symbol": "Getting Started",
        }),
    );

    assert_eq!(resp["success"], true, "html heading zoom: {resp:?}");
    assert_eq!(resp["name"], "Getting Started");
    assert!(
        resp["content"].as_str().unwrap().contains("Body text"),
        "{resp:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_symbol_not_found_with_close_matches() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "search.ts",
        r#"function handle_grep_search() {}
function handle_semantic_search() {}
function handle_semantic_or_hybrid_search() {}
function compute_total() {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-close-matches",
            "command": "zoom",
            "file": file,
            "symbol": "handle_search",
        }),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");
    let msg = resp["message"].as_str().unwrap();
    assert!(msg.contains("symbol 'handle_search' not found"));
    // The steering contract: name the retry futility, then offer the close
    // matches as a pick-one menu.
    assert!(
        msg.contains("Retrying this exact zoom call will fail again"),
        "refusal must carry the retry-unchanged signal: {msg}"
    );
    assert!(msg.contains("Choose one of these names from the file outline"));
    assert!(msg.contains("handle_grep_search"));
    assert!(msg.contains("handle_semantic_search"));
    assert!(msg.contains("handle_semantic_or_hybrid_search"));
    assert!(!msg.contains("compute_total"));

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_symbol_not_found_without_close_matches() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "search.ts",
        r#"function compute_total() {}
function unrelated_symbol() {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-no-close-matches",
            "command": "zoom",
            "file": file,
            "symbol": "handle_search",
        }),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");
    let msg = resp["message"].as_str().unwrap();
    // No fuzzy-close candidates exist, so steering degrades to the single
    // closest outline symbol - still telling the caller an unchanged retry
    // cannot succeed.
    assert!(msg.contains("symbol 'handle_search' not found"));
    assert!(
        msg.contains("Retrying this exact zoom call will fail again"),
        "refusal must carry the retry-unchanged signal: {msg}"
    );
    assert!(
        msg.contains("closest is `compute_total`"),
        "no-close-match refusal must still name the closest symbol: {msg}"
    );

    assert!(aft.shutdown().success());
}

const NESTED_JSON: &str = r#"{
  "registration_profile_manifest": {
    "host_only_allowlist": ["alpha", "beta"],
    "nested": {
      "deep": "value"
    }
  },
  "servers": [
    { "name": "primary", "port": 8080 },
    { "name": "backup", "port": 9090 }
  ],
  "a": {
    "b": [
      { "c": "first" },
      { "c": "second" }
    ]
  },
  "literal.dotted.key": "literal-value",
  "literal": {
    "dotted": {
      "key": "path-value"
    }
  },
  "plain.dotted": "plain-value"
}
"#;

#[test]
fn zoom_json_nested_object_path() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-nested",
            "command": "zoom",
            "file": file,
            "symbol": "registration_profile_manifest.nested.deep",
        }),
    );

    assert_eq!(resp["success"], true, "nested path zoom: {resp:?}");
    assert_eq!(resp["name"], "registration_profile_manifest.nested.deep");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("\"value\""),
        "nested path should render the resolved value: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_array_index() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-array",
            "command": "zoom",
            "file": file,
            "symbol": "servers[0]",
        }),
    );

    assert_eq!(resp["success"], true, "array index zoom: {resp:?}");
    assert_eq!(resp["name"], "servers[0]");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("primary"),
        "array index should render the element: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_chained_array_index() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-chained",
            "command": "zoom",
            "file": file,
            "symbol": "a.b[1].c",
        }),
    );

    assert_eq!(resp["success"], true, "chained array zoom: {resp:?}");
    assert_eq!(resp["name"], "a.b[1].c");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("\"second\""),
        "chained array should render the element value: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_literal_dotted_key_wins_over_path() {
    // The fixture has a literal key "plain.dotted" with no corresponding
    // nested path. Literal-first means the literal key wins outright.
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-literal-dotted",
            "command": "zoom",
            "file": file,
            "symbol": "plain.dotted",
        }),
    );

    assert_eq!(resp["success"], true, "literal dotted key zoom: {resp:?}");
    let content = resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("plain-value"),
        "literal dotted key should win over the path interpretation: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_equal_node_literal_and_path_is_not_ambiguous() {
    // A query that resolves to the same node both as a literal key and as a
    // path must not be reported as ambiguous.
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-equal-node",
            "command": "zoom",
            "file": file,
            "symbol": "servers",
        }),
    );

    assert_eq!(resp["success"], true, "equal-node zoom: {resp:?}");
    assert_eq!(resp["kind"], "variable");
    assert!(resp["content"].as_str().unwrap().contains("primary"));

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_different_node_literal_and_path_is_ambiguous() {
    // A query that resolves to DIFFERENT nodes as a literal key and as a path
    // must return ambiguous_match with both candidates.
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-ambiguous",
            "command": "zoom",
            "file": file,
            "symbol": "literal.dotted.key",
        }),
    );

    // The literal key "literal.dotted.key" and the path literal.dotted.key
    // resolve to different nodes → ambiguous_match.
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "ambiguous_match");
    let candidates = resp["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2, "both candidates: {candidates:?}");

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_miss_reports_deepest_prefix_and_failing_segment() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-miss",
            "command": "zoom",
            "file": file,
            "symbol": "registration_profile_manifest.host_only_allowlis",
        }),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");
    let msg = resp["message"].as_str().unwrap();
    assert!(
        msg.contains("registration_profile_manifest"),
        "miss should report deepest resolved prefix: {msg}"
    );
    assert!(
        msg.contains("host_only_allowlis"),
        "miss should report the failing segment: {msg}"
    );
    assert!(
        msg.contains("host_only_allowlist"),
        "miss should suggest the nearest key: {msg}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_multi_symbol_paths_work() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-multi",
            "command": "zoom",
            "file": file,
            "symbols": ["servers[0]", "a.b[0].c"],
        }),
    );

    assert_eq!(resp["success"], true, "multi-symbol zoom: {resp:?}");
    assert_eq!(resp["complete"], true);
    let entries = resp["symbols"].as_array().expect("symbols batch");
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert_eq!(
            entry["response"]["success"], true,
            "path entry should succeed: {entry:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_json_targets_form_works_with_paths() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "nested.json", NESTED_JSON);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-json-targets",
            "command": "zoom",
            "targets": [
                { "file": file, "symbol": "servers[1].name" },
                { "file": file, "symbol": "registration_profile_manifest.host_only_allowlist" }
            ],
        }),
    );

    assert_eq!(resp["success"], true, "targets zoom: {resp:?}");
    let targets = resp["targets"].as_array().expect("targets");
    assert_eq!(targets.len(), 2);
    for target in targets {
        assert_eq!(
            target["response"]["success"], true,
            "target should succeed: {target:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn zoom_non_json_dotted_query_keeps_existing_behavior() {
    // Negative control: a dotted query on a .ts file must still resolve via the
    // existing symbol logic (qualified name), and its miss message is unchanged.
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "search.ts",
        r#"class First {
  execute(): string {
    const firstBodyLine = "first";
    return firstBodyLine;
  }
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-ts-dotted",
            "command": "zoom",
            "file": file,
            "symbol": "First.execute",
        }),
    );

    assert_eq!(resp["success"], true, "ts dotted zoom: {resp:?}");
    assert_eq!(resp["name"], "execute");
    assert!(resp["content"].as_str().unwrap().contains("firstBodyLine"));

    // Miss message unchanged for non-JSON files.
    let miss = send(
        &mut aft,
        json!({
            "id": "zoom-ts-dotted-miss",
            "command": "zoom",
            "file": file,
            "symbol": "First.missing",
        }),
    );
    assert_eq!(miss["success"], false);
    assert_eq!(miss["code"], "symbol_not_found");
    let msg = miss["message"].as_str().unwrap();
    assert!(
        msg.contains("symbol 'First.missing' not found"),
        "non-JSON miss message should be unchanged: {msg}"
    );

    assert!(aft.shutdown().success());
}
