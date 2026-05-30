//! Handler for the `add_member` command: insert a method, field, or function
//! into a scope container (class, struct, impl block) with correct indentation.
//!
//! Supports TS/JS classes, Python classes, Rust structs/impl blocks, and Go
//! structs. Resolves insertion position via `first`, `last`, `before:name`,
//! `after:name`.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
use crate::indent::{detect_indent, IndentStyle};
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `add_member` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `scope` (string, required) — name of class/struct/impl to target
///   - `code` (string, required) — the member code to insert
///   - `position` (string, optional) — `first`, `last`, `before:name`, `after:name` (default `last`)
///
/// Returns: `{ file, scope, position, syntax_valid?, backup_id? }`
pub fn handle_add_member(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_member: missing required param 'file'",
            );
        }
    };

    let scope_name = match req.params.get("scope").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_member: missing required param 'scope'",
            );
        }
    };

    let code = match req.params.get("code").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_member: missing required param 'code'",
            );
        }
    };

    let position = req
        .params
        .get("position")
        .and_then(|v| v.as_str())
        .unwrap_or("last");

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("add_member: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "add_member: unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            );
        }
    };

    // --- Parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("add_member: cannot read file: {}", e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&grammar) {
        return Response::error(
            &req.id,
            "parse_error",
            format!("add_member: grammar init failed: {}", e),
        );
    }

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!("add_member: parse failed for {}", file),
            );
        }
    };

    let root = tree.root_node();

    // --- Find scope container ---
    let (scope_node_result, available) = find_scope_container(&root, &source, scope_name, lang);
    let (body_node_start, body_node_end, body_children) = match scope_node_result {
        Some(info) => info,
        None => {
            let err = AftError::ScopeNotFound {
                scope: scope_name.to_string(),
                available,
                file: file.to_string(),
            };
            return Response::error(&req.id, err.code(), err.to_string());
        }
    };

    // --- Detect indentation ---
    let file_indent = detect_indent(&source, lang);
    let member_indent = detect_member_indent(&source, &body_children, &file_indent, lang);

    // --- Resolve insertion position ---
    let insert_offset = match resolve_position(
        &source,
        position,
        body_node_start,
        body_node_end,
        &body_children,
        lang,
        scope_name,
        file,
    ) {
        Ok(offset) => offset,
        Err(err) => {
            return Response::error(&req.id, err.code(), err.to_string());
        }
    };

    // --- Indent the provided code ---
    let indented_code = indent_code(
        code,
        &member_indent,
        &source,
        insert_offset,
        position,
        body_node_start,
        body_node_end,
        &body_children,
        lang,
    );

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_member: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Insert ---
    let new_source =
        match edit::replace_byte_range(&source, insert_offset, insert_offset, &indented_code) {
            Ok(s) => s,
            Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
        };

    // --- Write, format, and validate ---
    let mut write_result =
        match edit::write_format_validate(&path, &new_source, &ctx.config(), &req.params) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    log::debug!("add_member: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "scope": scope_name,
        "position": position,
        "formatted": write_result.formatted,
    });

    if let Some(valid) = write_result.syntax_valid {
        result["syntax_valid"] = serde_json::json!(valid);
    }

    if let Some(ref reason) = write_result.format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if write_result.validate_requested {
        result["validation_errors"] = serde_json::json!(write_result.validation_errors);
    }
    if let Some(ref reason) = write_result.validate_skipped_reason {
        result["validate_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    write_result.append_lsp_diagnostics_to(&mut result);
    Response::success(&req.id, result)
}

/// Info about a body: (body_start_byte, body_end_byte, named_children)
/// where named_children are (name, start_byte, end_byte) tuples.
type BodyInfo = (usize, usize, Vec<BodyChild>);

#[derive(Debug)]
struct BodyChild {
    name: String,
    start_byte: usize,
    end_byte: usize,
}

/// Find a scope container node by name, returning body info and available scope names.
fn find_scope_container(
    root: &Node,
    source: &str,
    scope_name: &str,
    lang: LangId,
) -> (Option<BodyInfo>, Vec<String>) {
    let mut available: Vec<String> = Vec::new();

    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            // Walk for class_declaration matching name
            let mut cursor = root.walk();
            if cursor.goto_first_child() {
                loop {
                    let node = cursor.node();
                    if node.kind() == "class_declaration" {
                        if let Some(name) =
                            find_child_text(&node, source, &["type_identifier", "identifier"])
                        {
                            available.push(name.clone());
                            if name == scope_name {
                                if let Some(body) = find_child_by_kind(&node, "class_body") {
                                    let children = extract_body_children_ts(&body, source);
                                    return (
                                        Some((body.start_byte(), body.end_byte(), children)),
                                        available,
                                    );
                                }
                            }
                        }
                    }
                    // Also check export_statement wrapping class
                    if node.kind() == "export_statement" {
                        let mut inner = node.walk();
                        if inner.goto_first_child() {
                            loop {
                                let child = inner.node();
                                if child.kind() == "class_declaration" {
                                    if let Some(name) = find_child_text(
                                        &child,
                                        source,
                                        &["type_identifier", "identifier"],
                                    ) {
                                        if !available.contains(&name) {
                                            available.push(name.clone());
                                        }
                                        if name == scope_name {
                                            if let Some(body) =
                                                find_child_by_kind(&child, "class_body")
                                            {
                                                let children =
                                                    extract_body_children_ts(&body, source);
                                                return (
                                                    Some((
                                                        body.start_byte(),
                                                        body.end_byte(),
                                                        children,
                                                    )),
                                                    available,
                                                );
                                            }
                                        }
                                    }
                                }
                                if !inner.goto_next_sibling() {
                                    break;
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
            // Walk for class_definition matching identifier
            fn walk_py_classes<'a>(
                node: &Node<'a>,
                source: &str,
                scope_name: &str,
                available: &mut Vec<String>,
            ) -> Option<BodyInfo> {
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind() == "class_definition" {
                            if let Some(name) = find_child_text(&child, source, &["identifier"]) {
                                available.push(name.clone());
                                if name == scope_name {
                                    if let Some(body) = find_child_by_kind(&child, "block") {
                                        let children = extract_body_children_py(&body, source);
                                        return Some((
                                            body.start_byte(),
                                            body.end_byte(),
                                            children,
                                        ));
                                    }
                                }
                            }
                        }
                        // Also handle decorated_definition wrapping class_definition
                        if child.kind() == "decorated_definition" {
                            if let Some(result) =
                                walk_py_classes(&child, source, scope_name, available)
                            {
                                return Some(result);
                            }
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                None
            }
            if let Some(info) = walk_py_classes(root, source, scope_name, &mut available) {
                return (Some(info), available);
            }
        }
        LangId::Rust => {
            // Walk for impl_item first (more common target for add_member),
            // then struct_item. This means `impl Config` is found before `struct Config`.
            let mut struct_match: Option<BodyInfo> = None;
            let mut cursor = root.walk();
            if cursor.goto_first_child() {
                loop {
                    let node = cursor.node();
                    if node.kind() == "impl_item" {
                        let impl_name = extract_impl_name(&node, source);
                        if let Some(ref name) = impl_name {
                            if !available.contains(name) {
                                available.push(name.clone());
                            }
                            if name == scope_name {
                                if let Some(body) = find_child_by_kind(&node, "declaration_list") {
                                    let children = extract_body_children_rs_impl(&body, source);
                                    return (
                                        Some((body.start_byte(), body.end_byte(), children)),
                                        available,
                                    );
                                }
                            }
                        }
                    }
                    if node.kind() == "struct_item" {
                        if let Some(name) = find_child_text(&node, source, &["type_identifier"]) {
                            if !available.contains(&name) {
                                available.push(name.clone());
                            }
                            if name == scope_name && struct_match.is_none() {
                                if let Some(body) =
                                    find_child_by_kind(&node, "field_declaration_list")
                                {
                                    let children = extract_body_children_rs_struct(&body, source);
                                    struct_match =
                                        Some((body.start_byte(), body.end_byte(), children));
                                }
                            }
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            // If no impl matched but a struct did, use the struct
            if let Some(info) = struct_match {
                return (Some(info), available);
            }
        }
        LangId::Go => {
            // Walk for type_declaration → type_spec → struct_type
            let mut cursor = root.walk();
            if cursor.goto_first_child() {
                loop {
                    let node = cursor.node();
                    if node.kind() == "type_declaration" {
                        if let Some(info) =
                            find_go_struct(&node, source, scope_name, &mut available)
                        {
                            return (Some(info), available);
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl
        | LangId::Html
        | LangId::Markdown
        | LangId::Yaml => {}
    }

    (None, available)
}

/// Extract the name for a Rust impl block.
/// `impl Foo { ... }` → "Foo"
/// `impl Trait for Foo { ... }` → "Foo" (match on the target type)
fn extract_impl_name(node: &Node, source: &str) -> Option<String> {
    let mut type_names: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "type_identifier" {
                type_names.push(node_text(source, &child).to_string());
            } else if child.kind() == "generic_type" {
                // `impl<T> Config<T>` parses the implemented type as a
                // `generic_type` whose full text is "Config<T>". Descend to its
                // base `type_identifier` ("Config") so the impl matches a scope
                // requested as "Config" — otherwise the bare-name struct (whose
                // type_identifier is "Config") wins the fallback and the method
                // gets inserted into the struct's field list (invalid Rust).
                let base = find_child_by_kind(&child, "type_identifier")
                    .map(|id| node_text(source, &id).to_string())
                    .unwrap_or_else(|| node_text(source, &child).to_string());
                type_names.push(base);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // For `impl Foo`, return "Foo"
    // For `impl Trait for Foo`, return "Foo" (last type name)
    type_names.last().cloned()
}

/// Find a Go struct within a type_declaration node.
fn find_go_struct(
    type_decl: &Node,
    source: &str,
    scope_name: &str,
    available: &mut Vec<String>,
) -> Option<BodyInfo> {
    let mut cursor = type_decl.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "type_spec" {
                // type_spec has name (type_identifier) and type (struct_type)
                if let Some(name_node) = find_child_by_kind(&child, "type_identifier") {
                    let name = node_text(source, &name_node).to_string();
                    available.push(name.clone());
                    if name == scope_name {
                        if let Some(struct_type) = find_child_by_kind(&child, "struct_type") {
                            if let Some(body) =
                                find_child_by_kind(&struct_type, "field_declaration_list")
                            {
                                let children = extract_body_children_go(&body, source);
                                return Some((body.start_byte(), body.end_byte(), children));
                            }
                        }
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

// --- Body child extraction per language ---

fn extract_body_children_ts(body: &Node, source: &str) -> Vec<BodyChild> {
    let mut children = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            // Skip delimiters
            if child.kind() == "{" || child.kind() == "}" || child.kind() == ";" {
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            let name = extract_member_name_ts(&child, source);
            children.push(BodyChild {
                name,
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
            });
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

fn extract_member_name_ts(node: &Node, source: &str) -> String {
    // method_definition → name is property_identifier
    // public_field_definition → name is property_identifier
    if let Some(name_node) = node.child_by_field_name("name") {
        return node_text(source, &name_node).to_string();
    }
    // Fallback: first named child
    if node.named_child_count() > 0 {
        if let Some(child) = node.named_child(0) {
            return node_text(source, &child).to_string();
        }
    }
    String::new()
}

fn extract_body_children_py(body: &Node, source: &str) -> Vec<BodyChild> {
    let mut children = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let name = extract_member_name_py(&child, source);
            if !name.is_empty() {
                children.push(BodyChild {
                    name,
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                });
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

fn extract_member_name_py(node: &Node, source: &str) -> String {
    match node.kind() {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                return node_text(source, &name_node).to_string();
            }
        }
        "decorated_definition" => {
            // Find the inner function_definition
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "function_definition" || child.kind() == "class_definition" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            return node_text(source, &name_node).to_string();
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        "expression_statement" => {
            // Assignment like `x = 1` — name is the left side
            if let Some(assign) = node.named_child(0) {
                if assign.kind() == "assignment" {
                    if let Some(left) = assign.child_by_field_name("left") {
                        return node_text(source, &left).to_string();
                    }
                }
            }
        }
        _ => {}
    }
    String::new()
}

fn extract_body_children_rs_struct(body: &Node, source: &str) -> Vec<BodyChild> {
    let mut children = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "field_declaration" {
                let name = if let Some(name_node) = child.child_by_field_name("name") {
                    node_text(source, &name_node).to_string()
                } else {
                    String::new()
                };
                children.push(BodyChild {
                    name,
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                });
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

fn extract_body_children_rs_impl(body: &Node, source: &str) -> Vec<BodyChild> {
    let mut children = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "function_item" {
                let name = if let Some(name_node) = child.child_by_field_name("name") {
                    node_text(source, &name_node).to_string()
                } else {
                    String::new()
                };
                children.push(BodyChild {
                    name,
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                });
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

fn extract_body_children_go(body: &Node, source: &str) -> Vec<BodyChild> {
    let mut children = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "field_declaration" {
                // Go field names are field_identifier
                let name = if let Some(name_node) = find_child_by_kind(&child, "field_identifier") {
                    node_text(source, &name_node).to_string()
                } else {
                    String::new()
                };
                children.push(BodyChild {
                    name,
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                });
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children
}

// --- Helpers ---

/// Find a direct child node of a given kind.
fn find_child_by_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Find the text of the first child matching one of the given kinds.
fn find_child_text(node: &Node, source: &str, kinds: &[&str]) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if kinds.contains(&child.kind()) {
                return Some(node_text(source, &child).to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Detect the indentation used by existing body children.
/// If the body is empty, return one level of the file's indent style.
fn detect_member_indent(
    source: &str,
    children: &[BodyChild],
    file_indent: &IndentStyle,
    _lang: LangId,
) -> String {
    if let Some(first_child) = children.first() {
        // Extract the leading whitespace of the first child's line
        let line_start = source[..first_child.start_byte]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let line = &source[line_start..first_child.start_byte];
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if !indent.is_empty() {
            return indent;
        }
    }
    // Empty body — use one level of file indent
    file_indent.as_str().to_string()
}

/// Resolve the byte offset for the insertion position.
fn resolve_position(
    source: &str,
    position: &str,
    body_start: usize,
    body_end: usize,
    children: &[BodyChild],
    lang: LangId,
    scope_name: &str,
    file: &str,
) -> Result<usize, AftError> {
    match position {
        "first" => Ok(resolve_first(source, body_start, lang)),
        "last" => Ok(resolve_last(source, body_end, children, lang)),
        pos if pos.starts_with("before:") => {
            let member_name = &pos["before:".len()..];
            let child = children.iter().find(|c| c.name == member_name);
            match child {
                Some(c) => {
                    // Insert before this child's line
                    let line_start = source[..c.start_byte]
                        .rfind('\n')
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    Ok(line_start)
                }
                None => Err(AftError::MemberNotFound {
                    member: member_name.to_string(),
                    scope: scope_name.to_string(),
                    file: file.to_string(),
                }),
            }
        }
        pos if pos.starts_with("after:") => {
            let member_name = &pos["after:".len()..];
            let child = children.iter().find(|c| c.name == member_name);
            match child {
                Some(c) => {
                    // Insert after this child's end, on a new line
                    // Find the end of the line containing the child's end
                    let line_end = source[c.end_byte..]
                        .find('\n')
                        .map(|p| c.end_byte + p + 1)
                        .unwrap_or(c.end_byte);
                    Ok(line_end)
                }
                None => Err(AftError::MemberNotFound {
                    member: member_name.to_string(),
                    scope: scope_name.to_string(),
                    file: file.to_string(),
                }),
            }
        }
        _ => Err(AftError::InvalidRequest {
            message: format!(
                "add_member: invalid position '{}', expected first|last|before:name|after:name",
                position
            ),
        }),
    }
}

/// Resolve "first" position: right after the opening delimiter.
fn resolve_first(source: &str, body_start: usize, lang: LangId) -> usize {
    match lang {
        LangId::Python => {
            // Python: body starts right after the colon + newline
            // body_start is the start of the block node, which is the first
            // content line. We insert at body_start.
            body_start
        }
        _ => {
            // Brace-delimited: find the opening `{` and insert after the newline following it
            if let Some(brace_pos) = source[body_start..].find('{') {
                let after_brace = body_start + brace_pos + 1;
                // Find the newline after the brace
                if let Some(nl_pos) = source[after_brace..].find('\n') {
                    after_brace + nl_pos + 1
                } else {
                    after_brace
                }
            } else {
                body_start
            }
        }
    }
}

/// Resolve "last" position: before the closing delimiter.
fn resolve_last(source: &str, body_end: usize, children: &[BodyChild], lang: LangId) -> usize {
    match lang {
        LangId::Python => {
            // Python: insert after the last child's line
            if let Some(last) = children.last() {
                // Find end of the last child's line
                source[last.end_byte..]
                    .find('\n')
                    .map(|p| last.end_byte + p + 1)
                    .unwrap_or(last.end_byte)
            } else {
                // Empty body — insert at body_start
                body_start_for_empty_py(source, body_end)
            }
        }
        _ => {
            // Brace-delimited: insert before the closing `}`
            // Find the `}` searching backward from body_end
            let closing = source[..body_end].rfind('}').unwrap_or(body_end);
            // Insert at the start of the line containing `}`
            let line_start = source[..closing]
                .rfind('\n')
                .map(|p| p + 1)
                .unwrap_or(closing);
            line_start
        }
    }
}

/// For an empty Python class body, find the insertion point.
fn body_start_for_empty_py(_source: &str, body_end: usize) -> usize {
    // The body_end is the end of the block. For empty Python blocks
    // (which have `pass` or nothing), we insert at body start.
    // Walk backward to find the start of the block.
    body_end
}

/// Indent the provided code to match the target indentation.
fn indent_code(
    code: &str,
    member_indent: &str,
    _source: &str,
    _insert_offset: usize,
    _position: &str,
    _body_start: usize,
    _body_end: usize,
    children: &[BodyChild],
    lang: LangId,
) -> String {
    let mut result = String::new();
    let lines: Vec<&str> = code.lines().collect();
    let is_empty = children.is_empty();

    // For empty brace-delimited containers at "last" position,
    // we're inserting at the line of `}` — no special leading newline needed.
    let _needs_trailing_newline = is_empty && !matches!(lang, LangId::Python);

    for line in &lines {
        if line.trim().is_empty() {
            result.push('\n');
        } else {
            result.push_str(member_indent);
            result.push_str(line.trim_start());
            result.push('\n');
        }
    }

    // Ensure the code doesn't end with a double newline if inserting at "last"
    // but ensure there's always a trailing newline
    if result.is_empty() {
        result.push('\n');
    }

    result
}
