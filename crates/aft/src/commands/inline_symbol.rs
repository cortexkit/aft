//! Handler for the `inline_symbol` command: replace a function call with
//! the function's body, performing argument-to-parameter substitution and
//! scope conflict detection.
//!
//! Follows the extract_function.rs pattern: validate → parse → compute →
//! auto_backup → write_format_validate → respond.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Parser;

use crate::context::AppContext;
use crate::edit;
use crate::extract::{detect_scope_conflicts, substitute_params, validate_single_return};
use crate::lsp_hints;
use crate::parser::{detect_language, grammar_for, node_text, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `inline_symbol` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `symbol` (string, required) — name of the function to inline
///   - `call_site_line` (u32, required) — line where the call expression is (1-based)
///
/// Returns on success:
///   `{ file, symbol, call_context, substitutions, conflicts, syntax_valid?, backup_id }`
///
/// `syntax_valid` is absent when syntax validation could not run.
///
/// Error codes:
///   - `unsupported_language` — file is not TS/JS/TSX/Python
///   - `multiple_returns` — function has >1 return statement (D102)
///   - `scope_conflict` — variable name collisions at call site (D103)
///   - `symbol_not_found` — function symbol not found in file
///   - `call_not_found` — no call expression found at the specified line
pub fn handle_inline_symbol(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "inline_symbol: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "inline_symbol: missing required param 'symbol'",
            );
        }
    };

    let call_site_line = match req.params.get("call_site_line").and_then(|v| v.as_u64()) {
        Some(l) if l >= 1 => (l - 1) as u32,
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "inline_symbol: 'call_site_line' must be >= 1 (1-based)",
            );
        }
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "inline_symbol: missing required param 'call_site_line'",
            );
        }
    };

    // --- Validate file ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("inline_symbol: file not found: {}", file),
        );
    }

    // --- Language guard (D101) ---
    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                "inline_symbol: unsupported file type",
            );
        }
    };

    if !matches!(
        lang,
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Python
    ) {
        return Response::error(
            &req.id,
            "unsupported_language",
            format!(
                "inline_symbol: only TypeScript/JavaScript/Python files are supported, got {:?}",
                lang
            ),
        );
    }

    // --- Read and parse ---
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("inline_symbol: {}: {}", file, e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Response::error(
            &req.id,
            "parse_error",
            "inline_symbol: failed to initialize parser",
        );
    }
    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                "inline_symbol: failed to parse file",
            );
        }
    };

    // --- Resolve function symbol ---
    let matches = match ctx.provider().resolve_symbol(&path, symbol) {
        Ok(m) => m,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // LSP-enhanced disambiguation (S03)
    let matches = if let Some(hints) = lsp_hints::parse_lsp_hints(req) {
        lsp_hints::apply_lsp_disambiguation(matches, &hints)
    } else {
        matches
    };

    // Find the first function/method match
    let sym = match matches.iter().find(|m| {
        matches!(
            m.symbol.kind,
            crate::symbols::SymbolKind::Function | crate::symbols::SymbolKind::Method
        )
    }) {
        Some(m) => &m.symbol,
        None => {
            return Response::error(
                &req.id,
                "symbol_not_found",
                format!("inline_symbol: no function '{}' found in {}", symbol, file),
            );
        }
    };

    // --- Find the function node in the AST ---
    let fn_start_byte = edit::line_col_to_byte(&source, sym.range.start_line, sym.range.start_col);
    let fn_node = match find_function_node_at(&tree.root_node(), fn_start_byte, lang) {
        Some(n) => n,
        None => {
            return Response::error(
                &req.id,
                "symbol_not_found",
                format!(
                    "inline_symbol: could not locate function node for '{}' in AST",
                    symbol
                ),
            );
        }
    };

    // --- Validate single-return (D102) ---
    if let Err(count) = validate_single_return(&source, &tree, &fn_node, lang) {
        return Response::error_with_data(
            &req.id,
            "multiple_returns",
            format!(
                "inline_symbol: function '{}' has {} return statements (max 1 for inlining)",
                symbol, count
            ),
            serde_json::json!({
                "return_count": count,
                "symbol": symbol,
            }),
        );
    }

    // --- Extract function body and parameters ---
    let (param_names, body_text) = extract_fn_params_and_body(&fn_node, &source, lang);

    // --- Find call expression at call_site_line ---
    let call_line_start = edit::line_col_to_byte(&source, call_site_line, 0);
    let call_line_end = if (call_site_line + 1) as usize <= source.lines().count() {
        edit::line_col_to_byte(&source, call_site_line + 1, 0)
    } else {
        source.len()
    };

    let call_node = match find_call_node_at_line(
        &tree.root_node(),
        symbol,
        &source,
        call_line_start,
        call_line_end,
        lang,
    ) {
        Some(n) => n,
        None => {
            return Response::error(
                &req.id,
                "call_not_found",
                format!(
                    "inline_symbol: no call to '{}' found at line {} in {}",
                    symbol, call_site_line, file
                ),
            );
        }
    };

    // --- Determine call context ---
    let (call_context, replacement_node, assignment_var) =
        detect_call_context(&call_node, &source, lang);

    // --- Build param→arg map ---
    let args = extract_call_arguments(&call_node, &source, lang);
    let mut param_to_arg: HashMap<String, String> = HashMap::new();
    for (i, param) in param_names.iter().enumerate() {
        if let Some(arg) = args.get(i) {
            param_to_arg.insert(param.clone(), arg.clone());
        }
    }

    // --- Check scope conflicts (D103) ---
    let conflicts = detect_scope_conflicts(
        &source,
        &tree,
        replacement_node.start_byte(),
        &param_names,
        &body_text,
        lang,
    );

    if !conflicts.is_empty() {
        let conflicting_names: Vec<&str> = conflicts.iter().map(|c| c.name.as_str()).collect();
        let suggestions: Vec<serde_json::Value> = conflicts
            .iter()
            .map(|c| {
                serde_json::json!({
                    "original": c.name,
                    "suggested": c.suggested,
                })
            })
            .collect();

        return Response::error_with_data(
            &req.id,
            "scope_conflict",
            format!(
                "inline_symbol: scope conflicts detected when inlining '{}': [{}]",
                symbol,
                conflicting_names.join(", ")
            ),
            serde_json::json!({
                "conflicting_names": conflicting_names,
                "suggestions": suggestions,
                "symbol": symbol,
            }),
        );
    }

    // --- Substitute params in body ---
    let substituted_body = substitute_params(&body_text, &param_to_arg, lang);
    let substitution_count = param_to_arg.len();

    // --- Build replacement text ---
    //
    // Indentation contract: `replacement_text` is built with `replacement_indent`
    // ALREADY prepended to each line. To avoid the indent being doubled, we
    // expand `replacement_node` backwards over leading whitespace to the start
    // of its line so the replacement OVERWRITES the original indent rather than
    // inserting AFTER it.
    //
    // Without this, a 2-space-indented `const result = helper(5);` produced
    // `    const result = 5 * 2;` (4-space) because the original `  ` remained
    // before the replacement's `  const ...`.
    let replacement_indent = get_line_indent(&source, call_site_line as usize);
    let replacement_text = build_inline_replacement(
        &substituted_body,
        &call_context,
        &replacement_indent,
        lang,
        assignment_var.as_deref(),
    );

    // Expand replacement start backwards over horizontal whitespace to start
    // of line. Stop at start-of-file or newline.
    let replace_start = {
        let bytes = source.as_bytes();
        let mut s = replacement_node.start_byte();
        while s > 0 && matches!(bytes[s - 1], b' ' | b'\t') {
            s -= 1;
        }
        s
    };

    // --- Compute new file content ---
    let new_source = match edit::replace_byte_range(
        &source,
        replace_start,
        replacement_node.end_byte(),
        &replacement_text,
    ) {
        Ok(s) => s,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    // --- Auto-backup before mutation ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        &format!("inline_symbol: {}", symbol),
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Write, format, validate ---
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

    log::debug!("inline_symbol: {} at {}:{}", symbol, file, call_site_line);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "symbol": symbol,
        "call_context": call_context,
        "substitutions": substitution_count,
        "conflicts": [],
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

// ---------------------------------------------------------------------------
// AST helpers
// ---------------------------------------------------------------------------

/// Find a function/arrow_function/function_definition node at the given byte position.
fn find_function_node_at<'a>(
    root: &tree_sitter::Node<'a>,
    byte_pos: usize,
    lang: LangId,
) -> Option<tree_sitter::Node<'a>> {
    let fn_kinds: &[&str] = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
        ],
        LangId::Python => &["function_definition"],
        _ => &[],
    };

    // Find the function node that starts at or contains byte_pos
    find_node_at(root, byte_pos, fn_kinds)
}

/// Find a node of given kinds that starts at or contains byte_pos.
fn find_node_at<'a>(
    node: &tree_sitter::Node<'a>,
    byte_pos: usize,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if node.end_byte() <= byte_pos {
        return None;
    }

    if kinds.contains(&node.kind()) && node.start_byte() <= byte_pos && byte_pos < node.end_byte() {
        return Some(*node);
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if child.start_byte() <= byte_pos && byte_pos < child.end_byte() {
                if let Some(found) = find_node_at(&child, byte_pos, kinds) {
                    return Some(found);
                }
            }
        }
    }

    // Also check for lexical_declaration wrapping arrow_function
    if kinds.contains(&"arrow_function") && node.kind() == "lexical_declaration" {
        if node.start_byte() <= byte_pos && byte_pos < node.end_byte() {
            // Look for arrow_function inside
            let child_count = node.child_count();
            for i in 0..child_count {
                if let Some(child) = node.child(i as u32) {
                    if child.kind() == "variable_declarator" {
                        if let Some(value) = child.child_by_field_name("value") {
                            if value.kind() == "arrow_function" {
                                return Some(value);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Extract parameter names and body text from a function node.
fn extract_fn_params_and_body(
    fn_node: &tree_sitter::Node,
    source: &str,
    lang: LangId,
) -> (Vec<String>, String) {
    let mut param_names = Vec::new();

    // Collect parameter names
    let params_node = fn_node.child_by_field_name("parameters");
    if let Some(params) = params_node {
        let child_count = params.child_count();
        for i in 0..child_count {
            if let Some(child) = params.child(i as u32) {
                match lang {
                    LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                        if child.kind() == "required_parameter"
                            || child.kind() == "optional_parameter"
                        {
                            if let Some(pattern) = child.child_by_field_name("pattern") {
                                if pattern.kind() == "identifier" {
                                    param_names.push(node_text(source, &pattern).to_string());
                                }
                            }
                        } else if child.kind() == "identifier" {
                            param_names.push(node_text(source, &child).to_string());
                        }
                    }
                    LangId::Python => {
                        if child.kind() == "identifier" {
                            let name = node_text(source, &child).to_string();
                            if name != "self" {
                                param_names.push(name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Extract body text
    let body_text = if let Some(body) = fn_node.child_by_field_name("body") {
        let raw = node_text(source, &body);
        match lang {
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                // For statement_block, strip outer { }
                if body.kind() == "statement_block" {
                    strip_braces(raw)
                } else {
                    // Expression body (arrow function) — keep as-is
                    raw.to_string()
                }
            }
            LangId::Python => {
                // Python function body — the "body" field contains the block
                raw.to_string()
            }
            _ => raw.to_string(),
        }
    } else {
        String::new()
    };

    (param_names, body_text)
}

/// Strip outer braces and de-indent a statement block.
fn strip_braces(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        // Remove leading/trailing newlines
        let inner = inner.trim_start_matches('\n').trim_end_matches('\n');
        // De-indent: find minimum indent and strip it
        let min_indent = inner
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);

        inner
            .lines()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else if l.len() >= min_indent {
                    l[min_indent..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    }
}

/// Find a call expression node calling `symbol` within the given byte range.
fn find_call_node_at_line<'a>(
    root: &tree_sitter::Node<'a>,
    symbol: &str,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
) -> Option<tree_sitter::Node<'a>> {
    let call_kinds: Vec<&str> = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => vec!["call_expression"],
        LangId::Python => vec!["call"],
        _ => vec![],
    };

    find_call_recursive(root, symbol, source, start_byte, end_byte, &call_kinds)
}

/// Recursively find the first call node to `symbol` in the byte range.
fn find_call_recursive<'a>(
    node: &tree_sitter::Node<'a>,
    symbol: &str,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    call_kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return None;
    }

    if call_kinds.contains(&node.kind())
        && node.start_byte() >= start_byte
        && node.start_byte() < end_byte
        && node.end_byte() <= source.len()
    {
        if let Some(callee_name) = crate::calls::extract_callee_name(node, source) {
            if callee_name == symbol {
                return Some(*node);
            }
        }
    }

    // Recurse depth-first
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if let Some(found) =
                find_call_recursive(&child, symbol, source, start_byte, end_byte, call_kinds)
            {
                return Some(found);
            }
        }
    }

    None
}

/// Detect call context: whether the call is an assignment RHS or standalone.
///
/// Returns `(context_string, node_to_replace, assignment_var_name)` where:
/// - `context_string` is "assignment", "standalone", or "return"
/// - `node_to_replace` is the full expression statement or assignment to replace
/// - `assignment_var_name` is the variable being assigned to (for "assignment" context)
fn detect_call_context<'a>(
    call_node: &tree_sitter::Node<'a>,
    source: &str,
    _lang: LangId,
) -> (String, tree_sitter::Node<'a>, Option<String>) {
    if let Some(parent) = call_node.parent() {
        let pk = parent.kind();

        // Assignment RHS: const x = fn() or x = fn()
        if pk == "variable_declarator" || pk == "assignment" || pk == "assignment_expression" {
            // Extract the variable name
            let var_name = if pk == "variable_declarator" {
                parent
                    .child_by_field_name("name")
                    .map(|n| node_text(source, &n).to_string())
            } else {
                parent
                    .child_by_field_name("left")
                    .map(|n| node_text(source, &n).to_string())
            };

            // The full statement is the grandparent
            if let Some(grandparent) = parent.parent() {
                let gpk = grandparent.kind();
                if gpk == "lexical_declaration"
                    || gpk == "variable_declaration"
                    || gpk == "expression_statement"
                {
                    return ("assignment".to_string(), grandparent, var_name);
                }
            }
            return ("assignment".to_string(), parent, var_name);
        }

        // Expression statement: fn() on its own
        if pk == "expression_statement" {
            return ("standalone".to_string(), parent, None);
        }

        // Return statement: return fn()
        if pk == "return_statement" {
            return ("return".to_string(), parent, None);
        }
    }

    // Fallback: replace just the call node
    ("standalone".to_string(), *call_node, None)
}

/// Extract argument expressions from a call node.
fn extract_call_arguments(
    call_node: &tree_sitter::Node,
    source: &str,
    lang: LangId,
) -> Vec<String> {
    let mut args = Vec::new();

    let args_node = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            call_node.child_by_field_name("arguments")
        }
        LangId::Python => call_node.child_by_field_name("arguments"),
        _ => None,
    };

    if let Some(args_parent) = args_node {
        let child_count = args_parent.child_count();
        for i in 0..child_count {
            if let Some(child) = args_parent.child(i as u32) {
                // Skip punctuation: ( ) ,
                if child.kind() != "("
                    && child.kind() != ")"
                    && child.kind() != ","
                    && !child.kind().is_empty()
                {
                    args.push(node_text(source, &child).to_string());
                }
            }
        }
    }

    args
}

/// Get the leading whitespace of a source line.
fn get_line_indent(source: &str, line: usize) -> String {
    source
        .lines()
        .nth(line)
        .map(|l| {
            let trimmed = l.trim_start();
            l[..l.len() - trimmed.len()].to_string()
        })
        .unwrap_or_default()
}

/// Build the replacement text for the inlined function body.
///
/// Handles three call contexts:
/// - "assignment": `const x = fn()` → body statements + assign return value to x
/// - "standalone": `fn()` → body statements (strip return)
/// - "return": `return fn()` → body statements (keep return)
fn build_inline_replacement(
    body: &str,
    call_context: &str,
    indent: &str,
    lang: LangId,
    assignment_var: Option<&str>,
) -> String {
    let body_trimmed = body.trim();
    let lines: Vec<&str> = body_trimmed.lines().collect();

    match call_context {
        "assignment" => {
            if lines.len() == 1 {
                let line = lines[0].trim();
                if let Some(expr) = strip_return_prefix(line) {
                    // Single return: `const x = expr;`
                    if let Some(var) = assignment_var {
                        build_assignment_line(var, expr, indent, lang)
                    } else {
                        format!("{}{}", indent, expr)
                    }
                } else {
                    // Expression body (arrow fn): `const x = expr;`
                    if let Some(var) = assignment_var {
                        build_assignment_line(var, line, indent, lang)
                    } else {
                        format!("{}{}", indent, line)
                    }
                }
            } else {
                // Multi-line: emit all non-return lines, then assign return expr
                build_multiline_assignment(&lines, indent, lang, assignment_var)
            }
        }
        "standalone" => {
            // Strip return statements, keep everything else
            build_multiline_standalone(&lines, indent, lang)
        }
        "return" => {
            // Keep return statements as-is
            build_multiline_replacement(&lines, indent, lang)
        }
        _ => build_multiline_replacement(&lines, indent, lang),
    }
}

/// Strip "return " prefix and trailing semicolon from a return statement.
fn strip_return_prefix(line: &str) -> Option<&str> {
    let trimmed = line.strip_prefix("return ")?;
    Some(trimmed.trim_end_matches(';').trim())
}

/// Build a single assignment line.
fn build_assignment_line(var: &str, expr: &str, indent: &str, lang: LangId) -> String {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            format!("{}const {} = {};", indent, var, expr)
        }
        LangId::Python => {
            format!("{}{} = {}", indent, var, expr)
        }
        _ => format!("{}const {} = {};", indent, var, expr),
    }
}

/// Build multi-line replacement for assignment context.
/// Non-return lines are kept; the return line becomes an assignment.
fn build_multiline_assignment(
    lines: &[&str],
    indent: &str,
    lang: LangId,
    assignment_var: Option<&str>,
) -> String {
    let mut result = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            result.push(String::new());
        } else if let Some(expr) = strip_return_prefix(trimmed) {
            // Convert return to assignment
            if let Some(var) = assignment_var {
                result.push(build_assignment_line(var, expr, indent, lang));
            }
            // else: drop the return (void return in assignment context shouldn't happen)
        } else {
            result.push(format!("{}{}", indent, trimmed));
        }
    }
    result.join("\n")
}

/// Build multi-line replacement for standalone context (strip returns).
fn build_multiline_standalone(lines: &[&str], indent: &str, lang: LangId) -> String {
    let _ = lang;
    let mut result = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            result.push(String::new());
        } else if let Some(_expr) = strip_return_prefix(trimmed) {
            // In standalone context, drop the return entirely
            // (the return value isn't used)
        } else {
            result.push(format!("{}{}", indent, trimmed));
        }
    }
    result.join("\n")
}

/// Build multi-line replacement with proper indentation (preserving all lines).
fn build_multiline_replacement(lines: &[&str], indent: &str, _lang: LangId) -> String {
    let mut result = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            result.push(String::new());
        } else {
            result.push(format!("{}{}", indent, trimmed));
        }
    }
    result.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RawRequest;

    fn make_request(id: &str, command: &str, params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: id.to_string(),
            command: command.to_string(),
            params,
            lsp_hints: None,
            session_id: None,
        }
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("inline_symbol")
            .join(name)
    }

    // --- Param validation ---

    #[test]
    fn inline_symbol_missing_file() {
        let req = make_request("1", "inline_symbol", serde_json::json!({}));
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
        let msg = json["message"].as_str().unwrap();
        assert!(
            msg.contains("file"),
            "message should mention 'file': {}",
            msg
        );
    }

    #[test]
    fn inline_symbol_missing_symbol() {
        let req = make_request(
            "2",
            "inline_symbol",
            serde_json::json!({"file": "/tmp/test.ts"}),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
        let msg = json["message"].as_str().unwrap();
        assert!(
            msg.contains("symbol"),
            "message should mention 'symbol': {}",
            msg
        );
    }

    #[test]
    fn inline_symbol_missing_call_site_line() {
        let req = make_request(
            "3",
            "inline_symbol",
            serde_json::json!({"file": "/tmp/test.ts", "symbol": "foo"}),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn inline_symbol_unsupported_language() {
        let dir = std::env::temp_dir().join("aft_test_inline_lang");
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("test.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let req = make_request(
            "4",
            "inline_symbol",
            serde_json::json!({
                "file": file.display().to_string(),
                "symbol": "main",
                "call_site_line": 1,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "unsupported_language");

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Multiple returns rejection ---

    #[test]
    fn inline_symbol_multiple_returns_rejected() {
        let fixture = fixture_path("sample_multi.ts");

        let req = make_request(
            "5",
            "inline_symbol",
            serde_json::json!({
                "file": fixture.display().to_string(),
                "symbol": "multiReturn",
                "call_site_line": 9,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "multiple_returns");
        assert!(json["return_count"].as_u64().unwrap() >= 2);
    }

    // --- Scope conflict detection ---

    #[test]
    fn inline_symbol_scope_conflict_reported() {
        let fixture = fixture_path("sample_conflict.ts");

        let req = make_request(
            "6",
            "inline_symbol",
            serde_json::json!({
                "file": fixture.display().to_string(),
                "symbol": "compute",
                "call_site_line": 9,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_inline_symbol(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["success"], false,
            "should fail with scope_conflict: {:?}",
            json
        );
        assert_eq!(json["code"], "scope_conflict");
        let conflicting = json["conflicting_names"].as_array().unwrap();
        // Both 'temp' and/or 'result' could conflict — at minimum 'result' does
        // because `const result = compute(5)` declares `result` in main's scope
        assert!(
            !conflicting.is_empty(),
            "should report at least one conflict: {:?}",
            conflicting
        );
    }
}
