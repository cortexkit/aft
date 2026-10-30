//! Handler for the `extract_function` command: extract a range of code into
//! a new function with auto-detected parameters and return value.
//!
//! Follows the edit_symbol.rs pattern: validate → parse → compute →
//! auto_backup → write_format_validate → respond.

use std::path::Path;

use tree_sitter::Parser;

use crate::context::AppContext;
use crate::edit;
use crate::extract::{
    detect_free_variables, detect_return_value, generate_call_site, generate_extracted_function,
    ReturnKind,
};
use crate::indent::detect_indent;
use crate::parser::{detect_language, grammar_for, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `extract_function` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `name` (string, required) — name for the new function
///   - `start_line` (u32, required) — first line of the range to extract (1-based)
///   - `end_line` (u32, required) — last line (exclusive, 1-based) of the range to extract
///
/// Returns on success:
///   `{ file, name, parameters, return_type, extracted_range, call_site_range, syntax_valid?, backup_id }`
///
/// `syntax_valid` is absent when syntax validation could not run.
///
/// Error codes:
///   - `unsupported_language` — file is not TS/JS/TSX/Python
///   - `this_reference_in_range` — range contains `this`/`self`
pub fn handle_extract_function(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: missing required param 'file'",
            );
        }
    };

    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: missing required param 'name'",
            );
        }
    };

    let start_line_1based = match req.params.get("start_line").and_then(|v| v.as_u64()) {
        Some(l) if (1..=u64::from(u32::MAX)).contains(&l) => l as u32,
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: 'start_line' must fit a positive 32-bit line number",
            );
        }
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: missing required param 'start_line'",
            );
        }
    };
    let start_line = start_line_1based - 1;

    let end_line_1based = match req.params.get("end_line").and_then(|v| v.as_u64()) {
        Some(l) if (1..=u64::from(u32::MAX)).contains(&l) => l as u32,
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: 'end_line' must fit a positive 32-bit line number",
            );
        }
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "extract_function: missing required param 'end_line'",
            );
        }
    };
    let end_line = end_line_1based - 1;

    if start_line >= end_line {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "extract_function: start_line ({}) must be less than end_line ({})",
                start_line_1based, end_line_1based
            ),
        );
    }

    // --- Validate file ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("extract_function: file not found: {}", file),
        );
    }

    // --- Language guard (D101) ---
    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                "extract_function: unsupported file type",
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
                "extract_function: only TypeScript/JavaScript/Python files are supported, got {:?}",
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
                format!("extract_function: {}: {}", file, e),
            );
        }
    };

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Response::error(
            &req.id,
            "parse_error",
            "extract_function: failed to initialize parser",
        );
    }
    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                "extract_function: failed to parse file",
            );
        }
    };

    // --- Convert line range to byte range ---
    let start_byte = edit::line_col_to_byte(&source, start_line, 0);
    let end_byte = edit::line_col_to_byte(&source, end_line, 0);

    if start_byte >= source.len() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "extract_function: start_line {} is beyond end of file",
                start_line_1based
            ),
        );
    }

    if let Some(message) =
        crate::extract::unsupported_extraction_control_flow(&tree, start_byte, end_byte, lang)
    {
        return Response::error(&req.id, "unsupported_control_flow", message);
    }

    // --- Detect free variables ---
    let free_vars = detect_free_variables(&source, &tree, start_byte, end_byte, lang);

    // Check for this/self
    if free_vars.has_this_or_self {
        let keyword = match lang {
            LangId::Python => "self",
            _ => "this",
        };
        return Response::error(
            &req.id,
            "this_reference_in_range",
            format!(
                "extract_function: selected range contains '{}' reference. Consider extracting as a method instead, or move the {} usage outside the extracted range.",
                keyword, keyword
            ),
        );
    }

    // --- Find enclosing function for return value detection ---
    let root = tree.root_node();
    let enclosing_fn = find_enclosing_function_node(&root, start_byte, lang);
    let enclosing_fn_end_byte = enclosing_fn.map(|n| n.end_byte());

    // --- Detect return value ---
    let return_kind = match detect_return_value(
        &source,
        &tree,
        start_byte,
        end_byte,
        enclosing_fn_end_byte,
        lang,
    ) {
        Ok(kind) => kind,
        Err(error) => {
            return Response::error(&req.id, "unsupported_return_bindings", error.message);
        }
    };

    // --- Detect indentation ---
    let indent_style = detect_indent(&source, lang);

    // Determine base indent (indentation of the line where the enclosing function starts,
    // or no indent if at module level)
    let base_indent = if let Some(fn_node) = enclosing_fn {
        let fn_start_line = fn_node.start_position().row;
        get_line_indent(&source, fn_start_line as usize)
    } else {
        String::new()
    };

    // Determine the indent of the extracted range (for the call site)
    let range_indent = get_line_indent(&source, start_line as usize);

    // --- Extract body text ---
    let body_text = &source[start_byte..end_byte];
    let body_text = body_text
        .strip_suffix("\r\n")
        .or_else(|| body_text.strip_suffix('\n'))
        .unwrap_or(body_text);

    // --- Generate function and call site ---
    let extracted_fn = generate_extracted_function(
        name,
        &free_vars.parameters,
        &return_kind,
        body_text,
        &base_indent,
        lang,
        indent_style,
        // The selection's own line break is stripped above, so a one-line
        // body carries no line ending; take it from the whole file.
        crate::indent::source_line_ending(&source),
    );

    let call_site = generate_call_site(
        name,
        &free_vars.parameters,
        &return_kind,
        &range_indent,
        lang,
    );

    // --- Compute new file content ---
    // Insert the extracted function before the enclosing function (or at the range position
    // if there's no enclosing function).
    //
    // For TS/JS, when the enclosing function is wrapped in `export` (or
    // `export default`), the parser reports the function_declaration as
    // a child of an export_statement. If we use fn_node.start_byte() as
    // the insertion point, the `export` keyword stays attached to the
    // start of the file content, and the extracted function gets inserted
    // BETWEEN `export ` and `function`, producing:
    //   `export function newFn(...) {} \n\n function originalFn(...) {}`
    // The export keyword silently jumps from the original function to the
    // extracted one. Fix: if the parent is an export_statement, insert
    // before the export_statement instead.
    let insert_pos = if let Some(fn_node) = enclosing_fn {
        let mut anchor = fn_node;
        if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
            if let Some(parent) = fn_node.parent() {
                if parent.kind() == "export_statement" {
                    anchor = parent;
                }
            }
        }
        anchor.start_byte()
    } else {
        start_byte
    };

    let new_source = build_new_source(
        &source,
        insert_pos,
        start_byte,
        end_byte,
        &extracted_fn,
        &call_site,
    );

    // --- Return type string for the response ---
    let return_type = match &return_kind {
        ReturnKind::Expression(_) => "expression",
        ReturnKind::Variable(_) => "variable",
        ReturnKind::Variables(_) => "variables",
        ReturnKind::Void => "void",
    };

    // --- Auto-backup before mutation ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        &format!("extract_function: {}", name),
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

    // Honesty: write_format_validate reverts the file when the extraction
    // produced invalid syntax (e.g. extracting a class method emits a bare
    // `function helper` into a class body). The file is unchanged, so returning
    // a success response with the new symbol metadata would be a lie. Fail with
    // the validation errors so the agent retries. (edit_match/batch surface this
    // via rolled_back already; the refactor handlers did not.)
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "extract_function produced invalid syntax; the file was left unchanged. {}",
                edit::format_validation_errors(&write_result.validation_errors)
            ),
        );
    }

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    let param_count = free_vars.parameters.len();
    log::debug!(
        "extract_function: {} from {}:{}-{} ({} params)",
        name,
        file,
        start_line,
        end_line,
        param_count
    );

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "name": name,
        "parameters": free_vars.parameters,
        "return_type": return_type,
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

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn extraction_errors_report_submitted_one_based_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("source.ts");
        std::fs::write(&file, "function outer() { return 1; }\n").unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        for (start, end, message) in [
            (
                6,
                4,
                "extract_function: start_line (6) must be less than end_line (4)",
            ),
            (
                3,
                3,
                "extract_function: start_line (3) must be less than end_line (3)",
            ),
            (6, 7, "extract_function: start_line 6 is beyond end of file"),
        ] {
            let req = serde_json::from_value(serde_json::json!({"id": "bounds", "command": "extract_function", "file": file, "name": "extracted", "start_line": start, "end_line": end})).unwrap();
            let response = handle_extract_function(&req, &ctx);
            assert_eq!(response.data["code"], "invalid_request");
            assert_eq!(response.data["message"], message);
        }
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "function outer() { return 1; }\n"
        );
    }
}

/// Find the enclosing function node for a byte position.
fn find_enclosing_function_node<'a>(
    root: &'a tree_sitter::Node<'a>,
    byte_pos: usize,
    lang: LangId,
) -> Option<tree_sitter::Node<'a>> {
    let fn_kinds: &[&str] = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "lexical_declaration",
        ],
        LangId::Python => &["function_definition"],
        _ => &[],
    };

    find_deepest_ancestor(root, byte_pos, fn_kinds)
}

/// Find the deepest ancestor node of given kinds containing byte_pos.
fn find_deepest_ancestor<'a>(
    node: &tree_sitter::Node<'a>,
    byte_pos: usize,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut result: Option<tree_sitter::Node<'a>> = None;
    if kinds.contains(&node.kind()) && node.start_byte() <= byte_pos && byte_pos < node.end_byte() {
        result = Some(*node);
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if child.start_byte() <= byte_pos && byte_pos < child.end_byte() {
                if let Some(deeper) = find_deepest_ancestor(&child, byte_pos, kinds) {
                    result = Some(deeper);
                }
            }
        }
    }

    result
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

/// Build the new source with the extracted function inserted and the range replaced.
fn build_new_source(
    source: &str,
    insert_pos: usize,
    range_start: usize,
    range_end: usize,
    extracted_fn: &str,
    call_site: &str,
) -> String {
    let mut result = String::with_capacity(source.len() + extracted_fn.len() + 64);

    // Everything before the insertion point
    result.push_str(&source[..insert_pos]);

    // The extracted function + blank line
    result.push_str(extracted_fn);
    let ending = crate::indent::source_line_ending(source);
    result.push_str(ending);
    result.push_str(ending);

    // Everything between insert point and the range start (the original function
    // declaration up to where extraction begins)
    result.push_str(&source[insert_pos..range_start]);

    // The call site replacing the original range
    result.push_str(call_site);
    result.push_str(ending);

    // Everything after the range
    result.push_str(&source[range_end..]);

    result
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

    // --- Param validation ---

    #[test]
    fn extract_function_missing_file() {
        let req = make_request("1", "extract_function", serde_json::json!({}));
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
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
    fn extract_function_missing_name() {
        let req = make_request(
            "2",
            "extract_function",
            serde_json::json!({"file": "/tmp/test.ts"}),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
        let msg = json["message"].as_str().unwrap();
        assert!(
            msg.contains("name"),
            "message should mention 'name': {}",
            msg
        );
    }

    #[test]
    fn extract_function_missing_start_line() {
        let req = make_request(
            "3",
            "extract_function",
            serde_json::json!({"file": "/tmp/test.ts", "name": "foo"}),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn extract_function_unsupported_language() {
        // Create a temp .rs file (Rust is not supported for extract_function)
        let dir = std::env::temp_dir().join("aft_test_extract");
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("test.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let req = make_request(
            "4",
            "extract_function",
            serde_json::json!({
                "file": file.display().to_string(),
                "name": "foo",
                "start_line": 1,
                "end_line": 2,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "unsupported_language");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_function_invalid_line_range() {
        let dir = std::env::temp_dir().join("aft_test_extract_range");
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("test.ts");
        std::fs::write(&file, "const x = 1;\n").unwrap();

        let req = make_request(
            "5",
            "extract_function",
            serde_json::json!({
                "file": file.display().to_string(),
                "name": "foo",
                "start_line": 6,
                "end_line": 4,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_function_rejects_line_numbers_that_do_not_fit_internal_range() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.ts");
        let original = "function total() {\n  const a = 1;\n  const b = 2;\n  return a + b;\n}\n";
        std::fs::write(&file, original).unwrap();

        // These values used to wrap to lines 2-3 when narrowed to u32, causing
        // a valid extraction at a completely different range than requested.
        let req = make_request(
            "6",
            "extract_function",
            serde_json::json!({
                "file": file.display().to_string(),
                "name": "extracted",
                "start_line": u64::from(u32::MAX) + 3,
                "end_line": u64::from(u32::MAX) + 5,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );

        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
        assert_eq!(std::fs::read_to_string(file).unwrap(), original);
    }

    #[test]
    fn extract_function_this_reference_error() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/extract_function/sample_this.ts");

        let req = make_request(
            "6",
            "extract_function",
            serde_json::json!({
                "file": fixture.display().to_string(),
                "name": "extracted",
                "start_line": 5,
                "end_line": 8,
            }),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let resp = handle_extract_function(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "this_reference_in_range");
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn extraction_assembly_preserves_crlf_and_bom() {
        let source = "\u{feff}function outer() {\r\n  run();\r\n}\r\n";
        let start = source.find("  run();").unwrap();
        let end = start + "  run();\r\n".len();
        let content = build_new_source(
            source,
            3,
            start,
            end,
            "function helper() {\r\n  run();\r\n}",
            "  helper();",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &content,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), "\u{feff}function helper() {\r\n  run();\r\n}\r\n\r\nfunction outer() {\r\n  helper();\r\n}\r\n".as_bytes());
    }
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    fn extract(source: &str, suffix: &str, start: u32, end: u32) -> (serde_json::Value, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("source.{suffix}"));
        std::fs::write(&path, source.as_bytes()).unwrap();
        #[cfg(unix)]
        let held_original = std::fs::File::open(&path).unwrap();
        let config = crate::config::Config {
            storage_dir: Some(dir.path().join("state")),
            format_on_edit: false,
            ..Default::default()
        };
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let req = RawRequest {
            id: "guard".into(),
            command: "extract_function".into(),
            params: serde_json::json!({"file": path, "name": "helper", "start_line": start, "end_line": end, "validate": "off"}),
            lsp_hints: None,
            session_id: None,
        };
        let response = serde_json::to_value(handle_extract_function(&req, &ctx)).unwrap();
        if response["code"] == "unsupported_control_flow" {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(
                    held_original.metadata().unwrap().ino(),
                    std::fs::metadata(&path).unwrap().ino(),
                    "guard must refuse before any replacement"
                );
            }
        }
        (response, std::fs::read(path).unwrap())
    }

    #[test]
    fn bare_outer_returns_are_rejected_without_changing_bytes() {
        for (source, suffix) in [
            (
                "\u{feff}function outer() {\r\n    if (done) return;\r\n    finish();\r\n}\r\n",
                "ts",
            ),
            (
                "\u{feff}def outer():\r\n    if done: return\r\n    finish()\r\n",
                "py",
            ),
            ("function outer() {\n    return /* comment */;\n}\n", "js"),
        ] {
            let (response, bytes) = extract(source, suffix, 2, 3);
            assert_eq!(response["code"], "unsupported_control_flow", "{response}");
            assert_eq!(bytes, source.as_bytes());
        }
    }

    #[test]
    fn nested_returns_do_not_turn_helper_calls_into_caller_returns() {
        let source = "\u{feff}function outer(items) {\r\n    items.map(item => { return item; });\r\n    observe();\r\n    finish();\r\n}\r\n";
        let (response, bytes) = extract(source, "ts", 2, 4);
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(response["return_type"], "void");
        assert_eq!(bytes, "\u{feff}function helper(items) {\r\n    items.map(item => { return item; });\r\n    observe();\r\n}\r\n\r\nfunction outer(items) {\r\n    helper(items);\r\n    finish();\r\n}\r\n".as_bytes());
    }

    #[test]
    fn single_line_extraction_keeps_crlf_with_formatting_off() {
        // The selected line's only line break is stripped before generation,
        // so the helper's line ending must come from the whole file.
        let source = "function outer() {\r\n    prepare();\r\n    observe();\r\n}\r\n";
        let (response, bytes) = extract(source, "ts", 3, 4);
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "function helper() {\r\n    observe();\r\n}\r\n\r\nfunction outer() {\r\n    prepare();\r\n    helper();\r\n}\r\n"
        );
        let source = "def outer():\r\n    prepare()\r\n    observe()\r\n";
        let (response, bytes) = extract(source, "py", 3, 4);
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "def helper():\r\n    observe()\r\n\r\ndef outer():\r\n    prepare()\r\n    helper()\r\n"
        );
    }

    #[test]
    fn outer_await_and_yield_are_rejected_without_changing_bytes() {
        for (source, suffix) in [
            (
                "\u{feff}async function outer() {\r\n    await work();\r\n}\r\n",
                "ts",
            ),
            (
                "\u{feff}function* outer() {\r\n    yield value;\r\n}\r\n",
                "js",
            ),
            ("\u{feff}async def outer():\r\n    await work()\r\n", "py"),
            ("\u{feff}def outer():\r\n    yield value\r\n", "py"),
        ] {
            let (response, bytes) = extract(source, suffix, 2, 3);
            assert_eq!(response["code"], "unsupported_control_flow", "{response}");
            assert_eq!(bytes, source.as_bytes());
        }
    }
}
