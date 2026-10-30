//! Shared call-site extraction helpers.
//!
//! Extracted from `commands/zoom.rs` so both the zoom command and the
//! call-graph engine can reuse the same AST-walking logic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::parser::LangId;

// Defensive recursion bound for AST walkers. Hand-written code nests only tens
// of levels deep, but minified bundles, generated code, and very long
// expression/type chains can produce trees thousands of nodes deep. The inspect
// rayon pool uses bounded worker stacks, so unbounded AST recursion here can
// overflow the stack and SIGABRT the entire bridge. Past this depth we stop
// descending and treat the node as an opaque leaf. Kept well under the pool
// stack budget (see dispatch.rs stack_size).
const MAX_AST_WALK_DEPTH: u32 = 1_500;
const MAX_RUST_MACRO_CALL_DEPTH: u32 = 32;

thread_local! {
    static RUST_FRAGMENT_PARSER: std::cell::RefCell<Option<tree_sitter::Parser>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static RUST_FRAGMENT_PARSER_INITIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Returns the tree-sitter node kind strings that represent call expressions
/// for the given language.
pub fn call_node_kinds(lang: LangId) -> &'static [&'static str] {
    match lang {
        LangId::TypeScript | LangId::JavaScript => &["call_expression", "new_expression"],
        LangId::Tsx => &[
            "call_expression",
            "new_expression",
            "jsx_opening_element",
            "jsx_self_closing_element",
        ],
        LangId::Go => &["call_expression"],
        LangId::Python => &["call"],
        LangId::Rust => &["call_expression", "macro_invocation"],
        LangId::Solidity | LangId::Scala => &["call_expression"],
        LangId::Java => &["method_invocation"],
        LangId::Ruby => &["call"],
        LangId::Kotlin | LangId::Swift => &["call_expression"],
        LangId::Php => &[
            "function_call_expression",
            "member_call_expression",
            "nullsafe_member_call_expression",
            "scoped_call_expression",
        ],
        LangId::Perl => &["function_call_expression", "method_call_expression"],
        LangId::Lua => &["function_call"],
        LangId::C | LangId::Cpp | LangId::Cuda | LangId::Metal | LangId::Zig => {
            &["call_expression"]
        }
        LangId::CSharp => &["invocation_expression"],
        LangId::Bash
        | LangId::Scss
        | LangId::Vue
        | LangId::Html
        | LangId::Markdown
        | LangId::Json
        | LangId::Yaml
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC
        | LangId::Toml => &[],
    }
}

/// Recursively walk tree nodes looking for call expressions within a byte range.
///
/// Collects `(callee_name, line_number)` pairs into `results`.
fn walk_for_calls(
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
        | "generic_function"
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
            // Other callee kinds may be expressions rather than names.
            let text = &source[func_node.byte_range()];
            // If it contains a dot, take the last segment
            let name = if text.contains('.') {
                text.rsplit('.').next().map(|s| s.trim().to_string())
            } else {
                Some(text.trim().to_string())
            };
            name.filter(|name| is_name_like_callee(name))
        }
    }
}

/// Keep qualified names and language-specific sigils, but not the source text
/// of an anonymous function, arrow expression, or call result.
fn is_name_like_callee(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|ch| {
            ch.is_alphanumeric() || matches!(ch, '_' | '$' | ':' | '\\' | '@' | '?' | '!')
        })
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
    let func_node = strip_generic_callee_wrapper(func_node);

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

fn strip_generic_callee_wrapper<'a>(node: tree_sitter::Node<'a>) -> tree_sitter::Node<'a> {
    if node.kind() == "generic_function" {
        node.child_by_field_name("function")
            .or_else(|| node.named_child(0))
            .unwrap_or(node)
    } else {
        node
    }
}

fn extract_computed_member_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let index = node.child_by_field_name("index")?;
    let text = source[index.byte_range()].trim();
    computed_member_literal(text)
}

fn computed_member_literal(text: &str) -> Option<String> {
    if text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')))
    {
        return Some(text[1..text.len().saturating_sub(1)].to_string());
    }
    None
}

/// Extract the last segment of a member expression (the method/property name).
fn extract_last_segment(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let node = strip_generic_callee_wrapper(*node);
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
                "generic_function" | "generic_name" | "template_function" | "template_method" => {
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
fn extract_type_references_in_range(
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

    // Building a value with a struct literal (`ZoomResponse { .. }`) uses the
    // type even when no annotation ever names it.
    if lang == LangId::Rust && node.kind() == "struct_expression" {
        if let Some(name) = node.child_by_field_name("name") {
            collect_type_reference_identifiers(name, source, lang, results);
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

/// Extract Go expression references, including callbacks and package variables.
/// Resolution rejects identifiers that do not name indexed package symbols.
pub fn extract_go_value_references(source: &str, root: tree_sitter::Node) -> Vec<CallTuple> {
    fn visit(node: tree_sitter::Node, source: &str, results: &mut Vec<CallTuple>) {
        let parent = node.parent();
        let is_callee = parent.is_some_and(|p| {
            p.kind() == "call_expression" && p.child_by_field_name("function") == Some(node)
        });
        let is_value = node.kind() == "selector_expression"
            || (node.kind() == "identifier"
                && parent.is_some_and(|p| {
                    matches!(
                        p.kind(),
                        "argument_list"
                            | "expression_list"
                            | "return_statement"
                            | "binary_expression"
                            | "unary_expression"
                            | "parenthesized_expression"
                            | "keyed_element"
                            | "literal_element"
                    )
                }));
        if is_value && !is_callee {
            let full = source[node.byte_range()].to_string();
            let short = full.rsplit('.').next().unwrap_or(&full).to_string();
            results.push((
                full,
                short,
                node.start_position().row as u32 + 1,
                node.start_byte(),
                node.end_byte(),
            ));
        }
        // A dotted name is one reference, but a fluent selector's receiver can
        // contain calls with callback arguments. Walk that computed receiver
        // without inventing extra references to the names in a pure dotted path.
        if node.kind() == "selector_expression" {
            let mut operand = node.child_by_field_name("operand");
            while let Some(base) = operand {
                if base.kind() == "selector_expression" {
                    operand = base.child_by_field_name("operand");
                } else {
                    if !matches!(base.kind(), "identifier" | "package_identifier") {
                        visit(base, source, results);
                    }
                    break;
                }
            }
        } else {
            for child in node.named_children(&mut node.walk()) {
                visit(child, source, results);
            }
        }
    }
    let mut results = Vec::new();
    visit(root, source, &mut results);
    results
}

/// Extract bare or namespace-qualified callback arguments, not invocation edges.
pub fn extract_js_callback_references(
    source: &str,
    root: tree_sitter::Node<'_>,
) -> Vec<(String, String, u32, usize, usize)> {
    let mut results = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "arguments" {
            for argument in node.named_children(&mut node.walk()) {
                if matches!(argument.kind(), "identifier" | "member_expression") {
                    let full = source[argument.byte_range()].trim();
                    let root_name = full.split('.').next().unwrap_or(full);
                    if !js_callback_is_shadowed(source, argument, root_name) {
                        results.push((
                            full.to_string(),
                            full.rsplit('.').next().unwrap_or(full).to_string(),
                            argument.start_position().row as u32 + 1,
                            argument.start_byte(),
                            argument.end_byte(),
                        ));
                    }
                }
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    results.sort_by_key(|site| site.3);
    results
}

fn js_callback_is_shadowed(source: &str, argument: tree_sitter::Node<'_>, name: &str) -> bool {
    let mut ancestor = argument.parent();
    while let Some(scope) = ancestor {
        if let Some(parameters) = scope.child_by_field_name("parameters") {
            let mut stack = vec![parameters];
            while let Some(node) = stack.pop() {
                if node.kind() == "identifier" && &source[node.byte_range()] == name {
                    return true;
                }
                stack.extend(node.named_children(&mut node.walk()));
            }
        }
        if matches!(scope.kind(), "statement_block" | "program") {
            for declaration in scope.named_children(&mut scope.walk()) {
                if matches!(
                    declaration.kind(),
                    "lexical_declaration" | "variable_declaration"
                ) {
                    for binding in declaration.named_children(&mut declaration.walk()) {
                        if binding
                            .child_by_field_name("name")
                            .is_some_and(|n| &source[n.byte_range()] == name)
                        {
                            return !binding.child_by_field_name("value").is_some_and(|value| {
                                matches!(value.kind(), "arrow_function" | "function_expression")
                            });
                        }
                    }
                }
            }
        }
        ancestor = scope.parent();
    }
    false
}

/// Extract Rust function paths used as values rather than called directly.
///
/// The call graph stores these separately from calls so execution trees remain
/// call-only while reverse dependencies and dead-code reachability follow callbacks passed to APIs
/// such as `map` and `unwrap_or_else`.
pub fn extract_rust_value_references(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<(String, String, u32, usize, usize)> {
    #[cfg(test)]
    if LEGACY_VALUE_REFERENCES.with(|legacy| legacy.get()) {
        let mut results = Vec::new();
        collect_rust_value_references(root, source, &mut results, 0);
        return results;
    }
    let mut walk = RustValueWalk {
        source,
        ancestors: Vec::new(),
        blocks: HashMap::new(),
        results: Vec::new(),
    };
    // Callers may pass a subtree. Capture its outside ancestry once, rather
    // than asking tree-sitter to rediscover it for every identifier.
    let mut parent = root.parent();
    while let Some(node) = parent {
        walk.ancestors.push(node);
        parent = node.parent();
    }
    walk.ancestors.reverse();
    for node in &walk.ancestors {
        if node.kind() == "block" {
            walk.blocks
                .insert(node.id(), rust_block_bindings(*node, source));
        }
    }
    walk.visit(root, 0);
    walk.results
}

#[cfg(test)]
thread_local! {
    static LEGACY_VALUE_REFERENCES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static VALUE_BINDING_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct RustValueWalk<'tree, 'source> {
    source: &'source str,
    ancestors: Vec<tree_sitter::Node<'tree>>,
    // The first declaration end for a name is enough: the legacy rule asks
    // whether *any* preceding let binds it, not which binding supplies a type.
    blocks: HashMap<usize, HashMap<&'source str, usize>>,
    results: Vec<(String, String, u32, usize, usize)>,
}

fn rust_block_bindings<'source>(
    block: tree_sitter::Node<'_>,
    source: &'source str,
) -> HashMap<&'source str, usize> {
    let mut bindings = HashMap::new();
    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        #[cfg(test)]
        VALUE_BINDING_WORK.with(|work| work.set(work.get() + 1));
        if child.kind() != "let_declaration" {
            continue;
        }
        if let Some(pattern) = child.child_by_field_name("pattern") {
            let mut pending = vec![pattern];
            while let Some(node) = pending.pop() {
                #[cfg(test)]
                VALUE_BINDING_WORK.with(|work| work.set(work.get() + 1));
                if node.kind() == "identifier" {
                    bindings
                        .entry(&source[node.byte_range()])
                        .or_insert(child.end_byte());
                }
                let mut pattern_cursor = node.walk();
                pending.extend(node.children(&mut pattern_cursor));
            }
        }
    }
    bindings
}

impl<'tree> RustValueWalk<'tree, '_> {
    fn visit(&mut self, node: tree_sitter::Node<'tree>, depth: u32) {
        if node.kind() == "block" {
            self.blocks
                .insert(node.id(), rust_block_bindings(node, self.source));
        }
        let nested = self.ancestors.last().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "scoped_identifier" | "scoped_type_identifier" | "generic_function"
            )
        });
        if matches!(node.kind(), "identifier" | "scoped_identifier")
            && !nested
            && self.is_value_position(node)
        {
            let full = self.source[node.byte_range()].trim();
            let short = full.rsplit("::").next().unwrap_or(full).trim();
            if !full.is_empty()
                && !short.is_empty()
                && (node.kind() != "identifier" || !self.is_shadowed(node))
            {
                self.results.push((
                    full.to_string(),
                    short.to_string(),
                    node.start_position().row as u32 + 1,
                    node.start_byte(),
                    node.end_byte(),
                ));
            }
        }
        if depth < MAX_AST_WALK_DEPTH {
            self.ancestors.push(node);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.visit(child, depth + 1);
            }
            self.ancestors.pop();
        }
        if node.kind() == "block" {
            self.blocks.remove(&node.id());
        }
    }

    fn is_value_position(&self, node: tree_sitter::Node<'_>) -> bool {
        let mut current = node;
        for parent in self.ancestors.iter().rev() {
            let selected = |field: &str| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|child| same_node(&child, &current))
            };
            if parent.kind() == "call_expression" && selected("function") {
                return false;
            }
            match parent.kind() {
                "block"
                    if parent.named_child(parent.named_child_count().saturating_sub(1) as u32)
                        == Some(current) =>
                {
                    return true
                }
                "match_arm" if selected("value") => return true,
                "arguments" | "array_expression" | "tuple_expression" => return true,
                "let_declaration" if selected("value") => return true,
                "assignment_expression" if selected("right") => return true,
                "field_initializer" if selected("value") => return true,
                // Reading a static through a receiver or index runs its lazy
                // initializer. The member name itself is not a value use.
                "field_expression" if selected("value") => return true,
                "index_expression" => return true,
                "shorthand_field_initializer" | "return_expression" | "break_expression" => {
                    return true
                }
                "parenthesized_expression"
                | "reference_expression"
                | "unary_expression"
                | "await_expression"
                | "try_expression" => current = *parent,
                _ => return false,
            }
        }
        false
    }

    fn is_shadowed(&self, node: tree_sitter::Node<'_>) -> bool {
        let name = &self.source[node.byte_range()];
        let mut current = node;
        for parent in self.ancestors.iter().rev() {
            match parent.kind() {
                "function_item" | "closure_expression" => {
                    if parent
                        .child_by_field_name("parameters")
                        .is_some_and(|parameters| {
                            rust_pattern_binds_name(parameters, self.source, name)
                        })
                    {
                        return true;
                    }
                }
                "match_arm" => {
                    if parent
                        .child_by_field_name("pattern")
                        .is_some_and(|pattern| rust_pattern_binds_name(pattern, self.source, name))
                    {
                        return true;
                    }
                }
                "for_expression" => {
                    if parent
                        .child_by_field_name("body")
                        .is_some_and(|body| node_contains(&body, &node))
                        && parent
                            .child_by_field_name("pattern")
                            .is_some_and(|pattern| {
                                rust_pattern_binds_name(pattern, self.source, name)
                            })
                    {
                        return true;
                    }
                }
                "if_expression" | "while_expression" => {
                    if parent
                        .child_by_field_name("consequence")
                        .or_else(|| parent.child_by_field_name("body"))
                        .is_some_and(|body| node_contains(&body, &node))
                        && parent
                            .child_by_field_name("condition")
                            .is_some_and(|condition| {
                                rust_let_condition_binds_name(condition, self.source, name)
                            })
                    {
                        return true;
                    }
                }
                "block" => {
                    #[cfg(test)]
                    VALUE_BINDING_WORK.with(|work| work.set(work.get() + 1));
                    if self
                        .blocks
                        .get(&parent.id())
                        .and_then(|bindings| bindings.get(name))
                        .is_some_and(|end| *end <= current.start_byte())
                    {
                        return true;
                    }
                }
                _ => {}
            }
            current = *parent;
        }
        false
    }
}

#[cfg(test)]
fn legacy_rust_value_references(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<(String, String, u32, usize, usize)> {
    let mut results = Vec::new();
    collect_rust_value_references(root, source, &mut results, 0);
    results
}

#[cfg(test)]
fn collect_rust_value_references(
    node: tree_sitter::Node,
    source: &str,
    results: &mut Vec<(String, String, u32, usize, usize)>,
    depth: u32,
) {
    if matches!(node.kind(), "identifier" | "scoped_identifier")
        && !rust_path_is_nested(&node)
        && rust_path_is_value_position(&node)
    {
        let full = source[node.byte_range()].trim();
        let short = full.rsplit("::").next().unwrap_or(full).trim();
        if !full.is_empty()
            && !short.is_empty()
            && (node.kind() != "identifier" || !rust_bare_identifier_is_shadowed(&node, source))
        {
            results.push((
                full.to_string(),
                short.to_string(),
                node.start_position().row as u32 + 1,
                node.start_byte(),
                node.end_byte(),
            ));
        }
    }

    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_rust_value_references(cursor.node(), source, results, depth + 1);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
fn rust_path_is_nested(node: &tree_sitter::Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "scoped_identifier" | "scoped_type_identifier" | "generic_function"
        )
    })
}

#[cfg(test)]
fn rust_path_is_value_position(node: &tree_sitter::Node<'_>) -> bool {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .is_some_and(|function| same_node(&function, &current))
        {
            return false;
        }

        let is_selected_field = |field: &str| {
            parent
                .child_by_field_name(field)
                .is_some_and(|child| same_node(&child, &current))
        };
        match parent.kind() {
            "block"
                if parent.named_child(parent.named_child_count().saturating_sub(1) as u32)
                    == Some(current) =>
            {
                return true
            }
            "match_arm" if is_selected_field("value") => return true,
            "arguments" | "array_expression" | "tuple_expression" => return true,
            "let_declaration" if is_selected_field("value") => return true,
            "assignment_expression" if is_selected_field("right") => return true,
            "field_initializer" if is_selected_field("value") => return true,
            "field_expression" if is_selected_field("value") => return true,
            "index_expression" => return true,
            "shorthand_field_initializer" => return true,
            "return_expression" | "break_expression" => return true,
            "parenthesized_expression"
            | "reference_expression"
            | "unary_expression"
            | "await_expression"
            | "try_expression" => current = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
fn rust_bare_identifier_is_shadowed(node: &tree_sitter::Node<'_>, source: &str) -> bool {
    rust_name_is_shadowed_at(*node, source, &source[node.byte_range()])
}

/// Whether a binding shadows this name at an expression. Parameters, match/loop
/// patterns, let conditions and preceding lets in enclosing blocks count;
/// sibling scopes and later lets do not.
pub(crate) fn rust_name_is_shadowed_at(
    node: tree_sitter::Node<'_>,
    source: &str,
    name: &str,
) -> bool {
    let mut current = node;

    while let Some(parent) = current.parent() {
        match parent.kind() {
            "function_item" | "closure_expression" => {
                if parent
                    .child_by_field_name("parameters")
                    .is_some_and(|parameters| rust_pattern_binds_name(parameters, source, name))
                {
                    return true;
                }
            }
            "match_arm" => {
                if parent
                    .child_by_field_name("pattern")
                    .is_some_and(|pattern| rust_pattern_binds_name(pattern, source, name))
                {
                    return true;
                }
            }
            "for_expression" => {
                if parent
                    .child_by_field_name("pattern")
                    .is_some_and(|pattern| rust_pattern_binds_name(pattern, source, name))
                    && parent
                        .child_by_field_name("body")
                        .is_some_and(|body| node_contains(&body, &node))
                {
                    return true;
                }
            }
            "if_expression" | "while_expression" => {
                let body = parent
                    .child_by_field_name("consequence")
                    .or_else(|| parent.child_by_field_name("body"));
                if body.is_some_and(|body| node_contains(&body, &node))
                    && parent
                        .child_by_field_name("condition")
                        .is_some_and(|condition| {
                            rust_let_condition_binds_name(condition, source, name)
                        })
                {
                    return true;
                }
            }
            "block" => {
                let mut sibling = current.prev_named_sibling();
                while let Some(previous) = sibling {
                    #[cfg(test)]
                    VALUE_BINDING_WORK.with(|work| work.set(work.get() + 1));
                    if previous.kind() == "let_declaration"
                        && previous
                            .child_by_field_name("pattern")
                            .is_some_and(|pattern| rust_pattern_binds_name(pattern, source, name))
                    {
                        return true;
                    }
                    sibling = previous.prev_named_sibling();
                }
            }
            _ => {}
        }
        current = parent;
    }

    false
}

fn rust_let_condition_binds_name(
    condition: tree_sitter::Node<'_>,
    source: &str,
    name: &str,
) -> bool {
    if condition.kind() == "let_condition"
        && condition
            .child_by_field_name("pattern")
            .or_else(|| condition.named_child(0))
            .is_some_and(|pattern| rust_pattern_binds_name(pattern, source, name))
    {
        return true;
    }

    let mut cursor = condition.walk();
    if cursor.goto_first_child() {
        loop {
            if rust_let_condition_binds_name(cursor.node(), source, name) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

fn rust_pattern_binds_name(node: tree_sitter::Node<'_>, source: &str, name: &str) -> bool {
    if node.kind() == "identifier" && &source[node.byte_range()] == name {
        return true;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if rust_pattern_binds_name(cursor.node(), source, name) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

fn same_node(left: &tree_sitter::Node<'_>, right: &tree_sitter::Node<'_>) -> bool {
    left.id() == right.id()
}

fn node_contains(container: &tree_sitter::Node<'_>, node: &tree_sitter::Node<'_>) -> bool {
    container.start_byte() <= node.start_byte() && node.end_byte() <= container.end_byte()
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
    if lang == LangId::Rust {
        return extract_rust_calls_with_macro_facts(source, root, byte_start, byte_end).0;
    }
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

/// Call-site tuple shared by the extraction helpers:
/// `(full_callee, short_name, line, byte_start, byte_end)`.
pub type CallTuple = (String, String, u32, usize, usize);

/// Upper bound on unanalyzed macro mentions recorded for one file. A file full
/// of DSL macros (SQL, HTML templates) would otherwise record every word in
/// them; past this many the note on `callers` output is already clear enough.
const MAX_UNPARSED_MACRO_MENTIONS_PER_FILE: usize = 512;

/// The template of one `macro_rules!` arm, located in the original source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustMacroBody {
    /// Macro name without the `!`.
    pub name: String,
    /// Byte range strictly inside the arm's template delimiters.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// What call extraction learned about Rust macros beyond the calls themselves.
#[derive(Debug, Clone, Default)]
pub struct RustMacroFacts {
    /// Every `macro_rules!` arm template that was re-parsed for calls.
    pub bodies: Vec<RustMacroBody>,
    /// Identifiers inside macro token trees that tree-sitter could not parse
    /// as Rust, excluding names that were still recognized as callees. Each is
    /// a `CallTuple` whose full and short names are the identifier. A call
    /// hidden in such a region is invisible to the call graph, so `callers`
    /// reports these mentions instead of claiming there are no callers.
    /// Only one mention of each name per file is kept: one location per
    /// file is enough to point a reader at the macro, and macro-heavy
    /// files would otherwise record every local variable many times over.
    pub unparsed_mentions: Vec<CallTuple>,
    mention_names: HashSet<String>,
}

impl RustMacroFacts {
    fn push_mention(&mut self, mention: CallTuple) {
        if self.unparsed_mentions.len() < MAX_UNPARSED_MACRO_MENTIONS_PER_FILE
            && self.mention_names.insert(mention.1.clone())
        {
            self.unparsed_mentions.push(mention);
        }
    }
}

/// Rust call extraction including calls written inside macro token trees:
/// invocation arguments (`vec![f(x)]`, `tokio::select! { .. }`) and
/// `macro_rules!` templates, which tree-sitter keeps as opaque tokens. Each
/// token tree is parsed again on its own as Rust; tree-sitter's error recovery
/// still yields the calls in most macro argument shapes. Returns the calls
/// sorted by position, plus the macro facts gathered on the way.
pub fn extract_rust_calls_with_macro_facts(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
) -> (Vec<CallTuple>, RustMacroFacts) {
    let mut results = Vec::new();
    let call_kinds = call_node_kinds(LangId::Rust);
    collect_calls_full(
        root,
        source,
        byte_start,
        byte_end,
        &call_kinds,
        &mut results,
    );
    let mut facts = RustMacroFacts::default();
    collect_rust_macro_calls(
        root,
        source,
        byte_start,
        byte_end,
        0,
        &mut results,
        &mut facts,
    );
    results.sort_by_key(|(_, _, _, start, end)| (*start, *end));
    results.dedup();
    facts
        .unparsed_mentions
        .sort_by_key(|(_, _, _, start, end)| (*start, *end));
    (results, facts)
}

fn collect_rust_macro_calls(
    node: tree_sitter::Node<'_>,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    depth: u32,
    results: &mut Vec<CallTuple>,
    facts: &mut RustMacroFacts,
) {
    // The reparse-depth bound does not bound ordinary AST nesting. A cursor
    // keeps that traversal off the native stack without dropping nested facts.
    let mut cursor = node.walk();
    loop {
        let descend = visit_rust_macro_node(
            cursor.node(),
            source,
            byte_start,
            byte_end,
            depth,
            results,
            facts,
        );
        if descend && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn visit_rust_macro_node(
    node: tree_sitter::Node<'_>,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    depth: u32,
    results: &mut Vec<CallTuple>,
    facts: &mut RustMacroFacts,
) -> bool {
    if depth >= MAX_RUST_MACRO_CALL_DEPTH
        || node.end_byte() <= byte_start
        || node.start_byte() >= byte_end
    {
        return false;
    }

    match node.kind() {
        "macro_invocation" => {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "token_tree" {
                        collect_rust_token_tree_calls(child, source, depth, results, facts);
                        break;
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            return false;
        }
        // A `macro_rules!` template is code that runs wherever the macro is
        // expanded. Whole method families are sometimes written inside one,
        // so its calls are extracted like any other; the body ranges let the
        // call graph tell which calls came from which macro.
        "macro_definition" => {
            let name = node
                .child_by_field_name("name")
                .and_then(|name| source.get(name.byte_range()))
                .map(str::to_string);
            let mut cursor = node.walk();
            for rule in node.children(&mut cursor) {
                if rule.kind() != "macro_rule" {
                    continue;
                }
                let Some(right) = rule.child_by_field_name("right") else {
                    continue;
                };
                if let Some(name) = &name {
                    let range = right.byte_range();
                    if range.end > range.start + 1 {
                        facts.bodies.push(RustMacroBody {
                            name: name.clone(),
                            byte_start: range.start + 1,
                            byte_end: range.end - 1,
                        });
                    }
                }
                collect_rust_token_tree_calls(right, source, depth, results, facts);
            }
            return false;
        }
        _ => {}
    }

    true
}

fn parse_rust_macro_fragment(fragment: &str) -> Option<tree_sitter::Tree> {
    // Release the parser borrow before walking the owned tree: nested macros
    // can then reuse the same parser without recursive RefCell borrowing.
    RUST_FRAGMENT_PARSER.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.is_none() {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&crate::parser::grammar_for(LangId::Rust))
                .ok()?;
            #[cfg(test)]
            RUST_FRAGMENT_PARSER_INITIALIZATIONS.with(|count| count.set(count.get() + 1));
            *cached = Some(parser);
        }
        cached.as_mut()?.parse(fragment, None)
    })
}

fn collect_rust_token_tree_calls(
    token_tree: tree_sitter::Node<'_>,
    source: &str,
    depth: u32,
    results: &mut Vec<CallTuple>,
    facts: &mut RustMacroFacts,
) {
    let range = token_tree.byte_range();
    if range.end <= range.start + 1 {
        return;
    }
    let inner_start = range.start + 1;
    let inner_end = range.end - 1;
    let Some(fragment) = source.get(inner_start..inner_end) else {
        return;
    };

    let Some(tree) = parse_rust_macro_fragment(fragment) else {
        return;
    };

    let mut fragment_calls = Vec::new();
    let mut fragment_facts = RustMacroFacts::default();
    collect_calls_full(
        tree.root_node(),
        fragment,
        0,
        fragment.len(),
        &call_node_kinds(LangId::Rust),
        &mut fragment_calls,
    );
    collect_rust_macro_calls(
        tree.root_node(),
        fragment,
        0,
        fragment.len(),
        depth + 1,
        &mut fragment_calls,
        &mut fragment_facts,
    );
    // A call on a macro metavariable (`$value.len()`, `$f(x)`) names nothing
    // until the macro is expanded, so it is dropped rather than recorded as a
    // call a name-based resolver could bind to an unrelated function.
    // `$crate::path` is the exception: in a macro template `$crate` names the
    // crate that defines the macro, which is the crate of this file, so it is
    // rewritten to the ordinary `crate::path` the resolver understands.
    fragment_calls.retain_mut(|(full, _, _, _, _)| {
        if full.contains("$crate::") {
            *full = full.replace("$crate::", "crate::");
        }
        !full.contains('$')
    });
    if tree.root_node().has_error() {
        collect_unparsed_mentions(
            tree.root_node(),
            fragment,
            &fragment_calls,
            &mut fragment_facts,
        );
    }

    let token_start_row = token_tree.start_position().row as u32;
    let remap = |(full, short, line, start, end): CallTuple| {
        (
            full,
            short,
            token_start_row + line,
            inner_start + start,
            inner_start + end,
        )
    };
    results.extend(fragment_calls.into_iter().map(remap));
    facts
        .bodies
        .extend(fragment_facts.bodies.into_iter().map(|body| RustMacroBody {
            name: body.name,
            byte_start: inner_start + body.byte_start,
            byte_end: inner_start + body.byte_end,
        }));
    for mention in fragment_facts.unparsed_mentions {
        facts.push_mention(remap(mention));
    }
}

/// Record the identifiers tree-sitter had to put in `ERROR` nodes while
/// re-parsing one macro token tree. Names that were recognized as callees
/// anyway are skipped: the call graph already has those calls.
fn collect_unparsed_mentions(
    root: tree_sitter::Node<'_>,
    fragment: &str,
    fragment_calls: &[CallTuple],
    facts: &mut RustMacroFacts,
) {
    let callee_names: BTreeSet<&str> = fragment_calls
        .iter()
        .map(|(_, short, _, _, _)| short.as_str())
        .collect();
    // Explicit stack instead of recursion: fragments come from arbitrary
    // macro input and can nest deeply.
    let mut pending = vec![(root, false)];
    while let Some((node, in_error)) = pending.pop() {
        if facts.unparsed_mentions.len() >= MAX_UNPARSED_MACRO_MENTIONS_PER_FILE {
            return;
        }
        let in_error = in_error || node.is_error();
        if in_error && node.kind() == "identifier" {
            if let Some(name) = fragment.get(node.byte_range()) {
                if !callee_names.contains(name) {
                    facts.push_mention((
                        name.to_string(),
                        name.to_string(),
                        node.start_position().row as u32 + 1,
                        node.start_byte(),
                        node.end_byte(),
                    ));
                }
            }
            continue;
        }
        // Only subtrees that contain an error can hold an unparsed mention.
        if !in_error && !node.has_error() {
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            pending.push((child, in_error));
        }
    }
}

fn collect_calls_full(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    call_kinds: &[&str],
    results: &mut Vec<(String, String, u32, usize, usize)>,
) {
    // Preserve preorder and range pruning while allowing generated expressions
    // to nest beyond the refresh worker's native stack capacity.
    let mut cursor = node.walk();
    loop {
        let node = cursor.node();
        let node_start = node.start_byte();
        let node_end = node.end_byte();
        let overlaps = node_end > byte_start && node_start < byte_end;
        if overlaps {
            if call_kinds.contains(&node.kind()) && node_start >= byte_start && node_end <= byte_end
            {
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
            if cursor.goto_first_child() {
                continue;
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar_for;

    #[test]
    fn rust_value_binding_work_is_linear_and_results_are_identical() {
        let mut source = String::from("fn process() {\n");
        for i in 0..1000 {
            source.push_str(&format!(
                "let v{i} = callback; consume(v{i}); consume(callback);\n"
            ));
        }
        source.push_str("}\nfn scopes(callback: fn()) {\nlet later = callback;\n{ consume(other); let other = later; consume(other); }\nlet (a, b) = (later, ns::callback);\nconsume(a);\nfor other in values { consume(other); consume(callback); }\nif let Some(other) = option { consume(other); } else { consume(other); }\nwhile let Some(other) = option { consume(other); }\nmatch value { Some(other) => consume(other), _ => consume(callback) }\nlet f = |other| { consume(other); consume(callback); };\n}\n");
        let tree = parse_source(LangId::Rust, &source);
        assert!(!tree.root_node().has_error());
        VALUE_BINDING_WORK.with(|work| work.set(0));
        let reference = legacy_rust_value_references(&source, tree.root_node());
        let before = VALUE_BINDING_WORK.with(|work| work.get());
        VALUE_BINDING_WORK.with(|work| work.set(0));
        let actual = extract_rust_value_references(&source, tree.root_node());
        let after = VALUE_BINDING_WORK.with(|work| work.get());
        assert_eq!(actual, reference);
        assert!(actual.len() >= 2000);
        eprintln!("Rust binding work {before} -> {after}");
        assert!(
            after <= 10000,
            "{after} block binding operations for 1000 declarations"
        );
    }

    #[test]
    fn non_name_callee_expressions_are_not_recorded_as_call_names() {
        let source = "(function () { inner(); })();\n(() => 1)();\nfactory()();\nnamed();\n";
        let tree = parse_source(LangId::TypeScript, source);
        let names = extract_calls_full(
            source,
            tree.root_node(),
            0,
            source.len(),
            LangId::TypeScript,
        )
        .into_iter()
        .map(|call| call.1)
        .collect::<Vec<_>>();
        assert_eq!(names, vec!["inner", "factory", "named"]);

        // Go selector expressions use the text fallback but still name a callee.
        let source = "package main\nfunc main() { fmt.Println(1); helper(); }\n";
        let tree = parse_source(LangId::Go, source);
        let names = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Go)
            .into_iter()
            .map(|call| call.1)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Println", "helper"]);
    }

    #[test]
    fn computed_member_literal_rejects_a_single_quote_without_panicking() {
        assert_eq!(computed_member_literal("\""), None);
        assert_eq!(computed_member_literal("'"), None);
        assert_eq!(computed_member_literal("\"\""), Some(String::new()));
        assert_eq!(
            computed_member_literal("'method'"),
            Some("method".to_string())
        );
        assert_eq!(
            computed_member_literal("\"方法\""),
            Some("方法".to_string())
        );
    }

    #[test]
    fn rust_macro_fragments_reuse_one_parser_per_thread() {
        RUST_FRAGMENT_PARSER_INITIALIZATIONS.with(|count| count.set(0));
        let source = "fn main() { check!(first()); check!(second()); check!(nested!(third())); }";
        let tree = parse_source(LangId::Rust, source);
        let (calls, _) =
            extract_rust_calls_with_macro_facts(source, tree.root_node(), 0, source.len());
        for name in ["first", "second", "third"] {
            assert!(calls.iter().any(|call| call.0 == name));
        }
        RUST_FRAGMENT_PARSER_INITIALIZATIONS.with(|count| assert_eq!(count.get(), 1));
    }

    fn deep_calls_on_refresh_worker(lang: LangId, test_name: &str) {
        const DEPTH: usize = 8_000;
        if std::env::var_os("AFT_DEEP_CALLS_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env("AFT_DEEP_CALLS_CHILD", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "refresh worker subprocess failed: {status}"
            );
            return;
        }
        if lang == LangId::Rust {
            // Isolate the macro walker from other file-fact passes, using the
            // same default thread stack as the production refresh worker.
            let source = format!(
                "fn entry() {{ {}check!(leaf()){}; }}",
                "f(".repeat(DEPTH),
                ")".repeat(DEPTH)
            );
            let tree = parse_source(lang, &source);
            assert!(!tree.root_node().has_error());
            std::thread::Builder::new()
                .name("aft-callgraph-refresh".to_string())
                .spawn(move || {
                    let (calls, _) = extract_rust_calls_with_macro_facts(
                        &source,
                        tree.root_node(),
                        0,
                        source.len(),
                    );
                    assert!(calls.iter().any(|call| call.0 == "leaf"));
                })
                .unwrap()
                .join()
                .unwrap();
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let is_rust = lang == LangId::Rust;
        let source_path = root.join(if is_rust { "main.rs" } else { "main.ts" });
        std::fs::write(
            &source_path,
            if is_rust {
                "fn entry() {}"
            } else {
                "function entry() {}"
            },
        )
        .unwrap();
        let artifact_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &artifact_key, false);
        let store_dir = temp.path().join("storage/callgraph").join(artifact_key);
        let (store, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
            store_dir.clone(),
            root.clone(),
            std::slice::from_ref(&source_path),
        )
        .unwrap();
        drop(store);
        // The default export makes the independent export search finish at the
        // declaration, isolating call collection on the refresh worker stack.
        let mut source = if is_rust {
            "fn entry() { ".to_string()
        } else {
            "export default function entry() { ".to_string()
        };
        source.push_str(&"f(".repeat(DEPTH));
        source.push('0');
        source.push_str(&")".repeat(DEPTH));
        source.push_str("; }");
        let tree = parse_source(lang, &source);
        assert!(
            !tree.root_node().has_error(),
            "parser accepts the nested input"
        );
        eprintln!("parsed {DEPTH} nested {lang:?} calls without errors; entering refresh worker");
        std::fs::write(&source_path, &source).unwrap();
        crate::callgraph_store::enqueue_callgraph_store_refresh(
            store_dir.clone(),
            root.clone(),
            vec![source_path],
            std::sync::Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
        );
        assert!(
            crate::callgraph_store::flush_callgraph_store_refreshes_with_budget(
                std::time::Duration::from_secs(30)
            )
        );
        let store = crate::callgraph_store::CallGraphStore::open_readonly(store_dir, root)
            .unwrap()
            .unwrap();
        let entry = store
            .node_for(
                std::path::Path::new(if is_rust { "main.rs" } else { "main.ts" }),
                "entry",
            )
            .unwrap();
        assert_eq!(store.unresolved_calls_of(&entry).unwrap().len(), DEPTH);
    }

    #[test]
    fn deep_typescript_calls_survive_refresh_worker_stack() {
        deep_calls_on_refresh_worker(
            LangId::TypeScript,
            "calls::tests::deep_typescript_calls_survive_refresh_worker_stack",
        );
    }

    #[test]
    fn deep_rust_macro_walk_survives_default_worker_stack() {
        deep_calls_on_refresh_worker(
            LangId::Rust,
            "calls::tests::deep_rust_macro_walk_survives_default_worker_stack",
        );
    }

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
    fn extracts_rust_receiver_calls_inside_macro_tokens() {
        let source = r#"fn compare(before: Index, after: Index) {
    assert!(before.shares_index_with(&after));
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let calls = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Rust);
        let receiver_call = calls
            .iter()
            .find(|(full, _, _, _, _)| full == "before.shares_index_with")
            .expect("receiver call inside macro tokens");
        assert_eq!(receiver_call.1, "shares_index_with");
        assert_eq!(receiver_call.2, 2);
        assert_eq!(
            &source[receiver_call.3..receiver_call.4],
            "before.shares_index_with(&after)"
        );
    }

    #[test]
    fn rust_macro_definitions_do_not_fabricate_receiver_calls() {
        let source = r#"macro_rules! compare {
    ($before:expr, $after:expr) => {
        $before.shares_index_with(&$after)
    };
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let calls = extract_calls_full(source, tree.root_node(), 0, source.len(), LangId::Rust);
        assert!(
            calls
                .iter()
                .all(|(full, _, _, _, _)| full != "$before.shares_index_with"),
            "macro templates are definitions, not call sites: {calls:#?}"
        );
    }

    #[test]
    fn extracts_rust_function_paths_in_value_positions_but_not_direct_calls() {
        let source = r#"
fn callback() {}

fn run() {
    let assigned = callback;
    let tupled = (callback,);
    let arrayed = [callback];
    let holder = Holder { callback };
    consume(crate::callback, assigned, tupled, arrayed, holder);
    callback();
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());
        let names = refs
            .iter()
            .map(|(full, _, _, _, _)| full.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"crate::callback"), "value refs: {refs:#?}");
        assert_eq!(
            names.iter().filter(|name| **name == "callback").count(),
            4,
            "assignment, tuple, array, and shorthand field should be recorded; refs: {refs:#?}"
        );
    }

    #[test]
    fn rust_value_reference_does_not_bind_shadowing_parameter_or_local() {
        let source = r#"
fn callback() {}
fn consume(_: fn()) {}

fn parameter(callback: fn()) {
    consume(callback);
}

fn local() {
    let callback = || {};
    consume(callback);
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());

        assert!(
            refs.iter().all(|(_, short, _, _, _)| short != "callback"),
            "shadowing bindings must not resolve to the function item: {refs:#?}"
        );
    }

    #[test]
    fn rust_value_reference_respects_pattern_bindings_and_lexical_scope() {
        let source = r#"
fn callback() {}
fn consume(_: fn()) {}

fn run(maybe: Option<fn()>, callbacks: Vec<fn()>) {
    if let Some(callback) = maybe {
        consume(callback);
    }
    match maybe {
        Some(callback) => consume(callback),
        None => {}
    }
    for callback in callbacks {
        consume(callback);
    }
    let mut values = callbacks;
    while let Some(callback) = values.pop() {
        consume(callback);
    }
    {
        let callback = || {};
        consume(callback);
    }
    consume(callback);
}
"#;
        let tree = parse_source(LangId::Rust, source);
        let refs = extract_rust_value_references(source, tree.root_node());
        let callback_refs = refs
            .iter()
            .filter(|(_, short, _, _, _)| short == "callback")
            .collect::<Vec<_>>();

        assert_eq!(
            callback_refs.len(),
            1,
            "only the function item outside the closed shadowing scope is a value ref: {refs:#?}"
        );
        assert_eq!(callback_refs[0].2, 24);
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
