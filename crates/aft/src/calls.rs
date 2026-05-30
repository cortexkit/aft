//! Shared call-site extraction helpers.
//!
//! Extracted from `commands/zoom.rs` so both the zoom command and the
//! call-graph engine can reuse the same AST-walking logic.

use std::collections::BTreeSet;

use crate::parser::LangId;

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
        LangId::Perl => vec![
            "call_expression_recursive",
            "call_expression_with_args_with_brackets",
            "call_expression_with_bareword",
            "call_expression_with_spaced_args",
            "call_expression_with_sub",
            "call_expression_with_variable",
            "method_invocation",
        ],
        LangId::Lua => vec!["function_call"],
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Vue
        | LangId::Html
        | LangId::Markdown
        | LangId::Json
        | LangId::Yaml => vec![],
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

    // Recurse into children
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_for_calls(
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
        "member_expression" | "field_expression" | "attribute" => {
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
    let child_count = node.child_count();
    // Walk children from the end looking for an identifier-like node
    for i in (0..child_count).rev() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "property_identifier" | "field_identifier" | "identifier" => {
                    return Some(source[child.byte_range()].to_string());
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
    let node_start = node.start_byte();
    let node_end = node.end_byte();

    if node_end <= byte_start || node_start >= byte_end {
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
            collect_type_references(cursor.node(), source, byte_start, byte_end, lang, results);
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
    if is_type_reference_identifier(lang, node.kind()) {
        let name = source[node.byte_range()].trim();
        if let Some(name) = clean_type_reference_name(name) {
            results.insert(name);
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_type_reference_identifiers(cursor.node(), source, lang, results);
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
/// Returns `(full_callee, short_name, line)` triples.
/// `full_callee` is e.g. "utils.foo", `short_name` is "foo".
pub fn extract_calls_full(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<(String, String, u32)> {
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
    results: &mut Vec<(String, String, u32)>,
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
            results.push((full, short, node.start_position().row as u32 + 1));
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
