//! Handler for the `add_derive` command: add derive macros to Rust
//! structs and enums, appending to existing `#[derive(...)]` or creating new ones.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::context::AppContext;
use crate::edit;
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `add_derive` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `target` (string, required) — struct or enum name
///   - `derives` (array of strings, required) — derive names to add
///
/// Returns: `{ file, target, derives, syntax_valid?, backup_id? }`
pub fn handle_add_derive(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_derive: missing required param 'file'",
            );
        }
    };

    let target = match req.params.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_derive: missing required param 'target'",
            );
        }
    };

    let derives: Vec<String> = match req.params.get("derives").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_derive: missing required param 'derives' (array of strings)",
            );
        }
    };

    if derives.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "add_derive: 'derives' array must not be empty",
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
            format!("add_derive: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_derive: only Rust files are supported",
            );
        }
    };

    if !matches!(lang, LangId::Rust) {
        return Response::error(
            &req.id,
            "invalid_request",
            "add_derive: only Rust files are supported",
        );
    }

    // --- Parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("add_derive: cannot read file: {}", e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&grammar) {
        return Response::error(
            &req.id,
            "parse_error",
            format!("add_derive: grammar init failed: {}", e),
        );
    }

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!("add_derive: parse failed for {}", file),
            );
        }
    };

    let root = tree.root_node();

    // --- Find target struct/enum ---
    let target_info = match find_target(&root, &source, target) {
        (Some(info), _) => info,
        (None, avail) => {
            let msg = if avail.is_empty() {
                format!(
                    "add_derive: target '{}' not found in {} (no structs/enums found)",
                    target, file
                )
            } else {
                format!(
                    "add_derive: target '{}' not found in {}, available: [{}]",
                    target,
                    file,
                    avail.join(", ")
                )
            };
            return Response::error(&req.id, "target_not_found", msg);
        }
    };

    // --- Find existing derive attribute ---
    let (new_source, final_derives) = apply_derive(&source, &root, target_info, &derives);

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_derive: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
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

    log::debug!("add_derive: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "target": target,
        "derives": final_derives,
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

/// Target info: (node_start_byte, node that is the struct/enum)
struct TargetInfo {
    /// Byte offset where the target item starts (for inserting attribute before it).
    start_byte: usize,
}

/// Find a `struct_item` or `enum_item` by name.
fn find_target<'a>(
    root: &Node<'a>,
    source: &str,
    target_name: &str,
) -> (Option<TargetInfo>, Vec<String>) {
    let mut available: Vec<String> = Vec::new();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "struct_item" || node.kind() == "enum_item" {
                if let Some(name) = child_text_by_kind(&node, source, "type_identifier") {
                    available.push(name.clone());
                    if name == target_name {
                        return (
                            Some(TargetInfo {
                                start_byte: node.start_byte(),
                            }),
                            available,
                        );
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    (None, available)
}

fn child_text_by_kind<'a>(node: &Node<'a>, source: &str, kind: &str) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(node_text(source, &child).to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Apply derive changes to the source.
///
/// Searches for an existing `#[derive(...)]` attribute right before the target.
/// If found, appends new derives (dedup). If not found, inserts a new attribute.
///
/// Returns (new_source, final_derives_list).
fn apply_derive(
    source: &str,
    root: &Node,
    target: TargetInfo,
    new_derives: &[String],
) -> (String, Vec<String>) {
    // Walk backwards from the target to find attribute_item siblings.
    // Attributes are siblings of the struct/enum under the root (or a module).
    let mut derive_attr: Option<DeriveAttr> = None;

    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        let mut prev_attrs: Vec<(usize, usize, String)> = Vec::new(); // (start, end, text)
        loop {
            let node = cursor.node();
            if node.kind() == "attribute_item" {
                let text = node_text(source, &node);
                prev_attrs.push((node.start_byte(), node.end_byte(), text.to_string()));
            } else {
                if node.start_byte() == target.start_byte {
                    // Found our target — check preceding attributes
                    for (start, end, text) in prev_attrs.iter().rev() {
                        if let Some(existing) = parse_derive_attr(text) {
                            derive_attr = Some(DeriveAttr {
                                start_byte: *start,
                                end_byte: *end,
                                existing_derives: existing,
                            });
                            break;
                        }
                    }
                    break;
                }
                prev_attrs.clear();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    match derive_attr {
        Some(attr) => {
            // Merge new derives into existing, maintaining order, dedup
            let mut merged = attr.existing_derives.clone();
            for d in new_derives {
                if !merged.iter().any(|e| e == d) {
                    merged.push(d.clone());
                }
            }
            let new_attr = format!("#[derive({})]", merged.join(", "));
            let new_source =
                match edit::replace_byte_range(source, attr.start_byte, attr.end_byte, &new_attr) {
                    Ok(s) => s,
                    Err(_) => source.to_string(),
                };
            (new_source, merged)
        }
        None => {
            // No existing derive — insert new attribute before the target
            // Find the start of the line containing the target
            let line_start = source[..target.start_byte]
                .rfind('\n')
                .map(|p| p + 1)
                .unwrap_or(0);
            let indent: String = source[line_start..target.start_byte]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            let derive_line = format!(
                "{}#[derive({})]\n",
                indent,
                new_derives
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let new_source =
                match edit::replace_byte_range(source, line_start, line_start, &derive_line) {
                    Ok(s) => s,
                    Err(_) => source.to_string(),
                };
            (new_source, new_derives.to_vec())
        }
    }
}

struct DeriveAttr {
    start_byte: usize,
    end_byte: usize,
    existing_derives: Vec<String>,
}

/// Parse a `#[derive(Debug, Clone)]` attribute text into the list of derive names.
fn parse_derive_attr(text: &str) -> Option<Vec<String>> {
    let trimmed = text.trim();
    // Must match pattern: #[derive(...)]
    if !trimmed.starts_with("#[derive(") || !trimmed.ends_with(")]") {
        return None;
    }
    let inner = &trimmed["#[derive(".len()..trimmed.len() - 2];
    let names: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_derive_attr_basic() {
        let result = parse_derive_attr("#[derive(Debug, Clone)]");
        assert_eq!(result, Some(vec!["Debug".to_string(), "Clone".to_string()]));
    }

    #[test]
    fn parse_derive_attr_single() {
        let result = parse_derive_attr("#[derive(Debug)]");
        assert_eq!(result, Some(vec!["Debug".to_string()]));
    }

    #[test]
    fn parse_derive_attr_not_derive() {
        let result = parse_derive_attr("#[cfg(test)]");
        assert_eq!(result, None);
    }
}
