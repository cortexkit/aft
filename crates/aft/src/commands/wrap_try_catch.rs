//! Handler for the `wrap_try_catch` command: wrap a TS/JS function or method
//! body in a try/catch block, preserving indentation.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::context::AppContext;
use crate::edit;
use crate::indent::detect_indent;
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle a `wrap_try_catch` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `target` (string, required) — function or method name
///   - `catch_body` (string, optional) — code inside catch block (default: `throw error;`)
///
/// Returns: `{ file, target, syntax_valid?, backup_id? }`
pub fn handle_wrap_try_catch(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "wrap_try_catch: missing required param 'file'",
            );
        }
    };

    let target = match req.params.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "wrap_try_catch: missing required param 'target'",
            );
        }
    };

    let catch_body = req
        .params
        .get("catch_body")
        .and_then(|v| v.as_str())
        .unwrap_or("throw error;");

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("wrap_try_catch: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "wrap_try_catch: unsupported file type",
            );
        }
    };

    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        return Response::error(
            &req.id,
            "invalid_request",
            "wrap_try_catch: only TypeScript/JavaScript files are supported",
        );
    }

    // --- Parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("wrap_try_catch: cannot read file: {}", e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&grammar) {
        return Response::error(
            &req.id,
            "parse_error",
            format!("wrap_try_catch: grammar init failed: {}", e),
        );
    }

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!("wrap_try_catch: parse failed for {}", file),
            );
        }
    };

    let root = tree.root_node();

    // --- Find target function/method ---
    let target_info = match find_function(&root, &source, target) {
        Ok(info) => info,
        Err(msg) => {
            return Response::error(&req.id, "target_not_found", msg);
        }
    };

    // --- Build wrapped body ---
    let indent = detect_indent(&source, lang);
    let indent_str = indent.as_str();

    let body_node = target_info.body_node_range;
    let body_content = &source[body_node.0..body_node.1];

    // Extract lines between the opening { and closing }
    let inner = extract_block_inner(body_content);

    // Determine the base indentation of the function body
    let fn_line_start = source[..target_info.fn_start]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let fn_indent: String = source[fn_line_start..target_info.fn_start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();

    // Re-indent inner lines: add one extra level inside the try block
    let try_indent = format!("{}{}", fn_indent, indent_str);
    let body_indent = format!("{}{}{}", fn_indent, indent_str, indent_str);

    let mut reindented_lines = Vec::new();
    for line in inner.lines() {
        if line.trim().is_empty() {
            reindented_lines.push(String::new());
        } else {
            // Strip existing indentation and apply new
            let stripped = line.trim_start();
            reindented_lines.push(format!("{}{}", body_indent, stripped));
        }
    }

    let reindented_body = reindented_lines.join("\n");

    // Build the wrapped body
    let catch_lines: Vec<String> = catch_body
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", body_indent, l.trim_start())
            }
        })
        .collect();
    let catch_content = catch_lines.join("\n");

    let wrapped = format!(
        "{{\n{try_indent}try {{\n{reindented_body}\n{try_indent}}} catch (error) {{\n{catch_content}\n{try_indent}}}\n{fn_indent}}}",
        try_indent = try_indent,
        reindented_body = reindented_body,
        catch_content = catch_content,
        fn_indent = fn_indent,
    );

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "wrap_try_catch: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Replace ---
    let new_source = match edit::replace_byte_range(&source, body_node.0, body_node.1, &wrapped) {
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

    log::debug!("wrap_try_catch: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "target": target,
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

struct FunctionInfo {
    fn_start: usize,
    /// (start_byte, end_byte) of the statement_block
    body_node_range: (usize, usize),
}

/// Find a function_declaration or method_definition by name.
/// Walks the full tree recursively.
fn find_function(root: &Node, source: &str, target_name: &str) -> Result<FunctionInfo, String> {
    let mut available: Vec<String> = Vec::new();

    if let Some(info) = walk_for_function(root, source, target_name, &mut available) {
        return Ok(info);
    }

    if available.is_empty() {
        Err(format!(
            "wrap_try_catch: target '{}' not found (no functions/methods found)",
            target_name
        ))
    } else {
        Err(format!(
            "wrap_try_catch: target '{}' not found, available: [{}]",
            target_name,
            available.join(", ")
        ))
    }
}

fn walk_for_function(
    node: &Node,
    source: &str,
    target_name: &str,
    available: &mut Vec<String>,
) -> Option<FunctionInfo> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "function_declaration" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(source, &name_node);
                        available.push(name.to_string());
                        if name == target_name {
                            if let Some(body) = child.child_by_field_name("body") {
                                if body.kind() == "statement_block" {
                                    return Some(FunctionInfo {
                                        fn_start: child.start_byte(),
                                        body_node_range: (body.start_byte(), body.end_byte()),
                                    });
                                }
                            }
                        }
                    }
                }
                "method_definition" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(source, &name_node);
                        available.push(name.to_string());
                        if name == target_name {
                            if let Some(body) = child.child_by_field_name("body") {
                                if body.kind() == "statement_block" {
                                    return Some(FunctionInfo {
                                        fn_start: child.start_byte(),
                                        body_node_range: (body.start_byte(), body.end_byte()),
                                    });
                                }
                            }
                        }
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    // Check for arrow functions: const foo = () => { ... }
                    let mut inner_cursor = child.walk();
                    if inner_cursor.goto_first_child() {
                        loop {
                            let decl = inner_cursor.node();
                            if decl.kind() == "variable_declarator" {
                                if let Some(name_node) = decl.child_by_field_name("name") {
                                    let name = node_text(source, &name_node);
                                    if let Some(val) = decl.child_by_field_name("value") {
                                        if val.kind() == "arrow_function" {
                                            available.push(name.to_string());
                                            if name == target_name {
                                                // Check if arrow has a statement_block body
                                                if let Some(body) = val.child_by_field_name("body")
                                                {
                                                    if body.kind() == "statement_block" {
                                                        return Some(FunctionInfo {
                                                            fn_start: child.start_byte(),
                                                            body_node_range: (
                                                                body.start_byte(),
                                                                body.end_byte(),
                                                            ),
                                                        });
                                                    } else {
                                                        // Arrow without braces — error
                                                        return None;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if !inner_cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
                _ => {
                    // Recurse into class bodies, export statements, etc.
                    if let Some(info) = walk_for_function(&child, source, target_name, available) {
                        return Some(info);
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

/// Extract the inner content of a `{ ... }` block (between opening and closing braces).
fn extract_block_inner(block_text: &str) -> &str {
    let start = block_text.find('{').map(|p| p + 1).unwrap_or(0);
    let end = block_text.rfind('}').unwrap_or(block_text.len());
    let inner = &block_text[start..end];
    // Trim leading newline if present
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    // Trim trailing newline if present
    let inner = inner.strip_suffix('\n').unwrap_or(inner);
    inner
}
