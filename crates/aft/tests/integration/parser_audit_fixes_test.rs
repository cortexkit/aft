use std::path::{Path, PathBuf};

use crate::helpers::AftProcess;
use aft::language::LanguageProvider;
use aft::parser::{detect_language, FileParser, LangId, TreeSitterProvider};
use aft::symbols::{Symbol, SymbolKind};

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&path, content).expect("write file");
    path
}

fn extract(file: &Path) -> Vec<Symbol> {
    FileParser::new()
        .extract_symbols(file)
        .expect("extract symbols")
}

fn symbol<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
    symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("missing symbol {name}; got {symbols:?}"))
}

#[test]
fn ts_js_anonymous_default_exports_surface_and_resolve_as_default() {
    let tmp = crate::test_helpers::tempdir();
    let ts = write_file(
        tmp.path(),
        "anonymous.ts",
        "export default function () { return 1; }\n",
    );
    let js = write_file(
        tmp.path(),
        "anonymous.js",
        "export default class { run() {} }\n",
    );

    for (file, expected_kind) in [(&ts, SymbolKind::Function), (&js, SymbolKind::Class)] {
        let symbols = extract(file);
        let default_symbol = symbol(&symbols, "default");
        assert_eq!(default_symbol.kind, expected_kind);
        assert!(default_symbol.exported);

        let resolved = TreeSitterProvider::new()
            .resolve_symbol(file, "default")
            .expect("resolve default export");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].symbol.name, "default");
        assert_eq!(resolved[0].symbol.kind, expected_kind);
    }
}

/// Top-level const/let in a `.js` file must surface as a Variable symbol, the
/// same as in `.ts`. The JS extractor previously dropped these (they fell into
/// the catch-all match arm), so outline/dead_code/callgraph never saw
/// `export const VERSION = ...` in plain JavaScript.
#[test]
fn js_top_level_const_let_surface_as_variables() {
    let tmp = crate::test_helpers::tempdir();
    let src = "export const VERSION = \"1.0\";\nconst internal = 42;\nlet counter = 0;\n";
    let js = write_file(tmp.path(), "vars.js", src);
    let ts = write_file(tmp.path(), "vars.ts", src);

    for file in [&js, &ts] {
        let symbols = extract(file);
        let version = symbol(&symbols, "VERSION");
        assert_eq!(version.kind, SymbolKind::Variable, "in {file:?}");
        assert!(version.exported, "VERSION should be exported in {file:?}");
        assert_eq!(symbol(&symbols, "internal").kind, SymbolKind::Variable);
        assert_eq!(symbol(&symbols, "counter").kind, SymbolKind::Variable);
        assert!(!symbol(&symbols, "internal").exported);
    }
}

#[test]
fn common_module_and_stub_extensions_are_detected() {
    for (file, expected) in [
        ("module.mjs", LangId::JavaScript),
        ("module.cjs", LangId::JavaScript),
        ("module.mts", LangId::TypeScript),
        ("module.cts", LangId::TypeScript),
        ("module.pyi", LangId::Python),
    ] {
        assert_eq!(detect_language(Path::new(file)), Some(expected), "{file}");
    }
}

#[test]
fn ts_js_top_level_function_expression_initializers_are_functions() {
    let tmp = crate::test_helpers::tempdir();
    let ts = write_file(
        tmp.path(),
        "functions.ts",
        "const fnValue = function () { return 1; };\nconst genValue = function* () { yield 1; };\n",
    );
    let js = write_file(
        tmp.path(),
        "functions.js",
        "const fnValue = function () { return 1; };\nconst genValue = function* () { yield 1; };\n",
    );

    for file in [&ts, &js] {
        let symbols = extract(file);
        assert_eq!(symbol(&symbols, "fnValue").kind, SymbolKind::Function);
        assert_eq!(symbol(&symbols, "genValue").kind, SymbolKind::Function);
    }
}

#[test]
fn solidity_receive_and_fallback_are_synthetic_methods() {
    let tmp = crate::test_helpers::tempdir();
    let file = write_file(
        tmp.path(),
        "Vault.sol",
        r#"contract Vault {
    receive() external payable {}
    fallback() external {}
}
"#,
    );

    let symbols = extract(&file);
    let receive = symbol(&symbols, "receive");
    assert_eq!(receive.kind, SymbolKind::Method);
    assert_eq!(receive.parent.as_deref(), Some("Vault"));

    let fallback = symbol(&symbols, "fallback");
    assert_eq!(fallback.kind, SymbolKind::Method);
    assert_eq!(fallback.parent.as_deref(), Some("Vault"));
}

#[test]
fn lua_top_level_local_variables_are_captured() {
    let tmp = crate::test_helpers::tempdir();
    let file = write_file(
        tmp.path(),
        "module.lua",
        "local M = {}\nlocal count = 0\nfunction M.run() end\nreturn M\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "M").kind, SymbolKind::Variable);
    assert_eq!(symbol(&symbols, "count").kind, SymbolKind::Variable);
    assert_eq!(symbol(&symbols, "run").kind, SymbolKind::Method);
}

#[test]
fn swift_modifier_prefixed_structs_and_enums_keep_their_kind() {
    let tmp = crate::test_helpers::tempdir();
    let file = write_file(
        tmp.path(),
        "Model.swift",
        "public struct Box { let value: Int }\nindirect enum Tree { case leaf }\npublic class Worker {}\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "Box").kind, SymbolKind::Struct);
    assert_eq!(symbol(&symbols, "Tree").kind, SymbolKind::Enum);
    assert_eq!(symbol(&symbols, "Worker").kind, SymbolKind::Class);
}

#[test]
fn scala3_enums_are_reported_as_enums() {
    let tmp = crate::test_helpers::tempdir();
    let file = write_file(
        tmp.path(),
        "Color.scala",
        "enum Color {\n  case Red, Green\n}\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "Color").kind, SymbolKind::Enum);
}

fn range_lines(source: &str, symbol: &Symbol) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = symbol.range.start_line as usize;
    let end = (symbol.range.end_line as usize).min(lines.len().saturating_sub(1));
    lines[start..=end].join("\n")
}

fn range_excerpt(source: &str, symbol: &Symbol) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start_line = symbol.range.start_line as usize;
    let end_line = symbol.range.end_line as usize;
    let start_col = symbol.range.start_col as usize;
    let end_col = symbol.range.end_col as usize;

    if start_line == end_line {
        return lines[start_line][start_col..end_col].to_string();
    }

    let mut parts = Vec::new();
    parts.push(lines[start_line][start_col..].to_string());
    for line in lines.iter().take(end_line).skip(start_line + 1) {
        parts.push((*line).to_string());
    }
    parts.push(lines[end_line][..end_col].to_string());
    parts.join("\n")
}

fn ranges_overlap(left: &Symbol, right: &Symbol) -> bool {
    let left_start = (left.range.start_line, left.range.start_col);
    let left_end = (left.range.end_line, left.range.end_col);
    let right_start = (right.range.start_line, right.range.start_col);
    let right_end = (right.range.end_line, right.range.end_col);

    left_start < right_end && right_start < left_end
}

fn zoom_content(root: &Path, file: &Path, symbol: &str) -> String {
    let mut aft = AftProcess::spawn();
    let configured = aft.configure(root);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let request = serde_json::json!({
        "id": format!("zoom-{symbol}"),
        "command": "zoom",
        "file": file.to_string_lossy(),
        "symbol": symbol,
        "context_lines": 0,
    });
    let resp = aft.send(&request.to_string());
    assert_eq!(resp["success"], true, "zoom failed: {resp:?}");
    let content = resp["content"]
        .as_str()
        .expect("zoom content should be string")
        .to_string();
    assert!(aft.shutdown().success());
    content
}

#[test]
fn rust_inline_mod_functions_are_scoped_free_functions_not_dropped() {
    let tmp = crate::test_helpers::tempdir();
    let source = r#"pub fn top_level() {}

mod foo {
    pub fn bar() {}
}

struct Worker;
impl Worker {
    pub fn run(&self) {}
}
"#;
    let file = write_file(tmp.path(), "lib.rs", source);

    let symbols = extract(&file);
    let top_level = symbol(&symbols, "top_level");
    assert_eq!(top_level.kind, SymbolKind::Function);
    assert!(top_level.scope_chain.is_empty());
    assert_eq!(top_level.parent, None);

    let bar = symbol(&symbols, "bar");
    assert_eq!(bar.kind, SymbolKind::Function);
    assert_eq!(bar.scope_chain, vec!["foo".to_string()]);
    assert_eq!(bar.parent.as_deref(), Some("foo"));
    assert!(bar.exported, "pub mod function should be exported");

    let run = symbol(&symbols, "run");
    assert_eq!(run.kind, SymbolKind::Method);
    assert_eq!(run.scope_chain, vec!["Worker".to_string()]);
    assert_eq!(run.parent.as_deref(), Some("Worker"));
}

#[test]
fn ts_js_multi_declarator_variable_ranges_do_not_overlap_siblings() {
    let tmp = crate::test_helpers::tempdir();
    let source = "export const a = 1, b = 2;\n";
    let ts = write_file(tmp.path(), "vars.ts", source);
    let js = write_file(tmp.path(), "vars.js", source);

    for file in [&ts, &js] {
        let symbols = extract(file);
        let a = symbol(&symbols, "a");
        let b = symbol(&symbols, "b");
        assert_eq!(a.kind, SymbolKind::Variable, "in {file:?}");
        assert_eq!(b.kind, SymbolKind::Variable, "in {file:?}");
        assert!(a.exported, "a should inherit export in {file:?}");
        assert!(b.exported, "b should inherit export in {file:?}");
        assert!(
            !ranges_overlap(a, b),
            "multi-declarator ranges should not overlap in {file:?}: a={:?}, b={:?}",
            a.range,
            b.range
        );

        let a_text = range_excerpt(source, a);
        let b_text = range_excerpt(source, b);
        assert_eq!(a_text, "a = 1", "a range should cover only its declarator");
        assert_eq!(b_text, "b = 2", "b range should cover only its declarator");
        assert!(
            !a_text.contains("b = 2") && !b_text.contains("a = 1"),
            "replacing one declarator should not include its sibling: a={a_text:?}, b={b_text:?}"
        );
    }
}

#[test]
fn ts_js_jsdoc_attaches_only_without_blank_line() {
    let tmp = crate::test_helpers::tempdir();
    let source = r#"/** attached doc */
export function attached() {}

/** file-level note */

export function detached() {}
"#;
    let ts = write_file(tmp.path(), "doc.ts", source);
    let js = write_file(tmp.path(), "doc.js", source);

    for file in [&ts, &js] {
        let symbols = extract(file);
        let attached = symbol(&symbols, "attached");
        let attached_text = range_lines(source, attached);
        assert!(
            attached_text.starts_with("/** attached doc */"),
            "adjacent JSDoc should be part of attached range in {file:?}: {attached_text:?}"
        );
        assert!(attached_text.contains("export function attached"));

        let detached = symbol(&symbols, "detached");
        let detached_text = range_lines(source, detached);
        assert!(
            detached_text.starts_with("export function detached"),
            "blank-line-separated JSDoc must not be folded into detached range in {file:?}: {detached_text:?}"
        );
        assert!(
            !detached_text.contains("file-level note"),
            "detached range should not include unrelated file-level doc in {file:?}: {detached_text:?}"
        );
    }
}

#[test]
fn markdown_heading_zoom_stops_before_next_same_level_heading() {
    let tmp = crate::test_helpers::tempdir();
    let source = r#"# First

first body

## Child

child body

# Second

second body
"#;
    let file = write_file(tmp.path(), "doc.md", source);

    let symbols = extract(&file);
    let first = symbol(&symbols, "First");
    let first_range_text = range_lines(source, first);
    assert!(first_range_text.contains("## Child"));
    assert!(first_range_text.contains("child body"));
    assert!(
        !first_range_text.contains("# Second"),
        "First range should stop before next sibling heading: {first_range_text:?}"
    );

    let content = zoom_content(tmp.path(), &file, "First");
    assert!(content.contains("## Child"));
    assert!(content.contains("child body"));
    assert!(
        !content.contains("# Second") && !content.contains("second body"),
        "zoom should not include the next sibling section: {content:?}"
    );
}

#[test]
fn html_heading_zoom_stops_before_next_same_or_higher_heading() {
    let tmp = crate::test_helpers::tempdir();
    let source = r#"<h1>First</h1>
<p>first body</p>
<h2>Child</h2>
<p>child body</p>
<h1>Second</h1>
<p>second body</p>
"#;
    let file = write_file(tmp.path(), "doc.html", source);

    let symbols = extract(&file);
    let first = symbol(&symbols, "First");
    let first_range_text = range_lines(source, first);
    assert!(first_range_text.contains("<h2>Child</h2>"));
    assert!(first_range_text.contains("child body"));
    assert!(
        !first_range_text.contains("<h1>Second</h1>"),
        "First range should stop before next h1: {first_range_text:?}"
    );
    assert_eq!(
        first.range.end_col,
        "<p>child body</p>".len() as u32,
        "HTML heading range should end after the final line of its section so edit_symbol does not under-reach"
    );

    let content = zoom_content(tmp.path(), &file, "First");
    assert!(content.contains("<h2>Child</h2>"));
    assert!(content.contains("child body"));
    assert!(
        !content.contains("<h1>Second</h1>") && !content.contains("second body"),
        "zoom should not include the next sibling section: {content:?}"
    );
}
