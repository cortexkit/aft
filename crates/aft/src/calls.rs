//! Shared call-site extraction helpers.
//!
//! Extracted from `commands/zoom.rs` so both the zoom command and the
//! call-graph engine can reuse the same AST-walking logic.

use std::collections::BTreeSet;

use crate::parser::LangId;

// Defensive recursion bound for AST walkers. Hand-written code nests only tens
// of levels deep, but minified bundles, generated code, and very long
// expression/type chains can produce trees thousands of nodes deep. The inspect
// rayon pool uses bounded worker stacks, so unbounded AST recursion here can
// overflow the stack and SIGABRT the entire bridge. Past this depth we stop
// descending and treat the node as an opaque leaf. Kept well under the pool
// stack budget (see dispatch.rs stack_size).
const MAX_AST_WALK_DEPTH: u32 = 1_500;

/// Returns the tree-sitter node kind strings that represent call expressions
/// for the given language.
pub fn call_node_kinds(lang: LangId) -> Vec<&'static str> {
    match lang {
        LangId::TypeScript | LangId::JavaScript => vec!["call_expression", "new_expression"],
        LangId::Tsx => vec![
            "call_expression",
            "new_expression",
            "jsx_opening_element",
            "jsx_self_closing_element",
        ],
        LangId::Go => vec!["call_expression"],
        LangId::Python => vec!["call"],
        LangId::Rust => vec!["call_expression", "macro_invocation"],
        LangId::Solidity | LangId::Scala => vec!["call_expression"],
        LangId::Java => vec!["method_invocation"],
        LangId::Ruby => vec!["call"],
        LangId::Kotlin | LangId::Swift => vec!["call_expression"],
        LangId::Php => vec![
            "function_call_expression",
            "member_call_expression",
            "nullsafe_member_call_expression",
            "scoped_call_expression",
        ],
        LangId::Perl => vec!["function_call_expression", "method_call_expression"],
        LangId::Lua => vec!["function_call"],
        LangId::C | LangId::Cpp | LangId::Zig => vec!["call_expression"],
        LangId::CSharp => vec!["invocation_expression"],
        LangId::Bash
        | LangId::Scss
        | LangId::Vue
        | LangId::Html
        | LangId::Markdown
        | LangId::Json
        | LangId::Yaml
        | LangId::Pascal
        | LangId::R => vec![],
    }
}

/// Recursively walk tree nodes looking for call expressions within a byte range.
///
/// Collects `(callee_name, line_number)` pairs into `results`.
pub fn walk_for_calls(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    call_kinds: &[&str],
    results: &mut Vec<(String, u32)>,
) {
    walk_for_calls_at_depth(node, source, byte_start, byte_end, call_kinds, results, 0);
}

fn walk_for_calls_at_depth(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    call_kinds: &[&str],
    results: &mut Vec<(String, u32)>,
    depth: u32,
) {
    let node_start = node.start_byte();
    let node_end = node.end_byte();

    // Skip nodes entirely outside our range
    if node_end <= byte_start || node_start >= byte_end {
        return;
    }

    if call_kinds.contains(&node.kind()) && node_start >= byte_start && node_end <= byte_end {
        if let Some(name) = extract_callee_name(&node, source) {
            results.push((name, node.start_position().row as u32 + 1));
        }
    }

    // Stop descending past MAX_AST_WALK_DEPTH to avoid overflowing the bounded
    // inspect worker stack on pathologically deep trees (minified bundles,
    // generated code, long chains). Treat the truncated subtree as an opaque
    // leaf; calls already discovered at this node are still reported.
    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }

    // Recurse into children
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_for_calls_at_depth(
                cursor.node(),
                source,
                byte_start,
                byte_end,
                call_kinds,
                results,
                depth + 1,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Extract the callee name from a call expression node.
///
/// For simple calls like `foo()`, returns "foo".
/// For member access like `this.add()` or `obj.method()`, returns the last
/// segment ("add" / "method").
/// For Rust macros like `println!()`, returns "println!".
pub fn extract_callee_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let kind = node.kind();

    if kind == "macro_invocation" {
        // Rust macro: first child is the macro name (e.g. `println!`)
        let first_child = node.child(0)?;
        let text = &source[first_child.byte_range()];
        return Some(format!("{}!", text));
    }

    let func_node = callee_node(node)?;

    let func_kind = func_node.kind();
    match func_kind {
        // Simple identifier: foo()
        "identifier" => Some(source[func_node.byte_range()].to_string()),
        // Member access: obj.method() / this.method()
        "member_expression"
        | "field_expression"
        | "attribute"
        | "member_access_expression"
        | "qualified_identifier"
        | "generic_name"
        | "template_function"
        | "template_method" => {
            // Last child that's a property_identifier, field_identifier, or identifier
            extract_last_segment(&func_node, source)
        }
        // Computed member access: obj["method"]()
        "subscript_expression" => extract_computed_member_name(&func_node, source)
            .or_else(|| extract_last_segment(&func_node, source)),
        _ => {
            // Fallback: use the full text
            let text = &source[func_node.byte_range()];
            // If it contains a dot, take the last segment
            if text.contains('.') {
                text.rsplit('.').next().map(|s| s.trim().to_string())
            } else {
                Some(text.trim().to_string())
            }
        }
    }
}

/// Extract the full callee expression from a call expression node.
///
/// Unlike `extract_callee_name` which returns only the last segment,
/// this returns the full expression (e.g. "utils.foo" for `utils.foo()`).
/// Used by the call graph engine to detect namespace-qualified calls.
pub fn extract_full_callee(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let kind = node.kind();

    if kind == "macro_invocation" {
        let first_child = node.child(0)?;
        let text = &source[first_child.byte_range()];
        return Some(format!("{}!", text));
    }

    let func_node = callee_node(node)?;

    Some(source[func_node.byte_range()].trim().to_string())
}

fn callee_node<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    match node.kind() {
        "new_expression" => node
            .child_by_field_name("constructor")
            .or_else(|| node.named_child(0)),
        "jsx_opening_element" | "jsx_self_closing_element" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0)),
        // Perl method call `$obj->method(...)`: the callee is the `method`
        // field, not the leading invocant that `child(0)` would return.
        "method_call_expression" => node
            .child_by_field_name("method")
            .or_else(|| node.child_by_field_name("function")),
        _ => node
            .child_by_field_name("function")
            .or_else(|| node.child(0)),
    }
}

fn extract_computed_member_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let index = node.child_by_field_name("index")?;
    let text = source[index.byte_range()].trim();
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        return Some(text[1..text.len().saturating_sub(1)].to_string());
    }
    None
}

/// Extract the last segment of a member expression (the method/property name).
pub fn extract_last_segment(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        if let Some(segment) = extract_last_segment(&name, source) {
            return Some(segment);
        }
    }

    let child_count = node.child_count();
    // Walk children from the end looking for an identifier-like node
    for i in (0..child_count).rev() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "property_identifier" | "field_identifier" | "identifier" => {
                    return Some(source[child.byte_range()].to_string());
                }
                "generic_name" | "template_function" | "template_method" => {
                    if let Some(segment) = extract_last_segment(&child, source) {
                        return Some(segment);
                    }
                }
                _ => {}
            }
        }
    }
    // Fallback: full text, last dot segment
    let text = &source[node.byte_range()];
    text.rsplit('.').next().map(|s| s.trim().to_string())
}

/// Extract type-reference names within a byte range of the AST.
///
/// This is intentionally separate from call extraction. The live call graph and
/// `aft_callgraph` commands remain call-edge-only; dead-code analysis consumes
/// these type-position names as a side channel.
pub fn extract_type_references_in_range(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> BTreeSet<String> {
    let mut results = BTreeSet::new();
    collect_type_references(root, source, byte_start, byte_end, lang, &mut results);
    results
}

/// Extract all type-reference names in a parsed file.
pub fn extract_type_references(
    source: &str,
    root: tree_sitter::Node,
    lang: LangId,
) -> BTreeSet<String> {
    extract_type_references_in_range(source, root, 0, source.len(), lang)
}

fn collect_type_references(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
    results: &mut BTreeSet<String>,
) {
    collect_type_references_at_depth(node, source, byte_start, byte_end, lang, results, 0);
}

fn collect_type_references_at_depth(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
    results: &mut BTreeSet<String>,
    depth: u32,
) {
    let node_start = node.start_byte();
    let node_end = node.end_byte();

    if node_end <= byte_start || node_start >= byte_end {
        return;
    }

    // Stop descending past MAX_AST_WALK_DEPTH to avoid overflowing the bounded
    // inspect worker stack on pathologically deep trees (minified bundles,
    // generated code, long chains). Treat the truncated subtree as an opaque
    // leaf; type references inside it are intentionally skipped.
    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }

    if node_start >= byte_start && node_end <= byte_end {
        collect_type_reference_fields(&node, source, lang, results);
        if is_type_context_node(lang, node.kind()) {
            collect_type_reference_identifiers(node, source, lang, results);
            return;
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_type_references_at_depth(
                cursor.node(),
                source,
                byte_start,
                byte_end,
                lang,
                results,
                depth + 1,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn collect_type_reference_fields(
    node: &tree_sitter::Node,
    source: &str,
    lang: LangId,
    results: &mut BTreeSet<String>,
) {
    for field in ["type", "return_type", "result", "trait"] {
        if let Some(child) = node.child_by_field_name(field) {
            collect_type_reference_identifiers(child, source, lang, results);
        }
    }

    if matches!(lang, LangId::TypeScript | LangId::Tsx) && node.kind() == "type_alias_declaration" {
        if let Some(value) = node.child_by_field_name("value") {
            collect_type_reference_identifiers(value, source, lang, results);
        }
    }
}

fn is_type_context_node(lang: LangId, kind: &str) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx => matches!(
            kind,
            "type_annotation"
                | "type_arguments"
                | "extends_clause"
                | "implements_clause"
                | "satisfies_expression"
        ),
        LangId::JavaScript => false,
        LangId::Python => kind == "type",
        LangId::Rust => matches!(
            kind,
            "parameter"
                | "field_declaration"
                | "generic_type"
                | "type_arguments"
                | "reference_type"
                | "array_type"
                | "tuple_type"
                | "bounded_type"
        ),
        LangId::Go => matches!(
            kind,
            "field_declaration"
                | "parameter_declaration"
                | "generic_type"
                | "type_arguments"
                | "type_elem"
                | "pointer_type"
                | "array_type"
                | "slice_type"
                | "map_type"
                | "qualified_type"
                | "channel_type"
                | "function_type"
        ),
        _ => false,
    }
}

fn collect_type_reference_identifiers(
    node: tree_sitter::Node,
    source: &str,
    lang: LangId,
    results: &mut BTreeSet<String>,
) {
    collect_type_reference_identifiers_at_depth(node, source, lang, results, 0);
}

fn collect_type_reference_identifiers_at_depth(
    node: tree_sitter::Node,
    source: &str,
    lang: LangId,
    results: &mut BTreeSet<String>,
    depth: u32,
) {
    if is_type_reference_identifier(lang, node.kind()) {
        let name = source[node.byte_range()].trim();
        if let Some(name) = clean_type_reference_name(name) {
            results.insert(name);
        }
    }

    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_type_reference_identifiers_at_depth(
                cursor.node(),
                source,
                lang,
                results,
                depth + 1,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn is_type_reference_identifier(lang: LangId, kind: &str) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx => matches!(kind, "type_identifier" | "identifier"),
        LangId::Python => kind == "identifier",
        LangId::Rust | LangId::Go => kind == "type_identifier",
        _ => false,
    }
}

fn clean_type_reference_name(name: &str) -> Option<String> {
    let name = name
        .rsplit(['.', ':'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(name)
        .trim()
        .trim_start_matches('?');

    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_alphabetic())
    {
        return None;
    }

    Some(name.to_string())
}

/// Extract call expression names within a byte range of the AST.
///
/// Walks all nodes in the tree, finds call_expression/call/macro_invocation
/// nodes whose byte range falls within [byte_start, byte_end], and extracts
/// the callee name (last segment for member access like `obj.method()`).
///
/// Returns (callee_name, line_number) pairs.
pub fn extract_calls_in_range(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<(String, u32)> {
    let mut results = Vec::new();
    let call_kinds = call_node_kinds(lang);
    walk_for_calls(
        root,
        source,
        byte_start,
        byte_end,
        &call_kinds,
        &mut results,
    );
    results
}

/// Extract calls with full callee expressions (including namespace qualifiers).
///
/// Returns `(full_callee, short_name, line, byte_start, byte_end)` tuples.
/// `full_callee` is e.g. "utils.foo", `short_name` is "foo".
pub fn extract_calls_full(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<(String, String, u32, usize, usize)> {
    let mut results = Vec::new();
    let call_kinds = call_node_kinds(lang);
    collect_calls_full(
        root,
        source,
        byte_start,
        byte_end,
        &call_kinds,
        &mut results,
    );
    results
}

fn collect_calls_full(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    call_kinds: &[&str],
    results: &mut Vec<(String, String, u32, usize, usize)>,
) {
    let node_start = node.start_byte();
    let node_end = node.end_byte();

    if node_end <= byte_start || node_start >= byte_end {
        return;
    }

    if call_kinds.contains(&node.kind()) && node_start >= byte_start && node_end <= byte_end {
        if let (Some(full), Some(short)) = (
            extract_full_callee(&node, source),
            extract_callee_name(&node, source),
        ) {
            results.push((
                full,
                short,
                node.start_position().row as u32 + 1,
                node_start,
                node_end,
            ));
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_calls_full(
                cursor.node(),
                source,
                byte_start,
                byte_end,
                call_kinds,
                results,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar_for;

    fn parse_source(lang: LangId, source: &str) -> tree_sitter::Tree {
        let grammar = grammar_for(lang);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar)
            .expect("grammar should initialize");
        parser.parse(source, None).expect("parse source")
    }

    fn parse_typescript(source: &str) -> tree_sitter::Tree {
        parse_source(LangId::TypeScript, source)
    }

    fn deeply_nested_calls(depth: usize) -> String {
        let mut source = String::with_capacity(depth * 3 + 32);
        source.push_str("const x = ");
        for _ in 0..depth {
            source.push_str("f(");
        }
        source.push('a');
        for _ in 0..depth {
            source.push(')');
        }
        source.push_str(";\n");
        source
    }

    fn deeply_nested_type(depth: usize) -> String {
        let mut source = String::with_capacity(depth * 10 + 32);
        source.push_str("type T = ");
        for _ in 0..depth {
            source.push_str("Box<");
        }
        source.push('A');
        for _ in 0..depth {
            source.push('>');
        }
        source.push_str(";\n");
        source
    }

    fn extracted_call_pairs(lang: LangId, source: &str) -> Vec<(String, String)> {
        let tree = parse_source(lang, source);
        extract_calls_full(source, tree.root_node(), 0, source.len(), lang)
            .into_iter()
            .map(|(full, short, _, _, _)| (full, short))
            .collect()
    }

    fn assert_extracted_call(lang: LangId, source: &str, full: &str, short: &str) {
        let calls = extracted_call_pairs(lang, source);
        assert!(
            calls
                .iter()
                .any(|(actual_full, actual_short)| actual_full == full && actual_short == short),
            "expected {full:?}/{short:?} in {calls:?}"
        );
    }

    fn build_fixture_call_data(extension: &str, source: &str) -> crate::callgraph::FileCallData {
        let dir = tempfile::tempdir().expect("create temp fixture dir");
        let path = dir.path().join(format!("fixture.{extension}"));
        std::fs::write(&path, source).expect("write fixture");
        crate::callgraph::build_file_data(&path).expect("build fixture call data")
    }

    fn assert_symbol_has_call(
        data: &crate::callgraph::FileCallData,
        symbol: &str,
        full: &str,
        short: &str,
    ) {
        let calls = data
            .calls_by_symbol
            .iter()
            .find(|(name, _)| name.rsplit("::").next().is_some_and(|tail| tail == symbol))
            .map(|(_, calls)| calls)
            .unwrap_or_else(|| {
                panic!(
                    "expected calls for symbol {symbol:?}; available symbols: {:?}",
                    data.calls_by_symbol.keys().collect::<Vec<_>>()
                )
            });

        assert!(
            calls
                .iter()
                .any(|call| call.full_callee == full && call.callee_name == short),
            "expected {full:?}/{short:?} in calls for {symbol:?}: {calls:?}"
        );
    }

    #[test]
    fn extracts_c_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
int foo(void);
struct Obj { int (*method)(void); };
void caller(struct Obj *p, struct Obj obj) {
    foo();
    obj.method();
    p->method();
}
"#;

        assert_eq!(call_node_kinds(LangId::C), vec!["call_expression"]);
        assert_extracted_call(LangId::C, source, "foo", "foo");
        assert_extracted_call(LangId::C, source, "obj.method", "method");
        assert_extracted_call(LangId::C, source, "p->method", "method");

        let data = build_fixture_call_data("c", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "obj.method", "method");
        assert_symbol_has_call(&data, "caller", "p->method", "method");
    }

    #[test]
    fn extracts_cpp_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
namespace Foo { void bar(); }
struct Painter { void draw(); };
void foo();
void caller(Painter *p, Painter obj) {
    foo();
    obj.draw();
    p->draw();
    Foo::bar();
    Foo::templ<int>();
}
"#;

        assert_eq!(call_node_kinds(LangId::Cpp), vec!["call_expression"]);
        assert_extracted_call(LangId::Cpp, source, "foo", "foo");
        assert_extracted_call(LangId::Cpp, source, "obj.draw", "draw");
        assert_extracted_call(LangId::Cpp, source, "p->draw", "draw");
        assert_extracted_call(LangId::Cpp, source, "Foo::bar", "bar");
        assert_extracted_call(LangId::Cpp, source, "Foo::templ<int>", "templ");

        let data = build_fixture_call_data("cpp", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "p->draw", "draw");
        assert_symbol_has_call(&data, "caller", "Foo::bar", "bar");
    }

    #[test]
    fn extracts_csharp_calls_and_attributes_them_to_method_symbols() {
        let source = r#"
class Service { public void Find() {} }
class Program {
    void Foo() {}
    void Caller(Service svc) {
        Foo();
        svc.Find();
        Generic<int>();
    }
    T Generic<T>() => default;
}
"#;

        assert_eq!(
            call_node_kinds(LangId::CSharp),
            vec!["invocation_expression"]
        );
        assert_extracted_call(LangId::CSharp, source, "Foo", "Foo");
        assert_extracted_call(LangId::CSharp, source, "svc.Find", "Find");
        assert_extracted_call(LangId::CSharp, source, "Generic<int>", "Generic");

        let data = build_fixture_call_data("cs", source);
        assert_symbol_has_call(&data, "Caller", "Foo", "Foo");
        assert_symbol_has_call(&data, "Caller", "svc.Find", "Find");
        assert_symbol_has_call(&data, "Caller", "Generic<int>", "Generic");
    }

    #[test]
    fn extracts_zig_calls_and_attributes_them_to_function_symbols() {
        let source = r#"
fn foo() void {}
const Obj = struct {
    fn method(self: *Obj) void {}
};
fn caller(obj: *Obj) void {
    foo();
    obj.method();
    std.debug.print("x", .{});
}
"#;

        assert_eq!(call_node_kinds(LangId::Zig), vec!["call_expression"]);
        assert_extracted_call(LangId::Zig, source, "foo", "foo");
        assert_extracted_call(LangId::Zig, source, "obj.method", "method");
        assert_extracted_call(LangId::Zig, source, "std.debug.print", "print");

        let data = build_fixture_call_data("zig", source);
        assert_symbol_has_call(&data, "caller", "foo", "foo");
        assert_symbol_has_call(&data, "caller", "obj.method", "method");
        assert_symbol_has_call(&data, "caller", "std.debug.print", "print");
    }

    #[test]
    fn walk_for_calls_deep_tree_does_not_overflow_bounded_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT: a
        // pathologically deep expression (here ~6000 nested calls, far past
        // MAX_AST_WALK_DEPTH) must not recurse unbounded. Run on a bounded-stack
        // worker to prove the guard, not the main thread's larger stack.
        let source = deeply_nested_calls(6_000);
        let tree = parse_typescript(&source);
        let call_kinds = call_node_kinds(LangId::TypeScript);
        let mut results = Vec::new();

        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                walk_for_calls(
                    tree.root_node(),
                    &source,
                    0,
                    source.len(),
                    &call_kinds,
                    &mut results,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep call walk must not overflow the bounded stack");
    }

    #[test]
    fn collect_type_references_deep_tree_does_not_overflow_bounded_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT in the
        // dead-code type-reference side channel. Deep generated type chains are
        // treated as opaque past MAX_AST_WALK_DEPTH instead of recursing until
        // the worker stack overflows.
        let source = deeply_nested_type(6_000);
        let tree = parse_typescript(&source);
        let mut results = BTreeSet::new();

        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                collect_type_references(
                    tree.root_node(),
                    &source,
                    0,
                    source.len(),
                    LangId::TypeScript,
                    &mut results,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep type-reference walk must not overflow the bounded stack");
    }
}
