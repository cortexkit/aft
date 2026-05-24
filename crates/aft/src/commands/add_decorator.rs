//! Handler for the `add_decorator` command: insert Python decorators onto
//! functions or classes, handling both plain and already-decorated definitions.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::context::AppContext;
use crate::edit;
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `add_decorator` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `target` (string, required) — function or class name
///   - `decorator` (string, required) — decorator text without `@`
///   - `position` (string, optional) — `first` or `last` among existing decorators (default: `first`)
///
/// Returns: `{ file, target, decorator, syntax_valid?, backup_id? }`
pub fn handle_add_decorator(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_decorator: missing required param 'file'",
            );
        }
    };

    let target = match req.params.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_decorator: missing required param 'target'",
            );
        }
    };

    let decorator = match req.params.get("decorator").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_decorator: missing required param 'decorator'",
            );
        }
    };

    let position = req
        .params
        .get("position")
        .and_then(|v| v.as_str())
        .unwrap_or("first");

    if position != "first" && position != "last" {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "add_decorator: invalid position '{}', expected 'first' or 'last'",
                position
            ),
        );
    }

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("add_decorator: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_decorator: unsupported file type",
            );
        }
    };

    if !matches!(lang, LangId::Python) {
        return Response::error(
            &req.id,
            "invalid_request",
            "add_decorator: only Python files are supported",
        );
    }

    // --- Parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("add_decorator: cannot read file: {}", e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&grammar) {
        return Response::error(
            &req.id,
            "parse_error",
            format!("add_decorator: grammar init failed: {}", e),
        );
    }

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!("add_decorator: parse failed for {}", file),
            );
        }
    };

    let root = tree.root_node();

    // --- Find target ---
    let target_info = match find_decorator_target(&root, &source, target) {
        Ok(info) => info,
        Err(msg) => {
            return Response::error(&req.id, "target_not_found", msg);
        }
    };

    // --- Build insertion ---
    let decorator_line = format!("{}@{}\n", target_info.indent, decorator);

    let insert_offset = match position {
        "first" => target_info.first_decorator_start,
        "last" => target_info.last_decorator_end,
        _ => target_info.first_decorator_start,
    };

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_decorator: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Insert ---
    let new_source =
        match edit::replace_byte_range(&source, insert_offset, insert_offset, &decorator_line) {
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

    log::debug!("add_decorator: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "target": target,
        "decorator": decorator,
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

struct DecoratorTarget {
    /// The indentation of the function/class definition line.
    indent: String,
    /// Byte offset of the start of the first decorator line (or the def line if no decorators).
    first_decorator_start: usize,
    /// Byte offset just after the last decorator line (before the def line).
    /// For "last" position, we insert here (just before the def/class keyword line).
    last_decorator_end: usize,
}

/// Find a function_definition or class_definition by name.
/// Handles both plain and decorated definitions.
fn find_decorator_target(
    root: &Node,
    source: &str,
    target_name: &str,
) -> Result<DecoratorTarget, String> {
    let mut available: Vec<String> = Vec::new();

    if let Some(info) = walk_for_target(root, source, target_name, &mut available) {
        return Ok(info);
    }

    if available.is_empty() {
        Err(format!(
            "add_decorator: target '{}' not found (no functions/classes found)",
            target_name
        ))
    } else {
        Err(format!(
            "add_decorator: target '{}' not found, available: [{}]",
            target_name,
            available.join(", ")
        ))
    }
}

fn walk_for_target(
    node: &Node,
    source: &str,
    target_name: &str,
    available: &mut Vec<String>,
) -> Option<DecoratorTarget> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "function_definition" | "class_definition" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(source, &name_node);
                        available.push(name.to_string());
                        if name == target_name {
                            // Plain definition — no decorators
                            let indent = extract_indent(source, child.start_byte());
                            let line_start = line_start_byte(source, child.start_byte());
                            return Some(DecoratorTarget {
                                indent,
                                first_decorator_start: line_start,
                                last_decorator_end: line_start,
                            });
                        }
                    }
                    // Also recurse into class/function bodies to find nested targets
                    if let Some(info) = walk_for_target(&child, source, target_name, available) {
                        return Some(info);
                    }
                }
                "decorated_definition" => {
                    // Find the inner function_definition or class_definition
                    let inner_def = find_inner_def(&child);
                    if let Some(inner) = inner_def {
                        if let Some(name_node) = inner.child_by_field_name("name") {
                            let name = node_text(source, &name_node);
                            available.push(name.to_string());
                            if name == target_name {
                                // Has existing decorators
                                let indent = extract_indent(source, inner.start_byte());
                                let dec_start = line_start_byte(source, child.start_byte());
                                let def_line_start = line_start_byte(source, inner.start_byte());
                                return Some(DecoratorTarget {
                                    indent,
                                    first_decorator_start: dec_start,
                                    last_decorator_end: def_line_start,
                                });
                            }
                        }
                    }
                    // Also recurse into the decorated definition to find nested targets
                    if let Some(info) = walk_for_target(&child, source, target_name, available) {
                        return Some(info);
                    }
                }
                _ => {
                    // Recurse into blocks, if-statements, etc.
                    if let Some(info) = walk_for_target(&child, source, target_name, available) {
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

/// Find the inner function_definition or class_definition inside a decorated_definition.
fn find_inner_def<'a>(decorated: &Node<'a>) -> Option<Node<'a>> {
    let mut cursor = decorated.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "function_definition" || child.kind() == "class_definition" {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract the indentation of the line containing the given byte offset.
fn extract_indent(source: &str, byte_offset: usize) -> String {
    let line_start = line_start_byte(source, byte_offset);
    source[line_start..byte_offset]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect()
}

/// Find the byte offset of the start of the line containing `byte_offset`.
fn line_start_byte(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0)
}
