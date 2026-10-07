//! Handler for the `add_import` command: add an import statement to a file.
//!
//! Analyzes existing imports, checks for duplicates, finds the correct
//! insertion point based on group and alphabetical ordering, and inserts
//! the new import with auto-backup and syntax validation.

use std::path::Path;

use super::import_specifier_edit;
use super::organize_imports;
use crate::context::AppContext;
use crate::edit;
use crate::imports;
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Coerce an array-of-strings param, tolerating the shapes models/MCP
/// clients send in practice: a real array, a JSON-stringified array
/// (`'["a","b"]'`), or a single bare string (one-element array).
///
/// The old inline parse used `.as_array()` + `unwrap_or_default()`, which
/// SILENTLY dropped a stringified `names` array — the request then fell
/// through to the side-effect-import branch and wrote `import "module";`
/// with `success: true` (observed live twice). Wrong-typed values are now a
/// hard `invalid_request` instead of a silent behavior change.
fn coerce_string_array_param(params: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = params.get(key) else {
        return Ok(Vec::new());
    };
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(arr) => Ok(arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(Vec::new());
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                if let Ok(serde_json::Value::Array(arr)) =
                    serde_json::from_str::<serde_json::Value>(trimmed)
                {
                    return Ok(arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect());
                }
            }
            Ok(vec![trimmed.to_string()])
        }
        _ => Err(format!(
            "add_import: param '{key}' must be an array of strings"
        )),
    }
}

/// Merge named import specifiers, sorting additions only when the existing list
/// is already sorted. An unsorted source list is user-authored order and keeps
/// new names appended rather than silently moving its existing entries.
pub(crate) fn merge_named_import_specifiers(
    existing: &[String],
    additions: &[String],
) -> Vec<String> {
    let mut merged = existing.to_vec();
    for addition in additions {
        let imported = imports::specifier_imported_name(addition);
        let local = imports::specifier_local_name(addition);
        if !merged.iter().any(|name| {
            imports::specifier_imported_name(name) == imported
                && imports::specifier_local_name(name) == local
        }) {
            merged.push(addition.clone());
        }
    }
    if existing.windows(2).all(|pair| {
        imports::specifier_local_name(&pair[0]) <= imports::specifier_local_name(&pair[1])
    }) {
        merged.sort_by(|left, right| {
            imports::specifier_local_name(left).cmp(imports::specifier_local_name(right))
        });
    }
    merged
}

/// Reject bindings that cannot be represented by an attributed JSON module.
///
/// The same check runs before either the merge branch or the fresh-import branch
/// constructs replacement text, so a rejected request cannot create a backup or
/// change the file. JSON modules expose only their default export; namespace
/// bindings are also refused because the import engine treats them as invalid
/// for this module type.
fn json_module_binding_error(
    req: &RawRequest,
    file: &str,
    module: &str,
    names: &[String],
    namespace: Option<&str>,
    lang: LangId,
    block: &imports::ImportBlock,
) -> Option<Response> {
    if !matches!(
        lang,
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue
    ) {
        return None;
    }

    let unsupported_json_names = names
        .iter()
        .filter(|name| imports::specifier_imported_name(name) != "default")
        .collect::<Vec<_>>();
    if unsupported_json_names.is_empty() && namespace.is_none() {
        return None;
    }

    if !block.imports.iter().any(|imp| {
        imp.module_path == module && imports::es_import_attribute_type(imp) == Some("json")
    }) {
        return None;
    }

    if !unsupported_json_names.is_empty() {
        let unsupported = unsupported_json_names
            .iter()
            .map(|name| format!("'{}'", imports::specifier_imported_name(name)))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(Response::error_with_data(
            &req.id,
            "unsupported_json_named_export",
            format!(
                "add_import: cannot add named binding(s) {unsupported} from JSON module '{module}': JSON modules expose no named exports other than 'default'"
            ),
            serde_json::json!({
                "file": file,
                "module": module,
                "unsupported_names": unsupported_json_names,
            }),
        ));
    }

    let namespace = namespace.expect("namespace is present when no named binding is unsupported");
    Some(Response::error_with_data(
        &req.id,
        "unsupported_json_named_export",
        format!(
            "add_import: cannot add namespace binding '* as {namespace}' from JSON module '{module}': JSON modules expose no named exports other than 'default'"
        ),
        serde_json::json!({
            "file": file,
            "module": module,
            "unsupported_names": [],
        }),
    ))
}

/// Handle an `add_import` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `module` (string, required) — the module path (e.g., "react", "./utils")
///   - `names` (array of strings, optional) — named imports (e.g., ["useState", "useEffect"])
///   - `default_import` (string, optional) — default import name (e.g., "React")
///   - `type_only` (bool, optional, default false) — whether this is a type-only import
///
/// Returns: `{ file, added, module, group, already_present?, syntax_valid?, backup_id? }`
pub fn handle_add_import(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_import: missing required param 'file'",
            );
        }
    };

    let module = match req.params.get("module").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "add_import: missing required param 'module'",
            );
        }
    };

    let names: Vec<String> = match coerce_string_array_param(&req.params, "names") {
        Ok(values) => values,
        Err(message) => return Response::error(&req.id, "invalid_request", message),
    };

    let default_import = req
        .params
        .get("default_import")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let type_only = req
        .params
        .get("type_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Namespace import (`* as ns`) and whole-module alias — used by engines
    // that support them (ES namespace; Solidity namespace + whole-file alias).
    let namespace = req
        .params
        .get("namespace")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let alias = req
        .params
        .get("alias")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut modifiers: Vec<String> = match coerce_string_array_param(&req.params, "modifiers") {
        Ok(values) => values,
        Err(message) => return Response::error(&req.id, "invalid_request", message),
    };

    let import_kind = req
        .params
        .get("import_kind")
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
            format!("add_import: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "add_import: unsupported file extension: {}",
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
                "add_import: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    // Must have at least one of: names, default_import, or neither (side-effect)
    // All combinations are valid.

    // C/C++ ergonomics: agents pass includes with their delimiter (`<vector>`
    // or `"foo.h"`). Strip it and infer the include kind so dedup,
    // classification, generation, and the reported module path all see the bare
    // header — otherwise generation double-wraps into `#include <<vector>>`,
    // which fails validation and silently rolls back.
    let (module_owned, inferred_import_kind) = if matches!(lang, LangId::C | LangId::Cpp) {
        imports::normalize_include_module(module)
    } else {
        (module.to_string(), None)
    };
    let module = module_owned.as_str();
    let import_kind = import_kind.or_else(|| inferred_import_kind.map(String::from));

    if let Err(reason) = validate_module_path_for_add(lang, module) {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("add_import: invalid module path '{module}': {reason}"),
        );
    }

    // --- Parse file and imports ---
    let (source, tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if lang == LangId::Vue {
        if let Err(err) = imports::vue_single_script_content_range(&tree) {
            return Response::error(&req.id, err.code(), err.message("add_import"));
        }
    }

    if let Some(error) = json_module_binding_error(
        req,
        file,
        module,
        &names,
        namespace.as_deref(),
        lang,
        &block,
    ) {
        return error;
    }

    if matches!(lang, LangId::CSharp | LangId::Php)
        && organize_imports::imports_span_multiple_code_regions(&source, lang, &block.imports)
    {
        return Response::error_with_data(
            &req.id,
            "multi_region_imports",
            format!(
                "add_import: imports in {file} span multiple code regions; refusing to add because the target region is ambiguous"
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
                "add_import: PHP grouped use declarations matching '{module}' are not safe to edit member-wise; expand the grouped use first"
            ),
            serde_json::json!({ "file": file, "module": module }),
        );
    }

    if lang == LangId::Scala
        && imports::scala_block_uses_scala2_dialect(&block)
        && !modifiers.iter().any(|modifier| modifier == "scala2")
    {
        modifiers.push("scala2".to_string());
    }

    let import_request = imports::ImportRequest {
        module_path: module,
        names: &names,
        default_import: default_import.as_deref(),
        namespace: namespace.as_deref(),
        alias: alias.as_deref(),
        type_only,
        modifiers: &modifiers,
        import_kind: import_kind.as_deref(),
    };

    // --- Check for duplicates ---
    if imports::is_duplicate_import_request(lang, &block, &import_request) {
        log::debug!("add_import: {} (already present)", file);
        return Response::success(
            &req.id,
            serde_json::json!({
                "file": file,
                "added": false,
                "module": module,
                "already_present": true,
            }),
        );
    }

    // --- Determine group ---
    // C/C++ grouping depends on the include delimiter (system `<>` -> Stdlib,
    // local `""` -> External), but the registry classifier only sees the bare
    // header path and always returns Stdlib. Use the resolved include kind so a
    // local include lands in its own group after system includes, matching
    // organize's ordering instead of alphabetically colliding with system ones.
    let group = match (lang, import_kind.as_deref()) {
        (LangId::C | LangId::Cpp, Some(kind)) => imports::classify_group_c_import_kind(kind),
        _ => imports::classify_group(lang, module),
    };

    // --- Try to merge into an existing same-module, same-kind named import ---
    //
    // When adding `{ baz }` to a file that already has `import { foo } from "lib"`,
    // the historical behavior inserted a second `import { baz } from "lib"` line
    // (TS/JS, Rust, Py). That produces duplicate imports the linter then complains
    // about, and the agent has to call `organize` afterwards to clean up.
    //
    // Instead, when the target module already has a value/type import statement of
    // the matching kind with named specifiers, merge `names` into that statement's
    // existing names (deduped + sorted) and replace its byte range. This only
    // applies to languages where named imports are a list inside one statement —
    // Go's "import (...)" block is handled separately by the insertion path.
    let target_kind = if type_only {
        imports::ImportKind::Type
    } else {
        imports::ImportKind::Value
    };
    let merge_target = if !names.is_empty()
        && default_import.is_none()
        && matches!(
            lang,
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Python | LangId::Rust
        ) {
        block.imports.iter().find(|imp| {
            let es_import = matches!(
                lang,
                LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue
            );
            imp.module_path == module
                && imp.kind == target_kind
                && imp.namespace_import.is_none()
                && (es_import || imp.default_import.is_none())
                && (es_import || !imp.names.is_empty())
                && (lang != LangId::Python
                    || matches!(
                        imp.form,
                        imports::ImportForm::Python {
                            from_import: true,
                            ..
                        }
                    ))
        })
    } else {
        None
    };

    let namespace_merge_target = if names.is_empty()
        && default_import.is_some()
        && namespace.is_some()
        && matches!(
            lang,
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue
        ) {
        block.imports.iter().find(|imp| {
            imp.module_path == module
                && imp.kind == target_kind
                && imp.names.is_empty()
                && imp.default_import.as_deref() == default_import.as_deref()
                && imp.namespace_import.is_none()
        })
    } else {
        None
    };

    let (insert_offset, replace_end, insert_text, merged_into_existing) = if let Some(existing) =
        merge_target
    {
        // Build the merged named-import list: union of existing + new, sorted.
        let merged_names = merge_named_import_specifiers(&existing.names, &names);

        let additions: Vec<String> = names
            .iter()
            .filter(|addition| {
                !existing.names.iter().any(|current| {
                    imports::specifier_imported_name(current)
                        == imports::specifier_imported_name(addition)
                        && imports::specifier_local_name(current)
                            == imports::specifier_local_name(addition)
                })
            })
            .cloned()
            .collect();
        let merged_line = import_specifier_edit::insert_named_specifiers(
            &existing.raw_text,
            &existing.names,
            &additions,
            lang,
        )
        .unwrap_or_else(|| {
            imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                lang,
                &existing.module_path,
                &merged_names,
                existing.default_import.as_deref(),
                existing.namespace_import.as_deref(),
                type_only,
                imports::es_import_attribute_clause(existing),
                Some(imports::quotes::statement_style(&existing.raw_text)),
            )
        });
        (
            existing.byte_range.start,
            existing.byte_range.end,
            merged_line,
            true,
        )
    } else if let Some(existing) = namespace_merge_target {
        let merged_line =
            imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                lang,
                &existing.module_path,
                &existing.names,
                existing.default_import.as_deref(),
                namespace.as_deref(),
                type_only,
                imports::es_import_attribute_clause(existing),
                Some(imports::quotes::statement_style(&existing.raw_text)),
            );
        (
            existing.byte_range.start,
            existing.byte_range.end,
            merged_line,
            true,
        )
    } else {
        // Fall through to the original "find insertion point and insert a new
        // statement" behavior, with two empty-block special cases that would
        // otherwise place the import at offset 0:
        //
        //  - Vue: offset 0 lands before `<template>`, outside the `<script>`
        //    block. Insert at the start of the script body instead.
        //  - Header languages (Go/Java/.../PHP): offset 0 lands before the
        //    `package`/`namespace`/`pragma`/`<?php` header — invalid code.
        //    Insert just after the header prologue instead.
        let bytes = source.as_bytes();
        let skip_one_newline = |mut at: usize| {
            if at < bytes.len() && bytes[at] == b'\r' {
                at += 1;
            }
            if at < bytes.len() && bytes[at] == b'\n' {
                at += 1;
            }
            at
        };
        let (insert_offset, needs_blank_before, needs_blank_after) = if !block.imports.is_empty() {
            let (off, blank_before, blank_after) =
                imports::find_insertion_point(&source, &block, group, module, type_only);
            // C/C++ `#include`s form one contiguous block (system group then
            // local group, no blank line between) — matching organize. Suppress
            // the cross-group blank lines find_insertion_point inserts for
            // languages like TS/Python/Rust where groups are blank-separated.
            if matches!(lang, LangId::C | LangId::Cpp) {
                (off, false, false)
            } else {
                (off, blank_before, blank_after)
            }
        } else if lang == LangId::Vue {
            match imports::vue_script_content_range(&tree) {
                Some((start, _end)) => (skip_one_newline(start), false, false),
                None => imports::find_insertion_point(&source, &block, group, module, type_only),
            }
        } else if let Some(anchor) = shebang_anchor(lang, &source) {
            let at = skip_one_newline(anchor);
            let next_is_newline = at < bytes.len() && (bytes[at] == b'\n' || bytes[at] == b'\r');
            (at, false, !next_is_newline && at < source.len())
        } else if let Some(anchor) = header_prologue_anchor(lang, &source, &tree) {
            let at = skip_one_newline(anchor);
            // Blank line before separates the import from the header. Blank line
            // after only when the next byte isn't already a newline, so a header
            // already followed by a blank line doesn't double up.
            let next_is_newline = at < bytes.len() && (bytes[at] == b'\n' || bytes[at] == b'\r');
            (at, true, !next_is_newline && at < source.len())
        } else if let Some(anchor) = spdx_license_anchor(lang, &source) {
            let at = skip_one_newline(anchor);
            let next_is_newline = at < bytes.len() && (bytes[at] == b'\n' || bytes[at] == b'\r');
            (at, false, !next_is_newline && at < source.len())
        } else {
            imports::find_insertion_point(&source, &block, group, module, type_only)
        };

        // For Go, check if we're inserting into a grouped import block
        let import_line = if matches!(lang, LangId::Go) {
            let in_group = imports::go_offset_is_in_grouped_import(&source, &tree, insert_offset);
            imports::generate_go_import_line_pub(module, default_import.as_deref(), in_group)
        } else if matches!(
            lang,
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue
        ) {
            // Import attributes select the module type. A new sibling import of
            // an already-attributed module must carry the same clause or runtimes
            // such as Node reject that sibling before loading the module.
            let mut clauses = block
                .imports
                .iter()
                .filter(|imp| imp.module_path == module)
                .filter_map(imports::es_import_attribute_clause);
            let inherited_clause = clauses
                .next()
                .filter(|first| clauses.all(|item| item == *first));
            imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                lang,
                module,
                &names,
                default_import.as_deref(),
                namespace.as_deref(),
                type_only,
                inherited_clause,
                Some(imports::quotes::preferred_style(
                    &source,
                    &tree,
                    lang,
                    &path,
                    ctx.callgraph_project_root().as_deref(),
                )),
            )
        } else {
            imports::generate_import(lang, &import_request)
        };

        // Build the text to insert
        let mut insert_text = String::new();
        if needs_blank_before {
            insert_text.push('\n');
        }
        insert_text.push_str(&import_line);
        insert_text.push('\n');
        if needs_blank_after {
            insert_text.push('\n');
        }
        (insert_offset, insert_offset, insert_text, false)
    };

    let _ = merged_into_existing;

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "add_import: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Insert (or replace, when merging) ---
    let new_source =
        match edit::replace_byte_range(&source, insert_offset, replace_end, &insert_text) {
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

    log::debug!("add_import: {}", file);

    // A rollback means post-write syntax validation failed and the file was
    // restored to its original content — the import was NOT added. Report that
    // honestly with an error instead of claiming success with `added: true`.
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "add_import: adding '{module}' to {file} would produce invalid syntax; file left unchanged"
            ),
        );
    }

    // --- Build response ---
    let mut result = serde_json::json!({
        "file": file,
        "added": true,
        "module": module,
        "group": group.label(),
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

fn shebang_anchor(lang: LangId, source: &str) -> Option<usize> {
    if !matches!(
        lang,
        LangId::TypeScript
            | LangId::JavaScript
            | LangId::Python
            | LangId::Ruby
            | LangId::Perl
            | LangId::Lua
    ) || !source.starts_with("#!")
    {
        return None;
    }

    Some(source.find('\n').unwrap_or(source.len()))
}

fn spdx_license_anchor(lang: LangId, source: &str) -> Option<usize> {
    if lang != LangId::Solidity {
        return None;
    }

    let first_line_end = source.find('\n').unwrap_or(source.len());
    let first_line = &source[..first_line_end];
    first_line
        .trim_start()
        .starts_with("// SPDX-License-Identifier:")
        .then_some(first_line_end)
}

/// For a file with a language-level header prologue (package/namespace/pragma
/// declarations, optionally preceded by comments) but no existing imports,
/// return the byte offset at the END of that prologue. A freshly added import
/// is then placed after the header instead of at offset 0, which would produce
/// invalid code (e.g. an import before a Go `package`, or before PHP's `<?php`).
///
/// Returns `None` for languages without a header-before-imports convention, or
/// when the file does not begin with a recognized prologue node. C# is
/// deliberately excluded: `using` directives before `namespace` are idiomatic.
///
/// `strong` kinds are header declarations whose end advances the anchor;
/// `skippable` kinds (comments, PHP open tag) are stepped over without
/// anchoring so a leading license comment alone never displaces the import.
fn header_prologue_anchor(lang: LangId, source: &str, tree: &tree_sitter::Tree) -> Option<usize> {
    let (strong, skippable): (&[&str], &[&str]) = match lang {
        LangId::Go => (&["package_clause"], &["comment"]),
        LangId::Java => (&["package_declaration"], &["comment"]),
        LangId::Kotlin => (&["package_header"], &["comment"]),
        LangId::Scala => (&["package_clause"], &["comment"]),
        LangId::Solidity => (&["pragma_directive"], &["comment"]),
        LangId::Php => (&["php_tag", "namespace_definition"], &["comment"]),
        LangId::Perl => (&["package_statement"], &["comment"]),
        _ => return None,
    };

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut anchor: Option<usize> = None;
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if strong.contains(&kind) {
            anchor = if lang == LangId::Php && kind == "namespace_definition" {
                php_braced_namespace_anchor(source, child).or(Some(child.end_byte()))
            } else {
                Some(child.end_byte())
            };
        } else if skippable.contains(&kind) {
            continue;
        } else {
            break;
        }
    }
    anchor
}

fn validate_module_path_for_add(lang: LangId, module: &str) -> Result<(), &'static str> {
    let module = module.trim();

    if uses_path_module_validation(lang) {
        if is_absolute_module_path(module) {
            return Err("absolute paths are not allowed for this language");
        }
        return Ok(());
    }

    if uses_namespace_module_validation(lang) {
        if module.starts_with('/') || module.contains('/') {
            return Err("filesystem paths are not allowed for this language");
        }
        if (lang != LangId::Php && module.starts_with('\\')) || has_windows_drive_prefix(module) {
            return Err("absolute paths are not allowed for this language");
        }
        if lang != LangId::Php && module.contains('\\') {
            return Err("filesystem paths are not allowed for this language");
        }
        if module.contains("..") || contains_parent_path_segment(module) {
            return Err("parent path traversal segments are not allowed for this language");
        }
    }

    Ok(())
}

fn uses_path_module_validation(lang: LangId) -> bool {
    matches!(lang, LangId::C | LangId::Cpp | LangId::Solidity)
}

fn uses_namespace_module_validation(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Php | LangId::Java | LangId::Kotlin | LangId::Scala | LangId::CSharp
    )
}

fn is_absolute_module_path(module: &str) -> bool {
    module.starts_with('/') || module.starts_with('\\') || has_windows_drive_absolute_path(module)
}

fn has_windows_drive_absolute_path(module: &str) -> bool {
    let bytes = module.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn has_windows_drive_prefix(module: &str) -> bool {
    let bytes = module.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn contains_parent_path_segment(module: &str) -> bool {
    module
        .split(|ch| ch == '/' || ch == '\\')
        .any(|segment| segment == "..")
}

fn php_braced_namespace_anchor(source: &str, node: tree_sitter::Node<'_>) -> Option<usize> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "{" {
                return Some(child.end_byte());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    source[node.start_byte()..node.end_byte()]
        .find('{')
        .map(|offset| node.start_byte() + offset + 1)
}

#[cfg(test)]
mod tests {
    use super::coerce_string_array_param;
    use serde_json::json;

    #[test]
    fn coerce_accepts_real_array() {
        let params = json!({ "names": ["a", "b"] });
        assert_eq!(
            coerce_string_array_param(&params, "names").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn coerce_parses_json_stringified_array() {
        // The live bug: an MCP/model boundary delivered names as the STRING
        // '["bashToolDescription"]'. The old `.as_array()` parse silently
        // dropped it and add_import wrote a bare side-effect import.
        let params = json!({ "names": "[\"bashToolDescription\"]" });
        assert_eq!(
            coerce_string_array_param(&params, "names").unwrap(),
            vec!["bashToolDescription".to_string()]
        );
    }

    #[test]
    fn coerce_treats_bare_string_as_single_name() {
        let params = json!({ "names": "useState" });
        assert_eq!(
            coerce_string_array_param(&params, "names").unwrap(),
            vec!["useState".to_string()]
        );
    }

    #[test]
    fn coerce_missing_null_and_empty_yield_empty() {
        assert!(coerce_string_array_param(&json!({}), "names")
            .unwrap()
            .is_empty());
        assert!(
            coerce_string_array_param(&json!({ "names": null }), "names")
                .unwrap()
                .is_empty()
        );
        assert!(coerce_string_array_param(&json!({ "names": "" }), "names")
            .unwrap()
            .is_empty());
        assert!(coerce_string_array_param(&json!({ "names": [] }), "names")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn coerce_rejects_non_string_non_array_types() {
        let err = coerce_string_array_param(&json!({ "names": 42 }), "names").unwrap_err();
        assert!(err.contains("must be an array of strings"), "{err}");
        assert!(coerce_string_array_param(&json!({ "names": { "a": 1 } }), "names").is_err());
    }
}
