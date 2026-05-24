//! Shared extraction utilities for `extract_function` (and future `inline_symbol`).
//!
//! Provides:
//! - `detect_free_variables` — classify identifier references in a byte range
//! - `detect_return_value` — infer what the extracted function should return
//! - `generate_extracted_function` — produce function text for TS/JS or Python

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::indent::IndentStyle;
use crate::parser::{grammar_for, node_text, LangId};

// ---------------------------------------------------------------------------
// Free variable detection
// ---------------------------------------------------------------------------

/// Classification result for free variables in a selected byte range.
#[derive(Debug)]
pub struct FreeVariableResult {
    /// Identifiers declared in an enclosing function scope that the range
    /// references — these become parameters of the extracted function.
    pub parameters: Vec<String>,
    /// Whether `this` (JS/TS) or `self` (Python) appears in the range.
    pub has_this_or_self: bool,
}

/// Walk the AST for a byte range and classify every identifier reference.
///
/// Classification rules:
/// 1. Declared-in-range (local variable) → skip
/// 2. Declared in enclosing function scope → parameter
/// 3. Module-level or import → skip
/// 4. `this` / `self` keyword → flag (error for extract_function)
/// 5. `property_identifier` / `field_identifier` on the right side of `.` → skip
///    (these are member accesses, not free variables)
pub fn detect_free_variables(
    source: &str,
    tree: &Tree,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
) -> FreeVariableResult {
    let root = tree.root_node();

    // 1. Collect all identifiers referenced in the range (excluding property access)
    let mut references: Vec<String> = Vec::new();
    collect_identifier_refs(&root, source, start_byte, end_byte, lang, &mut references);

    // 2. Collect declarations within the range (these are locals, not free)
    let mut local_decls: HashSet<String> = HashSet::new();
    collect_declarations_in_range(&root, source, start_byte, end_byte, lang, &mut local_decls);

    // 3. Find the enclosing function scope boundary
    let enclosing_fn = find_enclosing_function(&root, start_byte, lang);

    // 4. Collect declarations in the enclosing function but outside the range
    let mut enclosing_decls: HashSet<String> = HashSet::new();
    if let Some(fn_node) = enclosing_fn {
        collect_declarations_in_range(
            &fn_node,
            source,
            fn_node.start_byte(),
            start_byte, // only before the range
            lang,
            &mut enclosing_decls,
        );
        // Also collect function parameters
        collect_function_params(&fn_node, source, lang, &mut enclosing_decls);
    }

    // 5. Check for this/self
    let has_this_or_self = check_this_or_self(&root, source, start_byte, end_byte, lang);

    // 6. Classify: a reference is a parameter if it's not a local decl,
    //    IS declared in the enclosing function scope, and is not module-level.
    let mut seen = HashSet::new();
    let mut parameters = Vec::new();
    for name in &references {
        if local_decls.contains(name) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue; // dedup
        }
        if enclosing_decls.contains(name) {
            parameters.push(name.clone());
        }
        // If not in enclosing_decls, it's module-level or global — skip
    }

    FreeVariableResult {
        parameters,
        has_this_or_self,
    }
}

/// Collect all `identifier` nodes in [start_byte, end_byte) that are genuine
/// references (not property accesses on the right side of `.`).
fn collect_identifier_refs(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
    out: &mut Vec<String>,
) {
    // Skip nodes entirely outside the range
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return;
    }

    let kind = node.kind();

    // An `identifier` node in the range that is NOT a property/field access
    if kind == "identifier" && node.start_byte() >= start_byte && node.end_byte() <= end_byte {
        // Check parent: if parent is member_expression and this is the "property" field,
        // it's a property access, not a free variable.
        if !is_property_access(node, lang) {
            let name = node_text(source, node).to_string();
            // Skip language keywords that parse as identifiers
            if !is_keyword(&name, lang) {
                out.push(name);
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_identifier_refs(&cursor.node(), source, start_byte, end_byte, lang, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Check if an identifier node is a property access (right side of `.`).
fn is_property_access(node: &Node, lang: LangId) -> bool {
    // property_identifier and field_identifier are separate node kinds in TS/JS,
    // so they won't even reach here. But for Python `attribute` access the child
    // is still `identifier`.
    if let Some(parent) = node.parent() {
        let pk = parent.kind();
        match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                // member_expression: object.property — the "property" child
                if pk == "member_expression" {
                    if let Some(prop) = parent.child_by_field_name("property") {
                        return prop.id() == node.id();
                    }
                }
            }
            LangId::Python => {
                // attribute: object.attr — the "attribute" child
                if pk == "attribute" {
                    if let Some(attr) = parent.child_by_field_name("attribute") {
                        return attr.id() == node.id();
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Identifiers that are language keywords and should not be treated as free variables.
fn is_keyword(name: &str, lang: LangId) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => matches!(
            name,
            "undefined" | "null" | "true" | "false" | "NaN" | "Infinity" | "console" | "require"
        ),
        LangId::Python => matches!(
            name,
            "None"
                | "True"
                | "False"
                | "print"
                | "len"
                | "range"
                | "str"
                | "int"
                | "float"
                | "list"
                | "dict"
                | "set"
                | "tuple"
                | "type"
                | "super"
                | "isinstance"
                | "enumerate"
                | "zip"
                | "map"
                | "filter"
                | "sorted"
                | "reversed"
                | "any"
                | "all"
                | "min"
                | "max"
                | "sum"
                | "abs"
                | "open"
                | "input"
                | "format"
                | "hasattr"
                | "getattr"
                | "setattr"
                | "delattr"
                | "repr"
                | "iter"
                | "next"
                | "ValueError"
                | "TypeError"
                | "KeyError"
                | "IndexError"
                | "Exception"
                | "RuntimeError"
                | "StopIteration"
                | "NotImplementedError"
                | "AttributeError"
                | "ImportError"
                | "OSError"
                | "IOError"
                | "FileNotFoundError"
        ),
        _ => false,
    }
}

/// Collect names declared (via variable declarations) within a byte range.
fn collect_declarations_in_range(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
    out: &mut HashSet<String>,
) {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return;
    }

    let kind = node.kind();

    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            // variable_declarator has a "name" child that is the declared identifier
            if kind == "variable_declarator" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if name_node.start_byte() >= start_byte && name_node.end_byte() <= end_byte {
                        out.insert(node_text(source, &name_node).to_string());
                    }
                }
            }
        }
        LangId::Python => {
            // assignment: left side
            if kind == "assignment" {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier"
                        && left.start_byte() >= start_byte
                        && left.end_byte() <= end_byte
                    {
                        out.insert(node_text(source, &left).to_string());
                    }
                }
            }
        }
        _ => {}
    }

    // Recurse
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_declarations_in_range(&cursor.node(), source, start_byte, end_byte, lang, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Collect parameter names from a function node.
fn collect_function_params(fn_node: &Node, source: &str, lang: LangId, out: &mut HashSet<String>) {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            // function_declaration / arrow_function have "parameters" field
            if let Some(params) = fn_node.child_by_field_name("parameters") {
                collect_param_identifiers(&params, source, lang, out);
            }
            // For arrow functions inside lexical_declaration, drill down
            let mut cursor = fn_node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "variable_declarator" {
                        if let Some(value) = child.child_by_field_name("value") {
                            if value.kind() == "arrow_function" {
                                if let Some(params) = value.child_by_field_name("parameters") {
                                    collect_param_identifiers(&params, source, lang, out);
                                }
                            }
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        LangId::Python => {
            if let Some(params) = fn_node.child_by_field_name("parameters") {
                collect_param_identifiers(&params, source, lang, out);
            }
        }
        _ => {}
    }
}

/// Walk a parameter list node and collect identifier names.
fn collect_param_identifiers(
    params_node: &Node,
    source: &str,
    lang: LangId,
    out: &mut HashSet<String>,
) {
    let mut cursor = params_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match lang {
                LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                    // required_parameter, optional_parameter have pattern child,
                    // or directly identifier
                    if child.kind() == "required_parameter" || child.kind() == "optional_parameter"
                    {
                        if let Some(pattern) = child.child_by_field_name("pattern") {
                            if pattern.kind() == "identifier" {
                                out.insert(node_text(source, &pattern).to_string());
                            }
                        }
                    } else if child.kind() == "identifier" {
                        out.insert(node_text(source, &child).to_string());
                    }
                }
                LangId::Python => {
                    if child.kind() == "identifier" {
                        let name = node_text(source, &child).to_string();
                        // Skip `self` parameter
                        if name != "self" {
                            out.insert(name);
                        }
                    }
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Find the innermost function node that encloses `byte_pos`.
fn find_enclosing_function<'a>(root: &Node<'a>, byte_pos: usize, lang: LangId) -> Option<Node<'a>> {
    find_deepest_function_ancestor(root, byte_pos, lang)
}

/// Find the deepest function-like ancestor that contains `byte_pos`.
fn find_deepest_function_ancestor<'a>(
    node: &Node<'a>,
    byte_pos: usize,
    lang: LangId,
) -> Option<Node<'a>> {
    let mut result: Option<Node<'a>> = None;
    if is_function_like_boundary(node, byte_pos, lang)
        && node.start_byte() <= byte_pos
        && byte_pos < node.end_byte()
    {
        result = Some(*node);
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if child.start_byte() <= byte_pos && byte_pos < child.end_byte() {
                if let Some(deeper) = find_deepest_function_ancestor(&child, byte_pos, lang) {
                    result = Some(deeper);
                }
            }
        }
    }

    result
}

fn is_function_like_boundary(node: &Node, byte_pos: usize, lang: LangId) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => match node.kind() {
            "function_declaration"
            | "method_definition"
            | "arrow_function"
            | "function_expression" => true,
            "lexical_declaration" => lexical_declaration_has_function_initializer(node, byte_pos),
            _ => false,
        },
        LangId::Python => node.kind() == "function_definition",
        _ => false,
    }
}

fn lexical_declaration_has_function_initializer(node: &Node, byte_pos: usize) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "variable_declarator" {
                if let Some(value) = child.child_by_field_name("value") {
                    if matches!(value.kind(), "arrow_function" | "function_expression")
                        && child.start_byte() <= byte_pos
                        && byte_pos < child.end_byte()
                    {
                        return true;
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

/// Check if `this` (JS/TS) or `self` (Python) appears in the byte range.
fn check_this_or_self(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
) -> bool {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return false;
    }

    if node.start_byte() >= start_byte && node.end_byte() <= end_byte {
        let kind = node.kind();
        match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                if kind == "this" {
                    return true;
                }
            }
            LangId::Python => {
                if kind == "identifier" && node_text(source, node) == "self" {
                    // Check it's not a parameter declaration (like `def foo(self):`)
                    if let Some(parent) = node.parent() {
                        if parent.kind() == "parameters" {
                            return false;
                        }
                    }
                    return true;
                }
            }
            _ => {}
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if check_this_or_self(&cursor.node(), source, start_byte, end_byte, lang) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Return value detection
// ---------------------------------------------------------------------------

/// What the extracted function should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnKind {
    /// The range contains an explicit `return expr;` → use that expression
    Expression(String),
    /// A variable declared in-range is used after the range in the enclosing function
    Variable(String),
    /// Nothing needs to be returned (void)
    Void,
}

const RETURN_VARIABLE_ASSIGNMENT_PREFIX: &str = "\0assignment:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsDeclarationKind {
    Const,
    Let,
    Var,
    Assignment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReturnVariableBinding {
    name: String,
    js_kind: JsDeclarationKind,
}

impl ReturnVariableBinding {
    fn encoded_for_return_kind(&self) -> String {
        match self.js_kind {
            JsDeclarationKind::Const => self.name.clone(),
            JsDeclarationKind::Let => format!("let {}", self.name),
            JsDeclarationKind::Var => format!("var {}", self.name),
            JsDeclarationKind::Assignment => {
                format!("{}{}", RETURN_VARIABLE_ASSIGNMENT_PREFIX, self.name)
            }
        }
    }
}

fn parse_return_variable(var: &str) -> ReturnVariableBinding {
    if let Some(name) = var.strip_prefix(RETURN_VARIABLE_ASSIGNMENT_PREFIX) {
        return ReturnVariableBinding {
            name: name.to_string(),
            js_kind: JsDeclarationKind::Assignment,
        };
    }

    for (prefix, js_kind) in [
        ("let ", JsDeclarationKind::Let),
        ("var ", JsDeclarationKind::Var),
        ("const ", JsDeclarationKind::Const),
    ] {
        if let Some(name) = var.strip_prefix(prefix) {
            return ReturnVariableBinding {
                name: name.to_string(),
                js_kind,
            };
        }
    }

    ReturnVariableBinding {
        name: var.to_string(),
        js_kind: JsDeclarationKind::Const,
    }
}

/// Detect what the extracted code range should return.
///
/// 1. If there's an explicit `return` statement in the range, use its expression.
/// 2. If a variable declared in-range is referenced after the range (but within
///    the enclosing function), that variable becomes the return value.
/// 3. Otherwise, void.
pub fn detect_return_value(
    source: &str,
    tree: &Tree,
    start_byte: usize,
    end_byte: usize,
    enclosing_fn_end_byte: Option<usize>,
    lang: LangId,
) -> ReturnKind {
    let root = tree.root_node();
    let effective_enclosing_fn_end_byte = find_enclosing_function(&root, start_byte, lang)
        .map(|node| node.end_byte())
        .or(enclosing_fn_end_byte);

    // Check for explicit return statements in the range
    if let Some(expr) = find_return_in_range(&root, source, start_byte, end_byte) {
        return ReturnKind::Expression(expr);
    }

    let in_range_bindings =
        collect_return_bindings_in_range(&root, source, start_byte, end_byte, lang);

    // Check if any in-range declaration is used after the range in the enclosing fn
    if let Some(fn_end) = effective_enclosing_fn_end_byte {
        let post_range_end = fn_end.min(source.len());
        if end_byte < post_range_end {
            let mut post_refs: Vec<String> = Vec::new();
            collect_identifier_refs(
                &root,
                source,
                end_byte,
                post_range_end,
                lang,
                &mut post_refs,
            );

            for binding in &in_range_bindings {
                if post_refs.contains(&binding.name) {
                    return ReturnKind::Variable(binding.encoded_for_return_kind());
                }
            }
        }
    }

    ReturnKind::Void
}

/// Collect in-range names that can be returned to preserve post-range uses.
fn collect_return_bindings_in_range(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
) -> Vec<ReturnVariableBinding> {
    let mut bindings = Vec::new();
    collect_return_bindings_recursive(node, source, start_byte, end_byte, lang, &mut bindings);
    bindings
}

fn collect_return_bindings_recursive(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
    out: &mut Vec<ReturnVariableBinding>,
) {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return;
    }

    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            if node.kind() == "variable_declarator" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if name_node.start_byte() >= start_byte && name_node.end_byte() <= end_byte {
                        let name = node_text(source, &name_node).to_string();
                        out.push(ReturnVariableBinding {
                            name,
                            js_kind: js_declaration_kind_for_declarator(node),
                        });
                    }
                }
            } else if is_assignment_node(node) {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier"
                        && left.start_byte() >= start_byte
                        && left.end_byte() <= end_byte
                    {
                        out.push(ReturnVariableBinding {
                            name: node_text(source, &left).to_string(),
                            js_kind: JsDeclarationKind::Assignment,
                        });
                    }
                }
            }
        }
        LangId::Python => {
            if node.kind() == "assignment" {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier"
                        && left.start_byte() >= start_byte
                        && left.end_byte() <= end_byte
                    {
                        out.push(ReturnVariableBinding {
                            name: node_text(source, &left).to_string(),
                            js_kind: JsDeclarationKind::Assignment,
                        });
                    }
                }
            }
        }
        _ => {}
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            collect_return_bindings_recursive(&child, source, start_byte, end_byte, lang, out);
        }
    }
}

fn js_declaration_kind_for_declarator(node: &Node) -> JsDeclarationKind {
    let Some(parent) = node.parent() else {
        return JsDeclarationKind::Const;
    };

    match parent.kind() {
        "variable_declaration" => JsDeclarationKind::Var,
        "lexical_declaration" => {
            let mut cursor = parent.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    match child.kind() {
                        "let" => return JsDeclarationKind::Let,
                        "const" => return JsDeclarationKind::Const,
                        _ => {}
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            JsDeclarationKind::Const
        }
        _ => JsDeclarationKind::Const,
    }
}

fn is_assignment_node(node: &Node) -> bool {
    matches!(
        node.kind(),
        "assignment_expression" | "augmented_assignment_expression" | "assignment"
    )
}

/// Find an explicit `return` statement in the byte range and return its expression text.
fn find_return_in_range(
    node: &Node,
    source: &str,
    start_byte: usize,
    end_byte: usize,
) -> Option<String> {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return None;
    }

    if node.kind() == "return_statement"
        && node.start_byte() >= start_byte
        && node.end_byte() <= end_byte
    {
        // Get the expression after "return"
        let text = node_text(source, node).trim().to_string();
        let expr = text
            .strip_prefix("return")
            .unwrap_or("")
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if !expr.is_empty() {
            return Some(expr);
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if let Some(result) = find_return_in_range(&cursor.node(), source, start_byte, end_byte)
            {
                return Some(result);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Function generation
// ---------------------------------------------------------------------------

/// Generate the text for an extracted function.
pub fn generate_extracted_function(
    name: &str,
    params: &[String],
    return_kind: &ReturnKind,
    body_text: &str,
    base_indent: &str,
    lang: LangId,
    indent_style: IndentStyle,
) -> String {
    let indent_unit = indent_style.as_str();

    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => generate_ts_function(
            name,
            params,
            return_kind,
            body_text,
            base_indent,
            indent_unit,
        ),
        LangId::Python => generate_py_function(
            name,
            params,
            return_kind,
            body_text,
            base_indent,
            indent_unit,
        ),
        _ => {
            // Shouldn't reach here due to language guard, but produce something reasonable
            generate_ts_function(
                name,
                params,
                return_kind,
                body_text,
                base_indent,
                indent_unit,
            )
        }
    }
}

fn generate_ts_function(
    name: &str,
    params: &[String],
    return_kind: &ReturnKind,
    body_text: &str,
    base_indent: &str,
    indent_unit: &str,
) -> String {
    let params_str = params.join(", ");
    let mut lines = Vec::new();

    lines.push(format!(
        "{}function {}({}) {{",
        base_indent, name, params_str
    ));

    // Re-indent body to be inside the function while preserving relative nesting.
    let common_indent = common_leading_indent(body_text);
    for line in body_text.lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            let body_line = strip_leading_indent(line, &common_indent);
            lines.push(format!("{}{}{}", base_indent, indent_unit, body_line));
        }
    }

    // Add return statement if needed
    match return_kind {
        ReturnKind::Variable(var) => {
            let binding = parse_return_variable(var);
            lines.push(format!(
                "{}{}return {};",
                base_indent, indent_unit, binding.name
            ));
        }
        ReturnKind::Expression(_) => {
            // The return is already in the body text
        }
        ReturnKind::Void => {}
    }

    lines.push(format!("{}}}", base_indent));
    lines.join("\n")
}

fn generate_py_function(
    name: &str,
    params: &[String],
    return_kind: &ReturnKind,
    body_text: &str,
    base_indent: &str,
    indent_unit: &str,
) -> String {
    let params_str = params.join(", ");
    let mut lines = Vec::new();

    lines.push(format!("{}def {}({}):", base_indent, name, params_str));

    // Re-indent body while preserving relative nesting.
    let common_indent = common_leading_indent(body_text);
    for line in body_text.lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            let body_line = strip_leading_indent(line, &common_indent);
            lines.push(format!("{}{}{}", base_indent, indent_unit, body_line));
        }
    }

    // Add return statement if needed
    match return_kind {
        ReturnKind::Variable(var) => {
            let binding = parse_return_variable(var);
            lines.push(format!(
                "{}{}return {}",
                base_indent, indent_unit, binding.name
            ));
        }
        ReturnKind::Expression(_) => {
            // Already in body
        }
        ReturnKind::Void => {}
    }

    lines.join("\n")
}

fn common_leading_indent(text: &str) -> String {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else {
        return String::new();
    };

    let mut common = leading_whitespace(first).to_string();
    for line in lines {
        let indent = leading_whitespace(line);
        let common_len = common
            .char_indices()
            .zip(indent.char_indices())
            .take_while(|((_, left), (_, right))| left == right)
            .map(|((idx, ch), _)| idx + ch.len_utf8())
            .last()
            .unwrap_or(0);
        common.truncate(common_len);
        if common.is_empty() {
            break;
        }
    }

    common
}

fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start_matches(|ch: char| ch == ' ' || ch == '\t');
    &line[..line.len() - trimmed.len()]
}

fn strip_leading_indent<'a>(line: &'a str, indent: &str) -> &'a str {
    if indent.is_empty() {
        line
    } else {
        line.strip_prefix(indent).unwrap_or(line)
    }
}

/// Generate the call site text that replaces the extracted range.
pub fn generate_call_site(
    name: &str,
    params: &[String],
    return_kind: &ReturnKind,
    indent: &str,
    lang: LangId,
) -> String {
    let args_str = params.join(", ");

    match return_kind {
        ReturnKind::Variable(var) => match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                let binding = parse_return_variable(var);
                match binding.js_kind {
                    JsDeclarationKind::Const => {
                        format!("{}const {} = {}({});", indent, binding.name, name, args_str)
                    }
                    JsDeclarationKind::Let => {
                        format!("{}let {} = {}({});", indent, binding.name, name, args_str)
                    }
                    JsDeclarationKind::Var => {
                        format!("{}var {} = {}({});", indent, binding.name, name, args_str)
                    }
                    JsDeclarationKind::Assignment => {
                        format!("{}{} = {}({});", indent, binding.name, name, args_str)
                    }
                }
            }
            LangId::Python => {
                let binding = parse_return_variable(var);
                format!("{}{} = {}({})", indent, binding.name, name, args_str)
            }
            _ => format!("{}const {} = {}({});", indent, var, name, args_str),
        },
        ReturnKind::Expression(_expr) => match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                format!("{}return {}({});", indent, name, args_str)
            }
            LangId::Python => {
                format!("{}return {}({})", indent, name, args_str)
            }
            _ => format!("{}return {}({});", indent, name, args_str),
        },
        ReturnKind::Void => match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                format!("{}{}({});", indent, name, args_str)
            }
            LangId::Python => {
                format!("{}{}({})", indent, name, args_str)
            }
            _ => format!("{}{}({});", indent, name, args_str),
        },
    }
}

// ---------------------------------------------------------------------------
// Inline symbol utilities
// ---------------------------------------------------------------------------

/// A detected scope conflict when inlining a function body at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeConflict {
    /// The variable name that conflicts.
    pub name: String,
    /// Suggested alternative name to avoid the conflict.
    pub suggested: String,
}

/// Detect scope conflicts between the call site scope and the function body
/// being inlined.
///
/// Collects all variable declarations at the call site's scope level
/// (surrounding function body), then checks for collisions with variables
/// declared in `body_text`.
pub fn detect_scope_conflicts(
    source: &str,
    tree: &Tree,
    insertion_byte: usize,
    _param_names: &[String],
    body_text: &str,
    lang: LangId,
) -> Vec<ScopeConflict> {
    let root = tree.root_node();

    // 1. Find the enclosing function at the call site
    let enclosing_fn = find_enclosing_function(&root, insertion_byte, lang);

    // 2. Collect all declarations in the call site's scope
    let mut scope_decls: HashSet<String> = HashSet::new();
    if let Some(fn_node) = enclosing_fn {
        collect_declarations_in_range(
            &fn_node,
            source,
            fn_node.start_byte(),
            fn_node.end_byte(),
            lang,
            &mut scope_decls,
        );
        collect_function_params(&fn_node, source, lang, &mut scope_decls);
    } else {
        // Module-level: collect all top-level declarations
        collect_declarations_in_range(
            &root,
            source,
            root.start_byte(),
            root.end_byte(),
            lang,
            &mut scope_decls,
        );
    }

    // 3. Collect declarations in the body being inlined
    let mut body_decls: HashSet<String> = HashSet::new();
    let body_grammar = grammar_for(lang);
    let mut body_parser = tree_sitter::Parser::new();
    if body_parser.set_language(&body_grammar).is_ok() {
        if let Some(body_tree) = body_parser.parse(body_text.as_bytes(), None) {
            let body_root = body_tree.root_node();
            collect_declarations_in_range(
                &body_root,
                body_text,
                0,
                body_text.len(),
                lang,
                &mut body_decls,
            );
        }
    }

    // 4. Find collisions
    let mut conflicts = Vec::new();
    for decl in &body_decls {
        if scope_decls.contains(decl) {
            conflicts.push(ScopeConflict {
                name: decl.clone(),
                suggested: format!("{}_inlined", decl),
            });
        }
    }

    // Sort for deterministic output
    conflicts.sort_by(|a, b| a.name.cmp(&b.name));
    conflicts
}

/// Validate that a function has at most one return statement (suitable for inlining).
///
/// - Arrow functions with expression bodies (no `return` keyword) → valid (single-return)
/// - Functions with 0 returns (void) → valid
/// - Functions with exactly 1 return → valid
/// - Functions with >1 return → invalid, returns the count
pub fn validate_single_return(
    source: &str,
    _tree: &Tree,
    fn_node: &Node,
    lang: LangId,
) -> Result<(), usize> {
    // Arrow functions with expression bodies are always single-return
    if lang != LangId::Python && fn_node.kind() == "arrow_function" {
        if let Some(body) = fn_node.child_by_field_name("body") {
            if body.kind() != "statement_block" {
                // Expression body — implicitly single-return
                return Ok(());
            }
        }
    }

    let count = count_return_statements(fn_node, source);
    if count > 1 {
        Err(count)
    } else {
        Ok(())
    }
}

/// Count `return_statement` nodes in a function body (non-recursive into nested functions).
fn count_return_statements(node: &Node, source: &str) -> usize {
    let _ = source;
    let mut count = 0;

    // Don't count returns in nested function bodies
    let nested_fn_kinds = [
        "function_declaration",
        "function_definition",
        "arrow_function",
        "method_definition",
    ];

    let kind = node.kind();
    if kind == "return_statement" {
        return 1;
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            // Skip nested function definitions
            if nested_fn_kinds.contains(&child.kind()) {
                continue;
            }
            count += count_return_statements(&child, source);
        }
    }

    count
}

/// Substitute parameter names with argument expressions in a function body.
///
/// Uses tree-sitter to find `identifier` nodes matching parameter names,
/// replacing from end to start to preserve byte offsets. Only replaces
/// whole-word matches (identifiers, not substrings).
pub fn substitute_params(
    body_text: &str,
    param_to_arg: &std::collections::HashMap<String, String>,
    lang: LangId,
) -> String {
    if param_to_arg.is_empty() {
        return body_text.to_string();
    }

    let grammar = grammar_for(lang);
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return body_text.to_string();
    }

    let tree = match parser.parse(body_text.as_bytes(), None) {
        Some(t) => t,
        None => return body_text.to_string(),
    };

    // Collect identifier references that are still bound to the inlined function's
    // parameters. Nested functions and local shadowing declarations are skipped.
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let shadowed = HashSet::new();
    collect_param_replacements(
        &tree.root_node(),
        body_text,
        param_to_arg,
        lang,
        &shadowed,
        true,
        &mut replacements,
    );

    // Sort by start position descending so replacements don't shift offsets
    replacements.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = body_text.to_string();
    for (start, end, replacement) in replacements {
        result = format!("{}{}{}", &result[..start], replacement, &result[end..]);
    }

    result
}

/// Collect identifier nodes that match parameter names for substitution.
fn collect_param_replacements(
    node: &Node,
    source: &str,
    param_to_arg: &std::collections::HashMap<String, String>,
    lang: LangId,
    shadowed: &HashSet<String>,
    is_root: bool,
    out: &mut Vec<(usize, usize, String)>,
) {
    if !is_root && is_function_scope_node(node, lang) {
        return;
    }

    let mut current_shadowed = shadowed.clone();
    collect_shadowing_bindings_in_scope(node, source, param_to_arg, lang, &mut current_shadowed);

    let kind = node.kind();

    if kind == "identifier" {
        // Check it's not a property access
        if !is_property_access(node, lang) && !is_binding_identifier(node) {
            let name = node_text(source, node);
            if !current_shadowed.contains(name) {
                if let Some(replacement) = param_to_arg.get(name) {
                    out.push((node.start_byte(), node.end_byte(), replacement.clone()));
                }
            }
        }
    }

    // Also handle Python-specific name node
    // Recurse into children
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            collect_param_replacements(
                &child,
                source,
                param_to_arg,
                lang,
                &current_shadowed,
                false,
                out,
            );
        }
    }
}

fn collect_shadowing_bindings_in_scope(
    scope: &Node,
    source: &str,
    param_to_arg: &std::collections::HashMap<String, String>,
    lang: LangId,
    out: &mut HashSet<String>,
) {
    collect_shadowing_bindings_in_scope_recursive(
        scope,
        scope.id(),
        source,
        param_to_arg,
        lang,
        out,
    );
}

fn collect_shadowing_bindings_in_scope_recursive(
    node: &Node,
    scope_id: usize,
    source: &str,
    param_to_arg: &std::collections::HashMap<String, String>,
    lang: LangId,
    out: &mut HashSet<String>,
) {
    if node.id() != scope_id {
        if is_function_scope_node(node, lang) || is_block_scope_node(node, lang) {
            return;
        }
    }

    match node.kind() {
        "variable_declarator" => {
            if let Some(name) = node.child_by_field_name("name") {
                collect_shadowing_names_from_pattern(&name, source, param_to_arg, out);
            }
        }
        "catch_clause" => {
            if let Some(parameter) = node.child_by_field_name("parameter") {
                collect_shadowing_names_from_pattern(&parameter, source, param_to_arg, out);
            }
        }
        "for_in_statement" | "for_of_statement" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_shadowing_names_from_pattern(&left, source, param_to_arg, out);
            }
        }
        "assignment" if lang == LangId::Python => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_shadowing_names_from_pattern(&left, source, param_to_arg, out);
            }
        }
        _ => {}
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            collect_shadowing_bindings_in_scope_recursive(
                &child,
                scope_id,
                source,
                param_to_arg,
                lang,
                out,
            );
        }
    }
}

fn collect_shadowing_names_from_pattern(
    node: &Node,
    source: &str,
    param_to_arg: &std::collections::HashMap<String, String>,
    out: &mut HashSet<String>,
) {
    if node.kind() == "identifier" {
        let name = node_text(source, node);
        if param_to_arg.contains_key(name) {
            out.insert(name.to_string());
        }
        return;
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            collect_shadowing_names_from_pattern(&child, source, param_to_arg, out);
        }
    }
}

fn is_function_scope_node(node: &Node, lang: LangId) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => matches!(
            node.kind(),
            "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
        ),
        LangId::Python => node.kind() == "function_definition" || node.kind() == "lambda",
        _ => false,
    }
}

fn is_block_scope_node(node: &Node, lang: LangId) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => node.kind() == "statement_block",
        LangId::Python => node.kind() == "block",
        _ => false,
    }
}

fn is_binding_identifier(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    if let Some(name) = parent.child_by_field_name("name") {
        if name.id() == node.id() || node_is_inside(&name, node) {
            return true;
        }
    }
    if let Some(pattern) = parent.child_by_field_name("pattern") {
        if pattern.id() == node.id() || node_is_inside(&pattern, node) {
            return true;
        }
    }
    if let Some(parameter) = parent.child_by_field_name("parameter") {
        if parameter.id() == node.id() || node_is_inside(&parameter, node) {
            return true;
        }
    }
    if let Some(left) = parent.child_by_field_name("left") {
        if matches!(
            parent.kind(),
            "for_in_statement" | "for_of_statement" | "assignment"
        ) && (left.id() == node.id() || node_is_inside(&left, node))
        {
            return true;
        }
    }

    false
}

fn node_is_inside(container: &Node, node: &Node) -> bool {
    container.start_byte() <= node.start_byte() && node.end_byte() <= container.end_byte()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar_for;
    use std::path::PathBuf;
    use tree_sitter::Parser;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("extract_function")
            .join(name)
    }

    fn parse_source(source: &str, lang: LangId) -> Tree {
        let grammar = grammar_for(lang);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        parser.parse(source.as_bytes(), None).unwrap()
    }

    // --- Free variable detection: simple identifiers ---

    #[test]
    fn free_vars_detects_enclosing_function_params() {
        // `items` and `prefix` are function params → should be detected as free variables
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // Lines 5-8 (0-indexed): the body of processData that uses `items` and `prefix`
        // "  const filtered = items.filter(item => item.length > 0);"
        // "  const mapped = filtered.map(item => prefix + item);"
        // These lines reference `items` and `prefix` from the function params.
        let line5_start = crate::edit::line_col_to_byte(&source, 5, 0);
        let line6_end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result =
            detect_free_variables(&source, &tree, line5_start, line6_end, LangId::TypeScript);
        assert!(
            result.parameters.contains(&"items".to_string()),
            "should detect 'items' as parameter, got: {:?}",
            result.parameters
        );
        assert!(
            result.parameters.contains(&"prefix".to_string()),
            "should detect 'prefix' as parameter, got: {:?}",
            result.parameters
        );
        assert!(!result.has_this_or_self);
    }

    // --- Property access filtering ---

    #[test]
    fn free_vars_filters_property_identifiers() {
        // In `items.filter(...)`, `filter` should NOT be a free variable.
        // In `item.length`, `length` should NOT be a free variable.
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        let line5_start = crate::edit::line_col_to_byte(&source, 5, 0);
        let line6_end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result =
            detect_free_variables(&source, &tree, line5_start, line6_end, LangId::TypeScript);
        // "filter", "map", "length" should NOT appear
        assert!(
            !result.parameters.contains(&"filter".to_string()),
            "property 'filter' should not be a free variable"
        );
        assert!(
            !result.parameters.contains(&"length".to_string()),
            "property 'length' should not be a free variable"
        );
        assert!(
            !result.parameters.contains(&"map".to_string()),
            "property 'map' should not be a free variable"
        );
    }

    // --- Module-level vs function-level classification ---

    #[test]
    fn free_vars_skips_module_level_refs() {
        // `BASE_URL` is module-level → should NOT be a parameter
        // `console` is a global → should NOT be a parameter
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // processData body: lines 5-9
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 10, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::TypeScript);
        assert!(
            !result.parameters.contains(&"BASE_URL".to_string()),
            "module-level 'BASE_URL' should not be a parameter, got: {:?}",
            result.parameters
        );
        assert!(
            !result.parameters.contains(&"console".to_string()),
            "'console' should not be a parameter, got: {:?}",
            result.parameters
        );
    }

    #[test]
    fn free_vars_plain_lexical_declaration_uses_real_enclosing_function() {
        let source = "function f(a: number) {\n  const x = a + 1;\n  return x;\n}\n";
        let tree = parse_source(source, LangId::TypeScript);
        let start = crate::edit::line_col_to_byte(source, 1, 0);
        let end = crate::edit::line_col_to_byte(source, 2, 0);

        let result = detect_free_variables(source, &tree, start, end, LangId::TypeScript);
        assert!(
            result.parameters.contains(&"a".to_string()),
            "plain const declaration should not stop enclosing-function lookup: {:?}",
            result.parameters
        );
    }

    // --- this/self detection ---

    #[test]
    fn free_vars_detects_this_in_ts() {
        let source = std::fs::read_to_string(fixture_path("sample_this.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // getUser method body lines 4-6 contain `this.users.get(key)`
        let start = crate::edit::line_col_to_byte(&source, 4, 0);
        let end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::TypeScript);
        assert!(result.has_this_or_self, "should detect 'this' reference");
    }

    #[test]
    fn free_vars_detects_self_in_python() {
        let source = r#"
class UserService:
    def get_user(self, id):
        key = id.lower()
        user = self.users.get(key)
        return user
"#;
        let tree = parse_source(source, LangId::Python);

        // Lines 3-4 (0-indexed) contain `self.users.get(key)`
        let start = crate::edit::line_col_to_byte(source, 4, 0);
        let end = crate::edit::line_col_to_byte(source, 5, 0);

        let result = detect_free_variables(source, &tree, start, end, LangId::Python);
        assert!(result.has_this_or_self, "should detect 'self' reference");
    }

    // --- Return value detection ---

    #[test]
    fn return_value_explicit_return() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // simpleHelper: lines 13-16 — contains "return added;"
        let start = crate::edit::line_col_to_byte(&source, 14, 0);
        let end = crate::edit::line_col_to_byte(&source, 17, 0);

        let result = detect_return_value(&source, &tree, start, end, None, LangId::TypeScript);
        assert_eq!(result, ReturnKind::Expression("added".to_string()));
    }

    #[test]
    fn return_value_post_range_usage() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // processData lines 5-7: declares `filtered`, `mapped`
        // Lines 7-9 use `result` (which comes after mapped), but line 7 declares `result`
        // Let's extract lines 5-6 only: `filtered` and `mapped` are declared
        // and `filtered` is used on line 6, `mapped` is used on line 7
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 6, 0);

        // Enclosing function ends around line 10
        let fn_end = crate::edit::line_col_to_byte(&source, 10, 0);

        let result =
            detect_return_value(&source, &tree, start, end, Some(fn_end), LangId::TypeScript);
        // `filtered` is declared in-range and used after the range
        assert_eq!(result, ReturnKind::Variable("filtered".to_string()));
    }

    #[test]
    fn return_value_void() {
        let source = std::fs::read_to_string(fixture_path("sample.ts")).unwrap();
        let tree = parse_source(&source, LangId::TypeScript);

        // voidWork lines 20-21: no return, `greeting` is only used within
        let start = crate::edit::line_col_to_byte(&source, 20, 0);
        let end = crate::edit::line_col_to_byte(&source, 22, 0);

        let result = detect_return_value(
            &source,
            &tree,
            start,
            end,
            Some(crate::edit::line_col_to_byte(&source, 23, 0)),
            LangId::TypeScript,
        );
        assert_eq!(result, ReturnKind::Void);
    }

    // --- Function generation ---

    #[test]
    fn generate_ts_function_with_params() {
        let body = "const doubled = x * 2;\nconst added = doubled + 10;";
        let result = generate_extracted_function(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("added".to_string()),
            body,
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(2),
        );
        assert!(result.contains("function compute(x)"));
        assert!(result.contains("return added;"));
        assert!(result.contains("}"));
    }

    #[test]
    fn generate_ts_function_preserves_relative_indentation() {
        let body = "  for (const item of items) {\n    if (item.active) {\n      console.log(item.name);\n    }\n  }";
        let result = generate_extracted_function(
            "processItems",
            &["items".to_string()],
            &ReturnKind::Void,
            body,
            "",
            LangId::TypeScript,
            IndentStyle::Spaces(2),
        );
        assert_eq!(
            result,
            "function processItems(items) {\n  for (const item of items) {\n    if (item.active) {\n      console.log(item.name);\n    }\n  }\n}"
        );
    }

    #[test]
    fn generate_py_function_with_params() {
        let body = "doubled = x * 2\nadded = doubled + 10";
        let result = generate_extracted_function(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("added".to_string()),
            body,
            "",
            LangId::Python,
            IndentStyle::Spaces(4),
        );
        assert!(result.contains("def compute(x):"));
        assert!(result.contains("return added"));
    }

    #[test]
    fn generate_call_site_with_return_var() {
        let call = generate_call_site(
            "compute",
            &["x".to_string()],
            &ReturnKind::Variable("result".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  const result = compute(x);");
    }

    #[test]
    fn generate_call_site_preserves_let_return_var() {
        let call = generate_call_site(
            "compute",
            &[],
            &ReturnKind::Variable("let result".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  let result = compute();");
    }

    #[test]
    fn generate_call_site_void() {
        let call = generate_call_site(
            "doWork",
            &["a".to_string(), "b".to_string()],
            &ReturnKind::Void,
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  doWork(a, b);");
    }

    #[test]
    fn generate_call_site_return_expression() {
        let call = generate_call_site(
            "compute",
            &["x".to_string()],
            &ReturnKind::Expression("x * 2".to_string()),
            "  ",
            LangId::TypeScript,
        );
        assert_eq!(call, "  return compute(x);");
    }

    // --- Python free variables ---

    #[test]
    fn free_vars_python_function_params() {
        let source = std::fs::read_to_string(fixture_path("sample.py")).unwrap();
        let tree = parse_source(&source, LangId::Python);

        // process_data body: lines 5-8 reference `items` and `prefix`
        let start = crate::edit::line_col_to_byte(&source, 5, 0);
        let end = crate::edit::line_col_to_byte(&source, 7, 0);

        let result = detect_free_variables(&source, &tree, start, end, LangId::Python);
        assert!(
            result.parameters.contains(&"items".to_string()),
            "should detect 'items': {:?}",
            result.parameters
        );
        assert!(
            result.parameters.contains(&"prefix".to_string()),
            "should detect 'prefix': {:?}",
            result.parameters
        );
        assert!(!result.has_this_or_self);
    }

    // --- validate_single_return ---

    #[test]
    fn validate_single_return_single() {
        let source =
            "function add(a: number, b: number): number {\n  const sum = a + b;\n  return sum;\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap(); // function_declaration
        assert!(validate_single_return(source, &tree, &fn_node, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_void() {
        let source = "function greet(name: string): void {\n  console.log(name);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap();
        assert!(validate_single_return(source, &tree, &fn_node, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_expression_body() {
        let source = "const double = (n: number): number => n * 2;";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        // lexical_declaration > variable_declarator > arrow_function
        let lex_decl = root.child(0).unwrap();
        let var_decl = lex_decl.child(1).unwrap(); // variable_declarator
        let arrow = var_decl.child_by_field_name("value").unwrap();
        assert_eq!(arrow.kind(), "arrow_function");
        assert!(validate_single_return(source, &tree, &arrow, LangId::TypeScript).is_ok());
    }

    #[test]
    fn validate_single_return_multiple() {
        let source = "function abs(x: number): number {\n  if (x > 0) {\n    return x;\n  }\n  return -x;\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let root = tree.root_node();
        let fn_node = root.child(0).unwrap();
        let result = validate_single_return(source, &tree, &fn_node, LangId::TypeScript);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 2);
    }

    // --- detect_scope_conflicts ---

    #[test]
    fn scope_conflicts_none() {
        // No overlap between call site scope and body vars
        let source = "function main() {\n  const x = 10;\n  const y = add(x, 5);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let body_text = "const sum = a + b;";
        let call_byte = crate::edit::line_col_to_byte(source, 2, 0);
        let conflicts =
            detect_scope_conflicts(source, &tree, call_byte, &[], body_text, LangId::TypeScript);
        assert!(
            conflicts.is_empty(),
            "expected no conflicts, got: {:?}",
            conflicts
        );
    }

    #[test]
    fn scope_conflicts_detected() {
        // `temp` exists at call site and inside body
        let source = "function main() {\n  const temp = 99;\n  const result = compute(5);\n}";
        let tree = parse_source(source, LangId::TypeScript);
        let body_text = "const temp = x * 2;\nconst result2 = temp + 10;";
        let call_byte = crate::edit::line_col_to_byte(source, 2, 0);
        let conflicts =
            detect_scope_conflicts(source, &tree, call_byte, &[], body_text, LangId::TypeScript);
        assert!(!conflicts.is_empty(), "expected conflict for 'temp'");
        assert!(
            conflicts.iter().any(|c| c.name == "temp"),
            "conflicts: {:?}",
            conflicts
        );
        assert!(
            conflicts.iter().any(|c| c.suggested == "temp_inlined"),
            "should suggest temp_inlined"
        );
    }

    // --- substitute_params ---

    #[test]
    fn substitute_params_basic() {
        let body = "const sum = a + b;";
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), "x".to_string());
        map.insert("b".to_string(), "y".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript);
        assert_eq!(result, "const sum = x + y;");
    }

    #[test]
    fn substitute_params_whole_word() {
        // Should NOT replace `i` inside `items`
        let body = "const result = items.filter(i => i > 0);";
        let mut map = std::collections::HashMap::new();
        map.insert("i".to_string(), "index".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript);
        // `items` should be untouched, and the nested arrow's shadowing `i`
        // parameter/reference pair should also be left alone.
        assert_eq!(result, body);
    }

    #[test]
    fn substitute_params_rewrites_outer_reference_not_shadowed_arrow_param() {
        let body = "return x + items.map(x => x + 1)[0];";
        let mut map = std::collections::HashMap::new();
        map.insert("x".to_string(), "5".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript);
        assert_eq!(result, "return 5 + items.map(x => x + 1)[0];");
    }

    #[test]
    fn substitute_params_noop_same_name() {
        let body = "const sum = x + y;";
        let mut map = std::collections::HashMap::new();
        map.insert("x".to_string(), "x".to_string());
        let result = substitute_params(body, &map, LangId::TypeScript);
        assert_eq!(result, "const sum = x + y;");
    }

    #[test]
    fn substitute_params_empty_map() {
        let body = "const sum = a + b;";
        let map = std::collections::HashMap::new();
        let result = substitute_params(body, &map, LangId::TypeScript);
        assert_eq!(result, body);
    }
}
