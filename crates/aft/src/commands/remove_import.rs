//! Handler for the `remove_import` command: remove an import statement (or a name from one).
//!
//! Two modes:
//! - If `name` is omitted: remove the entire import statement for the given module.
//! - If `name` is given and the import has multiple names: regenerate the import without that name.
//! - If `name` is given and the import has only that name (or it's a default/side-effect import):
//!   remove the entire import statement.

use std::path::Path;

use super::import_specifier_edit;
use super::organize_imports;
use crate::context::AppContext;
use crate::edit;
use crate::imports;
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle a `remove_import` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `module` (string, required) — the module path to match
///   - `name` (string, optional) — specific named import to remove; if omitted, remove entire import
///
/// Returns: `{ file, removed, module, name?, syntax_valid?, backup_id? }`
pub fn handle_remove_import(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "remove_import: missing required param 'file'",
            );
        }
    };

    let module = match req.params.get("module").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "remove_import: missing required param 'module'",
            );
        }
    };

    let name = req
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("remove_import: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "remove_import: unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            );
        }
    };

    if !imports::is_supported(lang) {
        return Response::error(
            &req.id,
            "unsupported_language",
            format!(
                "remove_import: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    let (module_owned, include_import_kind) = if matches!(lang, LangId::C | LangId::Cpp) {
        imports::normalize_include_module(module)
    } else {
        (module.to_string(), None)
    };
    let module = module_owned.as_str();

    // --- Parse file and imports ---
    let (source, tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if lang == LangId::Vue {
        if let Err(err) = imports::vue_single_script_content_range(&tree) {
            return Response::error(&req.id, err.code(), err.message("remove_import"));
        }
    }

    if matches!(lang, LangId::CSharp | LangId::Php)
        && organize_imports::imports_span_multiple_code_regions(&source, lang, &block.imports)
    {
        return Response::error_with_data(
            &req.id,
            "multi_region_imports",
            format!(
                "remove_import: imports in {file} span multiple code regions; refusing to remove because the target region is ambiguous"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    if lang == LangId::Php
        && block.imports.iter().any(|imp| {
            imports::php_grouped_use_shares_prefix(imp, module)
                || imports::php_grouped_use_matches_module(imp, module)
        })
    {
        return Response::error_with_data(
            &req.id,
            "unsupported_grouped_import",
            format!(
                "remove_import: PHP grouped use declarations matching '{module}' are not safe to edit member-wise; expand the grouped use first"
            ),
            serde_json::json!({ "file": file, "module": module }),
        );
    }

    // --- Find matching import ---
    let matching: Vec<(usize, &imports::ImportStatement)> = block
        .imports
        .iter()
        .enumerate()
        .filter(|(_, imp)| {
            let module_matches = if lang == LangId::Python {
                python_import_matches_module(imp, module)
            } else if lang == LangId::Php {
                imports::php_import_matches_module(imp, module)
            } else {
                imp.module_path == module
            };
            if !module_matches {
                return false;
            }
            if matches!(lang, LangId::C | LangId::Cpp) {
                if let Some(kind) = include_import_kind {
                    return imp.default_import.as_deref() == Some(kind);
                }
            }
            true
        })
        .collect();

    if matching.is_empty() {
        let mut result = serde_json::json!({
            "file": file,
            "removed": false,
            "module": module,
            "reason": "module_not_found",
            "no_op": true,
        });
        if let Some(ref n) = name {
            result["name"] = serde_json::json!(n);
        }
        return Response::success(&req.id, result);
    }

    // --- Determine edit ---
    let new_source = if let Some(ref target_name) = name {
        remove_name_from_imports(&source, &matching, target_name, lang)
    } else {
        remove_entire_imports(&source, &matching, lang, module)
    };
    let removed = new_source != source;

    if !removed {
        let reason = if name.is_some() {
            "name_not_found"
        } else {
            "no_matching_import_removed"
        };
        let mut result = serde_json::json!({
            "file": file,
            "removed": false,
            "module": module,
            "reason": reason,
            "no_op": true,
        });
        if let Some(ref n) = name {
            result["name"] = serde_json::json!(n);
        }
        return Response::success(&req.id, result);
    }

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "remove_import: pre-edit backup",
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

    // A rollback means post-write syntax validation failed and the file was
    // restored — the import was NOT removed. Report that honestly with an error
    // instead of claiming `removed: true`.
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "remove_import: removing '{module}' from {file} would produce invalid syntax; file left unchanged"
            ),
        );
    }

    log::debug!("remove_import: {}", file);

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "removed": removed,
        "module": module,
        "formatted": write_result.formatted,
    });

    if let Some(ref n) = name {
        result["name"] = serde_json::json!(n);
        let remaining_names = matching
            .iter()
            .map(|(_, imp)| {
                imp.names
                    .iter()
                    .filter(|specifier| !imports::specifier_matches(specifier, n))
                    .count()
            })
            .sum::<usize>();
        let only_name = matching.len() == 1
            && matching[0].1.names.len() == 1
            && matching[0]
                .1
                .names
                .iter()
                .any(|specifier| imports::specifier_matches(specifier, n))
            && (lang == LangId::Rust || matching[0].1.default_import.is_none())
            && matching[0].1.namespace_import.is_none();
        result["remaining_names"] = serde_json::json!(remaining_names);
        result["only_name"] = serde_json::json!(only_name);
    }

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

/// Remove a specific named import from the matched imports.
/// If the import only has that one name, remove the entire statement.
/// If it has multiple names, regenerate without the target name.
fn remove_name_from_imports(
    source: &str,
    matching: &[(usize, &imports::ImportStatement)],
    target_name: &str,
    lang: LangId,
) -> String {
    let mut result = source.to_string();
    // Process in reverse order to preserve byte offsets
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    for (_, imp) in matching {
        if lang == LangId::Python {
            if let Some(specifiers) = python_plain_import_specifiers(imp) {
                if specifiers
                    .iter()
                    .any(|specifier| imports::specifier_matches(specifier, target_name))
                {
                    let remaining: Vec<&str> = specifiers
                        .iter()
                        .filter(|specifier| !imports::specifier_matches(specifier, target_name))
                        .map(String::as_str)
                        .collect();
                    if remaining.is_empty() {
                        edits.push((line_range(source, &imp.byte_range), String::new()));
                    } else {
                        edits.push((
                            imp.byte_range.clone(),
                            format!("import {}", remaining.join(", ")),
                        ));
                    }
                }
                continue;
            }
        }

        if lang == LangId::Scala {
            if let Some(replacement) = remove_name_from_scala_import(imp, target_name) {
                match replacement {
                    Some(new_line) => edits.push((imp.byte_range.clone(), new_line)),
                    None => {
                        let range = line_range(source, &imp.byte_range);
                        edits.push((range, String::new()));
                    }
                }
            }
            continue;
        }

        // Match against either the imported name or the local binding so the
        // caller can ask to remove `input` even when the specifier is stored
        // verbatim as `stdin as input` (TS/JS).
        let any_match = imp
            .names
            .iter()
            .any(|n| imports::specifier_matches(n, target_name));
        if any_match {
            let new_names: Vec<String> = imp
                .names
                .iter()
                .filter(|n| !imports::specifier_matches(n, target_name))
                .cloned()
                .collect();
            // Rust visibility is metadata, not a surviving import binding.
            let has_other = (lang != LangId::Rust && imp.default_import.is_some())
                || imp.namespace_import.is_some()
                || !new_names.is_empty();
            if !has_other {
                // No bindings remain — remove entire statement
                let range = line_range(source, &imp.byte_range);
                edits.push((range, String::new()));
            } else {
                // Other bindings remain — edit the matched list entry in place when
                // the language has a lossless named-import list editor.
                let new_line = import_specifier_edit::remove_named_specifiers(
                    &imp.raw_text,
                    &imp.names,
                    target_name,
                    lang,
                )
                .unwrap_or_else(|| {
                    imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                        lang,
                        &imp.module_path,
                        &new_names,
                        imp.default_import.as_deref(),
                        imp.namespace_import.as_deref(),
                        imp.kind == imports::ImportKind::Type,
                        imports::es_import_attribute_clause(imp),
                        Some(imports::quotes::statement_style(&imp.raw_text)),
                    )
                });
                edits.push((imp.byte_range.clone(), new_line));
            }
        } else if lang != LangId::Rust && imp.default_import.as_deref() == Some(target_name) {
            // Removing the default import
            if imp.names.is_empty() && imp.namespace_import.is_none() {
                // Only default — remove entire statement
                let range = line_range(source, &imp.byte_range);
                edits.push((range, String::new()));
            } else {
                // Has named or namespace imports too — regenerate without default
                let new_line =
                    imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                        lang,
                        &imp.module_path,
                        &imp.names,
                        None,
                        imp.namespace_import.as_deref(),
                        imp.kind == imports::ImportKind::Type,
                        imports::es_import_attribute_clause(imp),
                        Some(imports::quotes::statement_style(&imp.raw_text)),
                    );
                edits.push((imp.byte_range.clone(), new_line));
            }
        } else if imp.namespace_import.as_deref() == Some(target_name) {
            // Removing the namespace import
            if imp.names.is_empty() && imp.default_import.is_none() {
                // Only namespace — remove entire statement
                let range = line_range(source, &imp.byte_range);
                edits.push((range, String::new()));
            } else {
                // Has default or named imports too — regenerate without namespace
                let new_line =
                    imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                        lang,
                        &imp.module_path,
                        &imp.names,
                        imp.default_import.as_deref(),
                        None,
                        imp.kind == imports::ImportKind::Type,
                        imports::es_import_attribute_clause(imp),
                        Some(imports::quotes::statement_style(&imp.raw_text)),
                    );
                edits.push((imp.byte_range.clone(), new_line));
            }
        }
    }

    // Apply edits in reverse order to preserve offsets
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    for (range, replacement) in edits {
        result = format!(
            "{}{}{}",
            &result[..range.start],
            replacement,
            &result[range.end..]
        );
    }

    result
}

fn remove_name_from_scala_import(
    imp: &imports::ImportStatement,
    target_name: &str,
) -> Option<Option<String>> {
    let any_match = imp
        .names
        .iter()
        .any(|name| imports::specifier_matches(name, target_name));
    if !any_match {
        return None;
    }

    let remaining_names: Vec<String> = imp
        .names
        .iter()
        .filter(|name| !imports::specifier_matches(name, target_name))
        .cloned()
        .collect();
    if remaining_names.is_empty() {
        return Some(None);
    }

    let replacement =
        rewrite_scala_selector_list(&imp.raw_text, target_name).unwrap_or_else(|| {
            imports::generate_import_line(
                LangId::Scala,
                &imp.module_path,
                &remaining_names,
                imp.default_import.as_deref(),
                false,
            )
        });
    Some(Some(replacement))
}

fn rewrite_scala_selector_list(raw_text: &str, target_name: &str) -> Option<String> {
    let open = raw_text.find('{')?;
    let close = raw_text.rfind('}')?;
    if close <= open {
        return None;
    }

    let body = &raw_text[open + 1..close];
    let selectors = split_scala_selectors(body);
    if selectors.is_empty() {
        return None;
    }

    let kept: Vec<String> = selectors
        .iter()
        .filter(|selector| !scala_selector_matches(selector, target_name))
        .map(|selector| selector.trim().to_string())
        .filter(|selector| !selector.is_empty())
        .collect();

    if kept.len() == selectors.len() || kept.is_empty() {
        return None;
    }

    Some(format!(
        "{}{}{}",
        &raw_text[..open + 1],
        kept.join(", "),
        &raw_text[close..]
    ))
}

fn split_scala_selectors(body: &str) -> Vec<String> {
    let mut selectors = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (idx, ch) in body.char_indices() {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                selectors.push(body[start..idx].to_string());
                start = idx + 1;
            }
            _ => {}
        }
    }
    selectors.push(body[start..].to_string());
    selectors
}

fn scala_selector_matches(selector: &str, target_name: &str) -> bool {
    let normalized = normalize_scala_selector(selector);
    imports::specifier_matches(&normalized, target_name)
}

fn normalize_scala_selector(selector: &str) -> String {
    let trimmed = selector.trim();
    if let Some((from, to)) = trimmed.split_once("=>") {
        format!("{} as {}", from.trim(), to.trim())
    } else {
        trimmed.to_string()
    }
}

/// Remove entire import statements for all matching imports.
fn remove_entire_imports(
    source: &str,
    matching: &[(usize, &imports::ImportStatement)],
    lang: LangId,
    module: &str,
) -> String {
    let mut result = source.to_string();
    // Process in reverse order to preserve byte offsets
    let mut edits: Vec<(std::ops::Range<usize>, String)> = matching
        .iter()
        .map(|(_, imp)| {
            if lang == LangId::Php {
                if let Some(replacement) = imports::rewrite_php_import_without_module(imp, module) {
                    return match replacement {
                        Some(new_line) => (imp.byte_range.clone(), new_line),
                        None => (line_range(source, &imp.byte_range), String::new()),
                    };
                }
            }
            if lang == LangId::Python {
                if let Some(specifiers) = python_plain_import_specifiers(imp) {
                    let remaining: Vec<&str> = specifiers
                        .iter()
                        .filter(|specifier| imports::specifier_imported_name(specifier) != module)
                        .map(String::as_str)
                        .collect();
                    if !remaining.is_empty() {
                        return (
                            imp.byte_range.clone(),
                            format!("import {}", remaining.join(", ")),
                        );
                    }
                }
            }
            (line_range(source, &imp.byte_range), String::new())
        })
        .collect();
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));

    for (range, replacement) in edits {
        result = format!(
            "{}{}{}",
            &result[..range.start],
            replacement,
            &result[range.end..]
        );
    }

    result
}

fn python_plain_import_specifiers(imp: &imports::ImportStatement) -> Option<&[String]> {
    match &imp.form {
        imports::ImportForm::Python {
            from_import: false,
            named,
        } => Some(named),
        _ => None,
    }
}

fn python_import_matches_module(imp: &imports::ImportStatement, module: &str) -> bool {
    python_plain_import_specifiers(imp).map_or_else(
        || imp.module_path == module,
        |specifiers| {
            specifiers
                .iter()
                .any(|specifier| imports::specifier_imported_name(specifier) == module)
        },
    )
}

/// Expand a byte range to include the full line (including trailing newline).
fn line_range(source: &str, range: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start = range.start;
    let mut end = range.end;

    // Include trailing newline
    if end < source.len() {
        let bytes = source.as_bytes();
        if bytes[end] == b'\n' {
            end += 1;
        } else if bytes[end] == b'\r' {
            end += 1;
            if end < source.len() && bytes[end] == b'\n' {
                end += 1;
            }
        }
    }

    start..end
}
