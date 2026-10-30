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
use crate::symbols::{SymbolKind, SymbolMatch};

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
        Some(l) if (1..=u64::from(u32::MAX)).contains(&l) => l as u32 - 1,
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "inline_symbol: 'call_site_line' must fit a positive 32-bit line number",
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
    let resolved_matches = match ctx.provider().resolve_symbol(&path, symbol) {
        Ok(matches) => matches
            .into_iter()
            .filter(|candidate| {
                matches!(
                    candidate.symbol.kind,
                    SymbolKind::Function | SymbolKind::Method
                )
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };
    if resolved_matches.is_empty() {
        return Response::error(
            &req.id,
            "symbol_not_found",
            format!("inline_symbol: no function '{}' found in {}", symbol, file),
        );
    }

    let candidates =
        inline_symbol_candidates(&resolved_matches, &tree.root_node(), &source, symbol, lang);

    // LSP hints may narrow declarations, but qualified calls still require the
    // complete candidate set below because a receiver cannot be type-resolved
    // safely from its final property name alone.
    let matches = if let Some(hints) = lsp_hints::parse_lsp_hints(req) {
        lsp_hints::apply_lsp_disambiguation(
            resolved_matches.clone(),
            &hints,
            &ctx.resolve_relative_path(Path::new(".")),
        )
    } else {
        resolved_matches.clone()
    };

    // --- Find call expression at call_site_line ---
    let call_line_start = edit::line_col_to_byte(&source, call_site_line, 0);
    let call_line_end = if (call_site_line + 1) as usize <= source.lines().count() {
        edit::line_col_to_byte(&source, call_site_line + 1, 0)
    } else {
        source.len()
    };
    let display_call_line = call_site_line + 1;

    let call_nodes = find_call_nodes_at_line(
        &tree.root_node(),
        symbol,
        &source,
        call_line_start,
        call_line_end,
        lang,
    );
    if call_nodes.is_empty() {
        return Response::error(
            &req.id,
            "call_not_found",
            format!(
                "inline_symbol: no call to '{}' found at line {} in {}",
                symbol, display_call_line, file
            ),
        );
    }
    if call_nodes.len() > 1 {
        let calls = call_nodes
            .iter()
            .map(|node| node_text(&source, node).to_string())
            .collect::<Vec<_>>();
        return Response::error_with_data(
            &req.id,
            "ambiguous_call_site",
            format!(
                "inline_symbol: line {} contains multiple calls named '{}'; choose a line with exactly one call",
                display_call_line, symbol
            ),
            serde_json::json!({
                "call_site_line": display_call_line,
                "calls": calls,
                "candidates": inline_candidates_json(&candidates),
            }),
        );
    }
    let call_node = call_nodes[0];
    let call_form = inline_callee_form(&call_node);

    // Bare identifiers can only refer to free functions. Qualified calls can
    // only use a uniquely resolved method; guessing a receiver from the final
    // property name can silently substitute an unrelated function body.
    let eligible = match call_form {
        InlineCalleeForm::Bare => matches
            .iter()
            .filter(|candidate| candidate.symbol.kind == SymbolKind::Function)
            .collect::<Vec<_>>(),
        InlineCalleeForm::Qualified => matches
            .iter()
            .filter(|candidate| candidate.symbol.kind == SymbolKind::Method)
            .collect::<Vec<_>>(),
    };

    let sym = match call_form {
        InlineCalleeForm::Bare if eligible.len() == 1 => eligible[0].symbol.clone(),
        InlineCalleeForm::Qualified if eligible.len() == 1 && candidates.len() == 1 => {
            eligible[0].symbol.clone()
        }
        InlineCalleeForm::Qualified => {
            let callee = crate::calls::extract_full_callee(&call_node, &source)
                .unwrap_or_else(|| node_text(&source, &call_node).to_string());
            return unsafe_qualified_call_response(
                &req.id,
                symbol,
                display_call_line,
                &callee,
                &candidates,
            );
        }
        InlineCalleeForm::Bare => {
            return ambiguous_bare_call_response(&req.id, symbol, display_call_line, &candidates);
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
    let (param_patterns, body_text) = extract_fn_params_and_body(&fn_node, &source, lang);
    let param_names = param_patterns
        .iter()
        .flat_map(InlineParam::binding_names)
        .collect::<Vec<_>>();

    // --- Determine call context ---
    let (call_context, replacement_node, assignment_var) =
        detect_call_context(&call_node, &source, lang);

    // --- Build param→arg map ---
    let args = extract_call_arguments(&call_node, &source, lang);
    let param_to_arg = match build_param_to_arg_map(&param_patterns, &args, lang) {
        Ok(map) => map,
        Err(reason) => {
            return Response::error_with_data(
                &req.id,
                "param_mismatch",
                format!("inline_symbol: cannot inline '{}': {}", symbol, reason),
                serde_json::json!({
                    "symbol": symbol,
                    "reason": reason,
                }),
            );
        }
    };

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
    // A body node that starts its own line (a Python block) begins at its
    // first statement, so the text's first line has lost the indentation
    // every later line keeps. Restore it, or the common-indent strip in
    // `build_inline_replacement` keeps the later lines one level too deep
    // (`total = x + y` then `        result = total`: an IndentationError).
    let substituted_body = match body_first_line_indent(&fn_node, &source) {
        Some(indent) => format!("{indent}{substituted_body}"),
        None => substituted_body,
    };
    // Lines inside a multi-line string literal are part of the string's
    // value, so re-indenting the body must leave them exactly as written.
    let body_lines = trim_body_keeping_indent(&substituted_body);
    let Some(in_string) = crate::fuzzy_match::text_lines_inside_strings(body_lines, &path, lang)
    else {
        return Response::error(
            &req.id,
            "ambiguous_indentation",
            format!(
                "inline_symbol: the body of '{}' does not parse cleanly, so its multi-line strings cannot be told apart from code and re-indenting it could change a string's value. Nothing was changed.",
                symbol
            ),
        );
    };

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
        body_lines,
        &in_string,
        &call_context,
        &replacement_indent,
        lang,
        assignment_var.as_deref(),
    );
    // The body's lines are joined with LF. In a file whose call-site line
    // ends in CRLF, every inlined line ends in CRLF too, so the file's line
    // endings stay consistent.
    let call_line_end = source[replacement_node.end_byte()..]
        .find('\n')
        .map(|at| replacement_node.end_byte() + at);
    let crlf = match call_line_end {
        Some(at) => at > 0 && source.as_bytes()[at - 1] == b'\r',
        None => source.contains("\r\n"),
    };
    let replacement_text = if crlf {
        replacement_text.replace('\n', "\r\n")
    } else {
        replacement_text
    };

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
    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let mut write_result = match edit::write_format_validate(
        tool_policy,
        &path,
        &new_source,
        &ctx.config(),
        &req.params,
    ) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Honesty: write_format_validate reverts the file when the inline produced
    // invalid syntax (e.g. inlining into expression context, or losing let/var).
    // The file is unchanged, so don't return a success response. Fail with the
    // validation errors so the agent retries.
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "inline_symbol produced invalid syntax; the file was left unchanged. {}",
                edit::format_validation_errors(&write_result.validation_errors)
            ),
        );
    }

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
    write_result.append_reformatted_excerpt_to(&mut result);
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

    // If this is an export wrapper (TS/JS `export_statement`), look for the
    // inner function/declaration inside. Symbol ranges for exported TS/JS
    // functions point at the `export` keyword (see parser.rs), but the
    // function_declaration child starts after `export `, so we need to walk
    // into it explicitly rather than relying on a byte-pos containment check.
    if node.kind() == "export_statement"
        && node.start_byte() <= byte_pos
        && byte_pos < node.end_byte()
    {
        let child_count = node.child_count();
        for i in 0..child_count {
            if let Some(child) = node.child(i as u32) {
                if kinds.contains(&child.kind()) {
                    return Some(child);
                }
                // Arrow-function case: `export const f = (...) => {...}` wraps
                // a lexical_declaration with an arrow_function inside.
                if kinds.contains(&"arrow_function") && child.kind() == "lexical_declaration" {
                    let grand_count = child.child_count();
                    for j in 0..grand_count {
                        if let Some(grand) = child.child(j as u32) {
                            if grand.kind() == "variable_declarator" {
                                if let Some(value) = grand.child_by_field_name("value") {
                                    if value.kind() == "arrow_function" {
                                        return Some(value);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
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

#[derive(Debug, Clone)]
enum InlineParam {
    Simple {
        name: String,
        default_value: Option<InlineArgument>,
    },
    Destructured {
        bindings: Vec<DestructuredBinding>,
    },
    Rest {
        name: String,
    },
    Unsupported {
        description: String,
    },
}

#[derive(Debug, Clone)]
struct DestructuredBinding {
    name: String,
    access_path: String,
}

#[derive(Debug, Clone)]
struct InlineArgument {
    text: String,
    kind: String,
    has_template_substitution: bool,
}

impl InlineArgument {
    fn from_node(node: &tree_sitter::Node, source: &str) -> Self {
        Self {
            text: node_text(source, node).to_string(),
            kind: node.kind().to_string(),
            has_template_substitution: node_contains_kind(node, "template_substitution"),
        }
    }

    fn substitution_text(&self, lang: LangId) -> String {
        // Parameter references are expression positions because binding identifiers are
        // excluded by `substitute_params`. Atomic expressions can replace an identifier
        // directly; every unknown or precedence-sensitive form is grouped because extra
        // parentheses preserve expression semantics while missing ones can change them.
        if argument_is_atomic(self, lang) {
            self.text.clone()
        } else {
            format!("({})", self.text)
        }
    }
}

fn node_contains_kind(node: &tree_sitter::Node, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }

    (0..node.child_count()).any(|index| {
        node.child(index as u32)
            .is_some_and(|child| node_contains_kind(&child, kind))
    })
}

fn argument_is_atomic(argument: &InlineArgument, lang: LangId) -> bool {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => match argument.kind.as_str() {
            "identifier"
            | "number"
            | "string"
            | "regex"
            | "true"
            | "false"
            | "null"
            | "member_expression"
            | "call_expression"
            | "parenthesized_expression"
            | "array"
            | "object" => true,
            "template_string" => !argument.has_template_substitution,
            _ => false,
        },
        LangId::Python => matches!(
            argument.kind.as_str(),
            "identifier"
                | "integer"
                | "float"
                | "string"
                | "concatenated_string"
                | "true"
                | "false"
                | "none"
                | "ellipsis"
                | "attribute"
                | "call"
                | "parenthesized_expression"
                | "list"
                | "set"
                | "dictionary"
                | "tuple"
        ),
        _ => false,
    }
}

impl InlineParam {
    fn binding_names(&self) -> Vec<String> {
        match self {
            InlineParam::Simple { name, .. } | InlineParam::Rest { name } => vec![name.clone()],
            InlineParam::Destructured { bindings } => bindings
                .iter()
                .map(|binding| binding.name.clone())
                .collect(),
            InlineParam::Unsupported { .. } => Vec::new(),
        }
    }
}

/// Extract parameter patterns and body text from a function node.
fn extract_fn_params_and_body(
    fn_node: &tree_sitter::Node,
    source: &str,
    lang: LangId,
) -> (Vec<InlineParam>, String) {
    let mut param_patterns = Vec::new();

    // Collect parameter bindings. TS/JS parameters may be identifiers, default
    // wrappers, rest params, or destructuring patterns; keep enough structure to
    // build safe substitutions once the call arguments are known.
    let params_node = fn_node.child_by_field_name("parameters");
    if let Some(params) = params_node {
        let child_count = params.child_count();
        for i in 0..child_count {
            if let Some(child) = params.child(i as u32) {
                match lang {
                    LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                        if let Some(param) = extract_ts_param_pattern(&child, source) {
                            param_patterns.push(param);
                        }
                    }
                    LangId::Python => {
                        if child.kind() == "identifier" {
                            let name = node_text(source, &child).to_string();
                            if name != "self" {
                                param_patterns.push(InlineParam::Simple {
                                    name,
                                    default_value: None,
                                });
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

    (param_patterns, body_text)
}

fn extract_ts_param_pattern(node: &tree_sitter::Node, source: &str) -> Option<InlineParam> {
    match node.kind() {
        "required_parameter" | "optional_parameter" => {
            let raw = node_text(source, node).trim_start();
            if raw.starts_with("...") {
                return node
                    .child_by_field_name("pattern")
                    .and_then(|pattern| first_identifier_name(&pattern, source))
                    .map(|name| InlineParam::Rest { name });
            }

            if let Some(default_value) = node.child_by_field_name("value") {
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    if pattern.kind() == "identifier" {
                        return Some(InlineParam::Simple {
                            name: node_text(source, &pattern).to_string(),
                            default_value: Some(InlineArgument::from_node(&default_value, source)),
                        });
                    }
                }
            }

            node.child_by_field_name("pattern")
                .and_then(|pattern| extract_ts_param_pattern(&pattern, source))
        }
        "identifier" => Some(InlineParam::Simple {
            name: node_text(source, node).to_string(),
            default_value: None,
        }),
        "assignment_pattern" => {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("pattern"));
            let right = node
                .child_by_field_name("right")
                .or_else(|| node.child_by_field_name("value"));
            match (left, right) {
                (Some(left), Some(right)) if left.kind() == "identifier" => {
                    Some(InlineParam::Simple {
                        name: node_text(source, &left).to_string(),
                        default_value: Some(InlineArgument::from_node(&right, source)),
                    })
                }
                (Some(left), _) => extract_ts_param_pattern(&left, source),
                _ => Some(InlineParam::Unsupported {
                    description: format!(
                        "unsupported default parameter `{}`",
                        node_text(source, node)
                    ),
                }),
            }
        }
        "rest_pattern" => {
            first_identifier_name(node, source).map(|name| InlineParam::Rest { name })
        }
        "object_pattern" | "array_pattern" => {
            let mut bindings = Vec::new();
            if collect_destructured_bindings(node, source, "", &mut bindings).is_some() {
                Some(InlineParam::Destructured { bindings })
            } else {
                Some(InlineParam::Unsupported {
                    description: format!(
                        "unsupported destructured parameter `{}`",
                        node_text(source, node)
                    ),
                })
            }
        }
        "(" | ")" | "," => None,
        _ if !node.is_named() => None,
        _ => None,
    }
}

fn first_identifier_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier" | "shorthand_property_identifier_pattern"
    ) {
        return Some(node_text(source, node).to_string());
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if let Some(name) = first_identifier_name(&child, source) {
                return Some(name);
            }
        }
    }

    None
}

fn collect_destructured_bindings(
    node: &tree_sitter::Node,
    source: &str,
    access_prefix: &str,
    out: &mut Vec<DestructuredBinding>,
) -> Option<()> {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.push(DestructuredBinding {
                name: node_text(source, node).to_string(),
                access_path: access_prefix.to_string(),
            });
            Some(())
        }
        "assignment_pattern" => {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("pattern"))?;
            collect_destructured_bindings(&left, source, access_prefix, out)
        }
        "object_pattern" => collect_object_pattern_bindings(node, source, access_prefix, out),
        "array_pattern" => collect_array_pattern_bindings(node, source, access_prefix, out),
        _ => None,
    }
}

fn collect_object_pattern_bindings(
    node: &tree_sitter::Node,
    source: &str,
    access_prefix: &str,
    out: &mut Vec<DestructuredBinding>,
) -> Option<()> {
    let child_count = node.child_count();
    for i in 0..child_count {
        let Some(child) = node.child(i as u32) else {
            continue;
        };
        match child.kind() {
            "{" | "}" | "," => {}
            "shorthand_property_identifier_pattern" | "identifier" => {
                let name = node_text(source, &child);
                out.push(DestructuredBinding {
                    name: name.to_string(),
                    access_path: format!("{}.{name}", access_prefix),
                });
            }
            "pair_pattern" | "pair" => {
                let key = child.child_by_field_name("key")?;
                let value = child
                    .child_by_field_name("value")
                    .or_else(|| child.child_by_field_name("pattern"))?;
                let property = property_access_for_key(&key, source)?;
                let nested_access = format!("{}{}", access_prefix, property);
                collect_destructured_bindings(&value, source, &nested_access, out)?;
            }
            "object_assignment_pattern" => {
                let name = first_identifier_name(&child, source)?;
                out.push(DestructuredBinding {
                    access_path: format!("{}.{name}", access_prefix),
                    name,
                });
            }
            "rest_pattern" => return None,
            _ if child.is_named() => return None,
            _ => {}
        }
    }

    Some(())
}

fn collect_array_pattern_bindings(
    node: &tree_sitter::Node,
    source: &str,
    access_prefix: &str,
    out: &mut Vec<DestructuredBinding>,
) -> Option<()> {
    let mut element_index = 0usize;
    let child_count = node.child_count();
    for i in 0..child_count {
        let Some(child) = node.child(i as u32) else {
            continue;
        };
        match child.kind() {
            "[" | "]" | "," => {}
            "rest_pattern" => return None,
            _ if child.is_named() => {
                let nested_access = format!("{}[{element_index}]", access_prefix);
                collect_destructured_bindings(&child, source, &nested_access, out)?;
                element_index += 1;
            }
            _ => {}
        }
    }

    Some(())
}

fn property_access_for_key(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let key = node_text(source, node).trim();
    if is_simple_identifier(key) {
        Some(format!(".{key}"))
    } else {
        None
    }
}

fn build_param_to_arg_map(
    params: &[InlineParam],
    args: &[InlineArgument],
    lang: LangId,
) -> Result<HashMap<String, String>, String> {
    let mut param_to_arg = HashMap::new();

    for (i, param) in params.iter().enumerate() {
        match param {
            InlineParam::Simple {
                name,
                default_value,
            } => {
                if let Some(arg) = args.get(i) {
                    param_to_arg.insert(name.clone(), arg.substitution_text(lang));
                } else if let Some(default_value) = default_value {
                    param_to_arg.insert(name.clone(), default_value.substitution_text(lang));
                }
            }
            InlineParam::Destructured { bindings } => {
                let Some(arg) = args.get(i) else {
                    return Err(format!(
                        "missing argument for destructured parameter {}",
                        i + 1
                    ));
                };
                if !is_simple_identifier(arg.text.trim()) {
                    return Err(format!(
                        "destructured parameter {} requires a simple variable argument, got `{}`",
                        i + 1,
                        arg.text.trim()
                    ));
                }
                for binding in bindings {
                    param_to_arg.insert(
                        binding.name.clone(),
                        format!("{}{}", arg.text.trim(), binding.access_path),
                    );
                }
            }
            InlineParam::Rest { name } => {
                let rest_args = if i < args.len() { &args[i..] } else { &[] };
                let elements = rest_args
                    .iter()
                    .map(|argument| argument.text.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                param_to_arg.insert(name.clone(), format!("[{elements}]"));
            }
            InlineParam::Unsupported { description } => return Err(description.clone()),
        }
    }

    Ok(param_to_arg)
}

fn is_simple_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod unicode_tests {
    #[test]
    fn mixed_unicode_indent_is_stripped_on_character_boundaries() {
        let body = super::strip_braces("{\n first();\n\u{3000}second();\n}");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            super::strip_common_indent(&lines, &[false, false]),
            ["first();", "second();"]
        );
    }
}

/// Strip the outer braces of a statement block and the blank lines just
/// inside them. The body keeps its indentation: `strip_common_indent` removes
/// it later, skipping lines inside multi-line strings (a template literal's
/// lines are part of its value).
fn strip_braces(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .trim_start_matches('\n')
            .trim_end_matches('\n')
            .to_string()
    } else {
        text.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineCalleeForm {
    Bare,
    Qualified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineCandidate {
    name: String,
    qualified: String,
    line: u32,
    kind: String,
}

fn inline_callee_form(call_node: &tree_sitter::Node) -> InlineCalleeForm {
    let mut callee = call_node
        .child_by_field_name("function")
        .or_else(|| call_node.child(0));
    while let Some(node) = callee {
        if matches!(
            node.kind(),
            "generic_function" | "template_function" | "template_method"
        ) {
            callee = node
                .child_by_field_name("function")
                .or_else(|| node.named_child(0));
        } else {
            return if node.kind() == "identifier" {
                InlineCalleeForm::Bare
            } else {
                InlineCalleeForm::Qualified
            };
        }
    }

    // A missing or unfamiliar callee shape must not be treated as a bare call.
    // Qualified handling rejects it unless a unique method declaration exists.
    InlineCalleeForm::Qualified
}

fn inline_symbol_candidates(
    matches: &[SymbolMatch],
    root: &tree_sitter::Node,
    source: &str,
    symbol: &str,
    lang: LangId,
) -> Vec<InlineCandidate> {
    let mut candidates = matches
        .iter()
        .map(|candidate| {
            let symbol = &candidate.symbol;
            InlineCandidate {
                name: symbol.name.clone(),
                qualified: if symbol.scope_chain.is_empty() {
                    symbol.name.clone()
                } else {
                    format!("{}::{}", symbol.scope_chain.join("::"), symbol.name)
                },
                line: symbol.range.start_line + 1,
                kind: inline_symbol_kind(&symbol.kind),
            }
        })
        .collect::<Vec<_>>();

    // The shared symbol index currently records methods owned by classes, but
    // object-literal methods are still declarations relevant to ambiguity. Scan
    // those AST nodes locally so a qualified call is rejected instead of being
    // mistaken for a same-named free function.
    if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        collect_local_method_candidates(root, source, symbol, &mut candidates);
    }

    candidates.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then_with(|| left.qualified.cmp(&right.qualified))
            .then_with(|| left.kind.cmp(&right.kind))
    });
    candidates.dedup();
    candidates
}

fn inline_symbol_kind(kind: &SymbolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| format!("{kind:?}").to_lowercase())
}

fn collect_local_method_candidates(
    node: &tree_sitter::Node,
    source: &str,
    symbol: &str,
    candidates: &mut Vec<InlineCandidate>,
) {
    if node.kind() == "method_definition" {
        if let Some(name_node) = node.child_by_field_name("name") {
            if node_text(source, &name_node) == symbol {
                let owner = inline_method_owner(node, source);
                candidates.push(InlineCandidate {
                    name: symbol.to_string(),
                    qualified: owner
                        .map(|owner| format!("{owner}::{symbol}"))
                        .unwrap_or_else(|| symbol.to_string()),
                    line: node.start_position().row as u32 + 1,
                    kind: "method".to_string(),
                });
            }
        }
    }

    for index in 0..node.child_count() {
        if let Some(child) = node.child(index as u32) {
            collect_local_method_candidates(&child, source, symbol, candidates);
        }
    }
}

fn inline_method_owner(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if matches!(current.kind(), "class_declaration" | "variable_declarator") {
            if let Some(name) = current.child_by_field_name("name") {
                return Some(node_text(source, &name).to_string());
            }
        }
        ancestor = current.parent();
    }
    None
}

fn inline_candidates_json(candidates: &[InlineCandidate]) -> Vec<serde_json::Value> {
    candidates
        .iter()
        .map(|candidate| {
            serde_json::json!({
                "name": candidate.name,
                "qualified": candidate.qualified,
                "line": candidate.line,
                "kind": candidate.kind,
            })
        })
        .collect()
}

fn inline_candidates_text(candidates: &[InlineCandidate]) -> String {
    candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} ({} at line {})",
                candidate.qualified, candidate.kind, candidate.line
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn unsafe_qualified_call_response(
    request_id: &str,
    symbol: &str,
    call_site_line: u32,
    callee: &str,
    candidates: &[InlineCandidate],
) -> Response {
    Response::error_with_data(
        request_id,
        "ambiguous_call_target",
        format!(
            "inline_symbol: qualified call '{callee}' at line {call_site_line} cannot be safely resolved because receiver dispatch is not type-resolved; candidate declarations for '{symbol}': {}",
            inline_candidates_text(candidates)
        ),
        serde_json::json!({
            "call_site_line": call_site_line,
            "callee": callee,
            "candidates": inline_candidates_json(candidates),
        }),
    )
}

fn ambiguous_bare_call_response(
    request_id: &str,
    symbol: &str,
    call_site_line: u32,
    candidates: &[InlineCandidate],
) -> Response {
    Response::error_with_data(
        request_id,
        "ambiguous_call_target",
        format!(
            "inline_symbol: bare call '{symbol}' at line {call_site_line} does not resolve to exactly one free-function declaration; candidates: {}",
            inline_candidates_text(candidates)
        ),
        serde_json::json!({
            "call_site_line": call_site_line,
            "callee": symbol,
            "candidates": inline_candidates_json(candidates),
        }),
    )
}

/// Find every call named `symbol` that starts on the requested source line.
fn find_call_nodes_at_line<'a>(
    root: &tree_sitter::Node<'a>,
    symbol: &str,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: LangId,
) -> Vec<tree_sitter::Node<'a>> {
    let call_kinds: &[&str] = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => &["call_expression"],
        LangId::Python => &["call"],
        _ => &[],
    };
    let mut matches = Vec::new();
    collect_call_nodes(
        root,
        symbol,
        source,
        start_byte,
        end_byte,
        call_kinds,
        &mut matches,
    );
    matches
}

fn collect_call_nodes<'a>(
    node: &tree_sitter::Node<'a>,
    symbol: &str,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    call_kinds: &[&str],
    matches: &mut Vec<tree_sitter::Node<'a>>,
) {
    if node.end_byte() <= start_byte || node.start_byte() >= end_byte {
        return;
    }

    if call_kinds.contains(&node.kind())
        && node.start_byte() >= start_byte
        && node.start_byte() < end_byte
        && node.end_byte() <= source.len()
        && crate::calls::extract_callee_name(node, source).as_deref() == Some(symbol)
    {
        matches.push(*node);
    }

    for index in 0..node.child_count() {
        if let Some(child) = node.child(index as u32) {
            collect_call_nodes(
                &child, symbol, source, start_byte, end_byte, call_kinds, matches,
            );
        }
    }
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
) -> Vec<InlineArgument> {
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
                    args.push(InlineArgument::from_node(&child, source));
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

/// The whitespace before the function body on its first line, when the body
/// starts its own line (a Python block after `def f():`). `None` when
/// anything else precedes it on that line, as for `{` bodies and same-line
/// suites like `def f(): return 1`.
fn body_first_line_indent<'s>(fn_node: &tree_sitter::Node, source: &'s str) -> Option<&'s str> {
    let body = fn_node.child_by_field_name("body")?;
    let line_start = source[..body.start_byte()]
        .rfind('\n')
        .map_or(0, |at| at + 1);
    let lead = &source[line_start..body.start_byte()];
    (!lead.is_empty() && lead.trim().is_empty()).then_some(lead)
}

/// Drop the body's leading blank lines and trailing whitespace, but keep the
/// first statement's indentation: trimming it would leave the first line
/// shallower than the rest, and the common-indent strip would then keep every
/// later line one level too deep.
fn trim_body_keeping_indent(body: &str) -> &str {
    let mut start = 0;
    for line in body.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        start += line.len();
    }
    body[start..].trim_end()
}

/// Build the replacement text for the inlined function body.
///
/// Handles three call contexts:
/// - "assignment": `const x = fn()` → body statements + assign return value to x
/// - "standalone": `fn()` → body statements (strip return)
/// - "return": `return fn()` → body statements (keep return)
/// `in_string[i]` says line `i` of the trimmed body starts inside a
/// multi-line string literal; such lines are emitted exactly as written.
fn build_inline_replacement(
    body: &str,
    in_string: &[bool],
    call_context: &str,
    indent: &str,
    lang: LangId,
    assignment_var: Option<&str>,
) -> String {
    let body_trimmed = trim_body_keeping_indent(body);
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
                build_multiline_assignment(&lines, in_string, indent, lang, assignment_var)
            }
        }
        "standalone" => {
            // Strip return statements, keep everything else
            build_multiline_standalone(&lines, in_string, indent, lang)
        }
        "return" => {
            // Keep return statements as-is
            build_multiline_replacement(&lines, in_string, indent, lang)
        }
        _ => build_multiline_replacement(&lines, in_string, indent, lang),
    }
}

/// Strip "return " prefix and trailing semicolon from a return statement.
fn strip_return_prefix(line: &str) -> Option<&str> {
    parse_return_statement(line).and_then(|expr| expr)
}

fn parse_return_statement(line: &str) -> Option<Option<&str>> {
    let trimmed = line.trim_start();
    if trimmed == "return" || trimmed == "return;" {
        return Some(None);
    }

    let expr = trimmed.strip_prefix("return ")?;
    let expr = expr.trim_end_matches(';').trim();
    if expr.is_empty() {
        Some(None)
    } else {
        Some(Some(expr))
    }
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
    in_string: &[bool],
    indent: &str,
    lang: LangId,
    assignment_var: Option<&str>,
) -> String {
    let normalized = strip_common_indent(lines, in_string);
    let mut result = Vec::new();
    for (index, line) in normalized.iter().enumerate() {
        let trimmed_start = line.trim_start();
        if is_string_line(in_string, index) {
            result.push(line.clone());
        } else if trimmed_start.is_empty() {
            result.push(String::new());
        } else if let Some(Some(expr)) = parse_return_statement(trimmed_start) {
            // Convert return to assignment, preserving any relative nesting indent.
            if let Some(var) = assignment_var {
                let line_indent = leading_whitespace(line);
                let assignment_indent = format!("{}{}", indent, line_indent);
                if let Some(open) = return_continuing_into_string(trimmed_start, in_string, index) {
                    result.push(build_assignment_open(var, open, &assignment_indent, lang));
                } else {
                    result.push(build_assignment_line(var, expr, &assignment_indent, lang));
                }
            }
            // else: drop the return (void return in assignment context shouldn't happen)
        } else if parse_return_statement(trimmed_start).is_some() {
            // Bare return in assignment context has no value to assign.
        } else {
            result.push(format!("{}{}", indent, line));
        }
    }
    result.join("\n")
}

/// Build multi-line replacement for standalone context.
fn build_multiline_standalone(
    lines: &[&str],
    in_string: &[bool],
    indent: &str,
    lang: LangId,
) -> String {
    let normalized = strip_common_indent(lines, in_string);
    let mut result = Vec::new();
    for (index, line) in normalized.iter().enumerate() {
        let trimmed_start = line.trim_start();
        if is_string_line(in_string, index) {
            result.push(line.clone());
        } else if trimmed_start.is_empty() {
            result.push(String::new());
        } else if let Some(return_expr) = parse_return_statement(trimmed_start) {
            if let Some(expr) = return_expr {
                let line_indent = leading_whitespace(line);
                if let Some(open) = return_continuing_into_string(trimmed_start, in_string, index) {
                    result.push(format!("{}{}{}", indent, line_indent, open));
                } else {
                    match lang {
                        LangId::Python => result.push(format!("{}{}{}", indent, line_indent, expr)),
                        _ => result.push(format!("{}{}{};", indent, line_indent, expr)),
                    }
                }
            }
            // Bare `return;` has no side effects and is dropped.
        } else {
            result.push(format!("{}{}", indent, line));
        }
    }
    result.join("\n")
}

/// Build multi-line replacement with proper indentation (preserving all lines).
fn build_multiline_replacement(
    lines: &[&str],
    in_string: &[bool],
    indent: &str,
    _lang: LangId,
) -> String {
    strip_common_indent(lines, in_string)
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if is_string_line(in_string, index) {
                line.clone()
            } else if line.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", indent, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_string_line(in_string: &[bool], index: usize) -> bool {
    in_string.get(index).copied().unwrap_or(false)
}

/// The returned expression exactly as written, when the `return` on line
/// `index` continues into a multi-line string on the next line. The
/// statement ends (and any `;` sits) on a later line, so this line is copied
/// without trimming or appending a terminator: either would change the
/// string's text.
fn return_continuing_into_string<'l>(
    trimmed_line: &'l str,
    in_string: &[bool],
    index: usize,
) -> Option<&'l str> {
    if !is_string_line(in_string, index + 1) {
        return None;
    }
    trimmed_line.strip_prefix("return").map(str::trim_start)
}

/// The opening line of an assignment whose value continues on later lines
/// (inside a multi-line string), so no terminator is appended.
fn build_assignment_open(var: &str, value: &str, indent: &str, lang: LangId) -> String {
    match lang {
        LangId::Python => format!("{}{} = {}", indent, var, value),
        _ => format!("{}const {} = {}", indent, var, value),
    }
}

/// Remove the indentation common to the body's code lines. Lines inside a
/// multi-line string literal (`in_string`) are part of the string's value:
/// they neither count toward the common indentation nor lose any of it, and
/// come back exactly as written. Indentation is counted in characters, so a
/// mix of ASCII and Unicode whitespace is stripped on character boundaries.
fn strip_common_indent(lines: &[&str], in_string: &[bool]) -> Vec<String> {
    let common_indent = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !is_string_line(in_string, *index) && !line.trim().is_empty())
        .map(|(_, line)| leading_whitespace(line).chars().count())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if is_string_line(in_string, index) {
                line.to_string()
            } else if line.trim().is_empty() {
                String::new()
            } else {
                line.chars().skip(common_indent).collect()
            }
        })
        .collect()
}

fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    &line[..line.len() - trimmed.len()]
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
