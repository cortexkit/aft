//! Handler for the `add_struct_tags` command: add or update Go struct field
//! tags (backtick-delimited key:"value" pairs).

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::context::AppContext;
use crate::edit;
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `add_struct_tags` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `target` (string, required) — struct name
///   - `field` (string, required) — field name
///   - `tag` (string, required) — tag key (e.g. `"json"`)
///   - `value` (string, required) — tag value (e.g. `"user_name,omitempty"`)
///
/// Returns: `{ file, target, field, tag_string, syntax_valid?, backup_id? }`
pub fn handle_add_struct_tags(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: missing required param 'file'",
            );
        }
    };

    let target = match req.params.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: missing required param 'target'",
            );
        }
    };

    let field_name = match req.params.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: missing required param 'field'",
            );
        }
    };

    let tag_key = match req.params.get("tag").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: missing required param 'tag'",
            );
        }
    };

    let tag_value = match req.params.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: missing required param 'value'",
            );
        }
    };

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("add_struct_tags: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_struct_tags: unsupported file type",
            );
        }
    };

    if !matches!(lang, LangId::Go) {
        return Response::error(
            &req.id,
            "invalid_request",
            "add_struct_tags: only Go files are supported",
        );
    }

    // --- Parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("add_struct_tags: cannot read file: {}", e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&grammar) {
        return Response::error(
            &req.id,
            "parse_error",
            format!("add_struct_tags: grammar init failed: {}", e),
        );
    }

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!("add_struct_tags: parse failed for {}", file),
            );
        }
    };

    let root = tree.root_node();

    // --- Find target struct and field ---
    let field_info = match find_struct_field(&root, &source, target, field_name, file) {
        Ok(info) => info,
        Err(resp) => return resp,
    };

    // --- Modify tags ---
    let (new_source, final_tag_string) = match field_info.existing_tag {
        Some(tag_info) => {
            // Parse existing tag, add/update key
            let existing_text = &source[tag_info.start..tag_info.end];
            // Strip backticks
            let inner = &existing_text[1..existing_text.len() - 1];
            let mut tags = parse_struct_tags(inner);
            // Add or update
            let mut found = false;
            for t in &mut tags {
                if t.0 == tag_key {
                    t.1 = tag_value.to_string();
                    found = true;
                    break;
                }
            }
            if !found {
                tags.push((tag_key.to_string(), tag_value.to_string()));
            }
            let new_tag_str = format!("`{}`", format_struct_tags(&tags));
            let new_source =
                match edit::replace_byte_range(&source, tag_info.start, tag_info.end, &new_tag_str)
                {
                    Ok(s) => s,
                    Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
                };
            (new_source, new_tag_str)
        }
        None => {
            // No existing tag — insert after the type, before end of field line
            let tag_str = format!("`{}:\"{}\"`", tag_key, tag_value);
            // Insert a space + tag before the end of the field_declaration
            // We insert right before the newline at end of field, after the type
            let insert_pos = field_info.type_end;
            let insert_text = format!(" {}", tag_str);
            let new_source =
                match edit::replace_byte_range(&source, insert_pos, insert_pos, &insert_text) {
                    Ok(s) => s,
                    Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
                };
            (new_source, tag_str)
        }
    };

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_struct_tags: pre-edit backup",
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

    log::debug!("add_struct_tags: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "target": target,
        "field": field_name,
        "tag_string": final_tag_string,
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

struct FieldInfo {
    /// Byte offset right after the type (where we'd insert a new tag).
    type_end: usize,
    /// Existing tag literal, if present.
    existing_tag: Option<TagRange>,
}

struct TagRange {
    start: usize,
    end: usize,
}

/// Find a struct's field_declaration by struct and field name.
fn find_struct_field(
    root: &Node,
    source: &str,
    struct_name: &str,
    field_name: &str,
    file: &str,
) -> Result<FieldInfo, Response> {
    let mut available_structs: Vec<String> = Vec::new();

    // Find the struct
    let struct_body = find_go_struct_body(root, source, struct_name, &mut available_structs);
    let body = match struct_body {
        Some(b) => b,
        None => {
            let msg = if available_structs.is_empty() {
                format!(
                    "add_struct_tags: struct '{}' not found in {} (no structs found)",
                    struct_name, file
                )
            } else {
                format!(
                    "add_struct_tags: struct '{}' not found in {}, available: [{}]",
                    struct_name,
                    file,
                    available_structs.join(", ")
                )
            };
            return Err(Response::error("", "target_not_found", msg));
        }
    };

    // Find the field within the struct body
    let mut available_fields: Vec<String> = Vec::new();
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "field_declaration" {
                if let Some(fi_node) = find_child_by_kind_node(&child, "field_identifier") {
                    let fname = node_text(source, &fi_node);
                    available_fields.push(fname.to_string());
                    if fname == field_name {
                        // Found the field — find existing tag and type end
                        let tag = find_child_by_kind_node(&child, "raw_string_literal").map(|n| {
                            TagRange {
                                start: n.start_byte(),
                                end: n.end_byte(),
                            }
                        });

                        // type_end: the end of the last non-tag child
                        let type_end = find_type_end(&child, source);

                        return Ok(FieldInfo {
                            type_end,
                            existing_tag: tag,
                        });
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let msg = if available_fields.is_empty() {
        format!(
            "add_struct_tags: field '{}' not found in struct '{}' (no fields found)",
            field_name, struct_name
        )
    } else {
        format!(
            "add_struct_tags: field '{}' not found in struct '{}', available: [{}]",
            field_name,
            struct_name,
            available_fields.join(", ")
        )
    };
    Err(Response::error("", "field_not_found", msg))
}

/// Find a Go struct's field_declaration_list node by name.
fn find_go_struct_body<'a>(
    root: &Node<'a>,
    source: &str,
    struct_name: &str,
    available: &mut Vec<String>,
) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "type_declaration" {
                let mut inner = node.walk();
                if inner.goto_first_child() {
                    loop {
                        let child = inner.node();
                        if child.kind() == "type_spec" {
                            if let Some(name_node) =
                                find_child_by_kind_node(&child, "type_identifier")
                            {
                                let name = node_text(source, &name_node);
                                available.push(name.to_string());
                                if name == struct_name {
                                    if let Some(st) = find_child_by_kind_node(&child, "struct_type")
                                    {
                                        if let Some(body) =
                                            find_child_by_kind_node(&st, "field_declaration_list")
                                        {
                                            return Some(body);
                                        }
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
    None
}

fn find_child_by_kind_node<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
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

/// Find the byte offset right after the type in a field_declaration.
/// Skips field_identifier and type, stops before raw_string_literal if present.
fn find_type_end(field: &Node, _source: &str) -> usize {
    let mut last_non_tag_end = field.start_byte();
    let mut cursor = field.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() != "raw_string_literal" {
                let end = child.end_byte();
                if end > last_non_tag_end {
                    last_non_tag_end = end;
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    last_non_tag_end
}

/// Parse a struct tag string like `json:"name" xml:"value,omitempty"` into key-value pairs.
fn parse_struct_tags(tag_inner: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    let mut remaining = tag_inner.trim();

    while !remaining.is_empty() {
        // Find the key (up to the colon)
        let colon_pos = match remaining.find(':') {
            Some(p) => p,
            None => break,
        };
        let key = remaining[..colon_pos].trim().to_string();
        remaining = &remaining[colon_pos + 1..];

        // Value is quoted
        if !remaining.starts_with('"') {
            break;
        }
        remaining = &remaining[1..]; // skip opening quote

        // Find closing quote (handle escaped quotes)
        let mut value = String::new();
        let mut chars = remaining.chars();
        loop {
            match chars.next() {
                Some('\\') => {
                    if let Some(c) = chars.next() {
                        value.push('\\');
                        value.push(c);
                    }
                }
                Some('"') => break,
                Some(c) => value.push(c),
                None => break,
            }
        }
        remaining = chars.as_str().trim_start();
        tags.push((key, value));
    }

    tags
}

/// Format struct tags back into the `key:"value" key2:"value2"` format.
fn format_struct_tags(tags: &[(String, String)]) -> String {
    tags.iter()
        .map(|(k, v)| format!("{}:\"{}\"", k, v))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tags_basic() {
        let tags = parse_struct_tags(r#"json:"name""#);
        assert_eq!(tags, vec![("json".to_string(), "name".to_string())]);
    }

    #[test]
    fn parse_tags_multiple() {
        let tags = parse_struct_tags(r#"json:"name" xml:"user_name,omitempty""#);
        assert_eq!(
            tags,
            vec![
                ("json".to_string(), "name".to_string()),
                ("xml".to_string(), "user_name,omitempty".to_string()),
            ]
        );
    }

    #[test]
    fn format_tags_roundtrip() {
        let tags = vec![
            ("json".to_string(), "name".to_string()),
            ("xml".to_string(), "value".to_string()),
        ];
        assert_eq!(format_struct_tags(&tags), r#"json:"name" xml:"value""#);
    }
}
