//! Handler for the `organize_imports` command: re-group, sort, deduplicate, and
//! optionally merge imports in a file.
//!
//! For all languages: extracts imports, groups by convention, sorts alphabetically
//! within groups, deduplicates, and regenerates the import block with blank-line
//! separators between groups.
//!
//! For Rust: merges separate `use` declarations sharing a common prefix into
//! `use` trees (e.g. `use std::path::Path;` + `use std::path::PathBuf;` →
//! `use std::path::{Path, PathBuf};`). This implements D045's deferred merging.

use std::collections::BTreeMap;
use std::path::Path;

use crate::context::AppContext;
use crate::edit;
use crate::imports::{self, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `organize_imports` request.
///
/// Params:
///   - `file` (string, required) — target file path
///
/// Returns: `{ file, groups: [{name, count}], removed_duplicates, syntax_valid?, backup_id? }`
pub fn handle_organize_imports(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "organize_imports: missing required param 'file'",
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
            format!("organize_imports: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "organize_imports: unsupported file extension: {}",
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
            "invalid_request",
            format!(
                "organize_imports: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    // --- Parse file and imports ---
    let (source, _tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if block.imports.is_empty() {
        log::debug!("organize_imports: {} (no imports)", file);
        return Response::success(
            &req.id,
            serde_json::json!({
                "file": file,
                "groups": [],
                "removed_duplicates": 0,
            }),
        );
    }

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "organize_imports: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Organize: group, sort, dedup ---
    let original_count = block.imports.len();
    let (grouped, removed_duplicates) = organize(&block.imports, lang);

    // --- Generate new import block ---
    let grouped_go_range = if matches!(lang, LangId::Go) {
        imports::go_has_grouped_import(&source, &_tree)
    } else {
        None
    };
    let new_import_text = if matches!(lang, LangId::Go) && grouped_go_range.is_some() {
        generate_go_grouped_block(&grouped)
    } else {
        generate_organized_block(&grouped, lang)
    };

    // --- Replace import region ---
    let import_range = match grouped_go_range.as_ref().or(block.byte_range.as_ref()) {
        Some(range) => range,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!(
                    "organize_imports: missing import byte range for {} despite parsed imports",
                    file
                ),
            );
        }
    };
    let new_source = format!(
        "{}{}{}",
        &source[..import_range.start],
        new_import_text,
        &source[import_range.end..],
    );

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

    log::debug!("organize_imports: {}", file);

    // --- Build response ---
    let groups_info: Vec<serde_json::Value> = grouped
        .iter()
        .map(|(group, imps)| {
            serde_json::json!({
                "name": group.label(),
                "count": imps.len(),
            })
        })
        .collect();

    let _ = original_count; // used for removed_duplicates calculation above

    let mut result = serde_json::json!({
        "file": file,
        "groups": groups_info,
        "removed_duplicates": removed_duplicates,
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

/// Organize imports: group by convention, sort within groups, deduplicate.
/// Returns (grouped imports in order, count of removed duplicates).
fn organize(
    imports: &[ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    // Group imports
    let mut groups: BTreeMap<ImportGroup, Vec<&ImportStatement>> = BTreeMap::new();
    for imp in imports {
        groups.entry(imp.group).or_default().push(imp);
    }

    let mut result: Vec<(ImportGroup, Vec<OrganizedImport>)> = Vec::new();
    let mut total_removed = 0;

    for (group, imps) in &groups {
        let (organized, removed) = if matches!(lang, LangId::Rust) {
            organize_rust_group(imps)
        } else {
            organize_generic_group(imps, lang)
        };
        total_removed += removed;
        if !organized.is_empty() {
            result.push((*group, organized));
        }
    }

    (result, total_removed)
}

/// An organized import ready for code generation.
#[derive(Debug, Clone)]
struct OrganizedImport {
    module_path: String,
    names: Vec<String>,
    default_import: Option<String>,
    namespace_import: Option<String>,
    kind: ImportKind,
}

/// Organize a group of non-Rust imports: sort by module path, deduplicate.
fn organize_generic_group(
    imps: &[&ImportStatement],
    _lang: LangId,
) -> (Vec<OrganizedImport>, usize) {
    use std::collections::HashSet;

    let mut seen: HashSet<String> = HashSet::new();
    let mut organized: Vec<OrganizedImport> = Vec::new();
    let mut removed = 0;

    let mut side_effects: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind == ImportKind::SideEffect)
        .collect();
    let mut sorted: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind != ImportKind::SideEffect)
        .collect();
    sorted.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Side-effect imports are evaluation-order sensitive. Keep their original
    // relative source order as a pinned subgroup before value/type imports.
    side_effects.extend(sorted);

    for imp in side_effects {
        // Build dedup key: module_path + kind + sorted names + default + namespace.
        // Namespace imports introduce local bindings, so different aliases are
        // distinct and side-effect imports are not duplicates of namespace
        // imports from the same module.
        let names_key = {
            let mut n = imp.names.clone();
            sort_named_specifiers(&mut n);
            n.join(",")
        };
        let dedup_key = format!(
            "{}|{:?}|{}|{}|{}",
            imp.module_path,
            imp.kind,
            names_key,
            imp.default_import.as_deref().unwrap_or(""),
            imp.namespace_import.as_deref().unwrap_or("")
        );

        if seen.contains(&dedup_key) {
            removed += 1;
            continue;
        }
        seen.insert(dedup_key);

        let mut names = imp.names.clone();
        sort_named_specifiers(&mut names);

        organized.push(OrganizedImport {
            module_path: imp.module_path.clone(),
            names,
            default_import: imp.default_import.clone(),
            namespace_import: imp.namespace_import.clone(),
            kind: imp.kind,
        });
    }

    (organized, removed)
}

fn sort_named_specifiers(names: &mut [String]) {
    names.sort_by(|a, b| {
        imports::specifier_imported_name(a)
            .cmp(imports::specifier_imported_name(b))
            .then_with(|| a.cmp(b))
    });
}

/// Organize Rust use declarations: sort, deduplicate, and merge common prefixes.
fn organize_rust_group(imps: &[&ImportStatement]) -> (Vec<OrganizedImport>, usize) {
    use std::collections::BTreeMap as BMap;

    // First pass: collect all use paths. For items like `use std::path::Path;`,
    // extract prefix `std::path` and item `Path`. For items like `use serde::{Deserialize, Serialize}`,
    // keep as-is (already a tree).
    #[derive(Debug)]
    struct UsePath {
        /// Full original module_path (e.g. "std::path::Path" or "serde::{Deserialize, Serialize}")
        full_path: String,
        /// Prefix for merging (e.g. "std::path")
        prefix: Option<String>,
        /// Leaf item(s) for merging (e.g. ["Path"])
        items: Vec<String>,
        kind: ImportKind,
        is_pub: bool,
    }

    let mut paths: Vec<UsePath> = Vec::new();
    let mut removed = 0;

    for imp in imps {
        let is_pub = imp.default_import.as_deref() == Some("pub");
        let mp = &imp.module_path;

        // Check if this already has a use list (contains '{')
        if mp.contains('{') {
            // Already a tree like "serde::{Deserialize, Serialize}"
            // Extract prefix and items
            if let Some(brace_pos) = mp.find("::{") {
                let prefix = mp[..brace_pos].to_string();
                let items_str = &mp[brace_pos + 3..mp.len() - 1]; // strip ::{ and }
                let items: Vec<String> = items_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: Some(prefix),
                    items,
                    kind: imp.kind,
                    is_pub,
                });
            } else {
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: None,
                    items: vec![],
                    kind: imp.kind,
                    is_pub,
                });
            }
        } else if let Some(last_sep) = mp.rfind("::") {
            // Simple path like "std::path::Path" → prefix "std::path", item "Path"
            let prefix = mp[..last_sep].to_string();
            let item = mp[last_sep + 2..].to_string();
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: Some(prefix),
                items: vec![item],
                kind: imp.kind,
                is_pub,
            });
        } else {
            // Single-segment like "serde" — no prefix to merge on
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: None,
                items: vec![],
                kind: imp.kind,
                is_pub,
            });
        }
    }

    // Group by (prefix, kind, is_pub) for merging
    // key: (prefix, kind_discriminant, is_pub)
    let mut merge_groups: BMap<(String, u8, bool), Vec<String>> = BMap::new();
    let mut no_prefix: Vec<OrganizedImport> = Vec::new();

    for up in &paths {
        if let Some(ref prefix) = up.prefix {
            let kind_d = match up.kind {
                ImportKind::Value => 0,
                ImportKind::Type => 1,
                ImportKind::SideEffect => 2,
            };
            let key = (prefix.clone(), kind_d, up.is_pub);
            let entry = merge_groups.entry(key).or_default();
            for item in &up.items {
                if !entry.contains(item) {
                    entry.push(item.clone());
                } else {
                    removed += 1;
                }
            }
        } else {
            // Check for duplicate
            let already = no_prefix.iter().any(|o| {
                o.module_path == up.full_path
                    && o.kind == up.kind
                    && (o.default_import.as_deref() == Some("pub")) == up.is_pub
            });
            if already {
                removed += 1;
            } else {
                no_prefix.push(OrganizedImport {
                    module_path: up.full_path.clone(),
                    names: vec![],
                    default_import: if up.is_pub {
                        Some("pub".to_string())
                    } else {
                        None
                    },
                    namespace_import: None,
                    kind: up.kind,
                });
            }
        }
    }

    // Convert merge groups into OrganizedImport entries
    let mut organized: Vec<OrganizedImport> = Vec::new();

    for ((prefix, kind_d, is_pub), mut items) in merge_groups {
        items.sort();
        let kind = match kind_d {
            1 => ImportKind::Type,
            2 => ImportKind::SideEffect,
            _ => ImportKind::Value,
        };

        let module_path = if items.len() == 1 {
            // Single item — no braces needed
            format!("{}::{}", prefix, items[0])
        } else {
            // Multiple items — use tree
            format!("{}::{{{}}}", prefix, items.join(", "))
        };

        organized.push(OrganizedImport {
            module_path,
            names: vec![],
            default_import: if is_pub {
                Some("pub".to_string())
            } else {
                None
            },
            namespace_import: None,
            kind,
        });
    }

    // Add no-prefix items and sort everything by module_path
    organized.extend(no_prefix);
    organized.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Track how many original imports were merged away
    let final_count = organized.len();
    let original_count = imps.len();
    if original_count > final_count + removed {
        removed = original_count - final_count;
    }

    (organized, removed)
}

/// Generate the full organized import block text.
fn generate_organized_block(
    grouped: &[(ImportGroup, Vec<OrganizedImport>)],
    lang: LangId,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    for (_, imps) in grouped {
        let mut lines: Vec<String> = Vec::new();
        for imp in imps {
            let line = generate_organized_line(imp, lang);
            lines.push(line);
        }
        parts.push(lines.join("\n"));
    }

    parts.join("\n\n")
}

fn generate_go_grouped_block(grouped: &[(ImportGroup, Vec<OrganizedImport>)]) -> String {
    let mut lines = Vec::new();
    lines.push("import (".to_string());
    for (group_idx, (_, imps)) in grouped.iter().enumerate() {
        if group_idx > 0 {
            lines.push(String::new());
        }
        for imp in imps {
            if let Some(ref alias) = imp.default_import {
                lines.push(format!("\t{} \"{}\"", alias, imp.module_path));
            } else {
                lines.push(format!("\t\"{}\"", imp.module_path));
            }
        }
    }
    lines.push(")".to_string());
    lines.join("\n")
}

/// Generate a single import line from an OrganizedImport.
fn generate_organized_line(imp: &OrganizedImport, lang: LangId) -> String {
    match lang {
        LangId::Rust => {
            let prefix = if imp.default_import.as_deref() == Some("pub") {
                "pub "
            } else {
                ""
            };
            format!("{}use {};", prefix, imp.module_path)
        }
        LangId::Go => {
            // Go organize: regenerate as standalone imports
            // (organize_imports for Go would need grouped import rewrite — keep simple for now)
            if let Some(ref alias) = imp.default_import {
                format!("import {} \"{}\"", alias, imp.module_path)
            } else {
                format!("import \"{}\"", imp.module_path)
            }
        }
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript
            if imp.names.is_empty()
                && imp.default_import.is_none()
                && imp.namespace_import.is_some() =>
        {
            let namespace = imp.namespace_import.as_deref().unwrap_or_default();
            format!("import * as {} from '{}';", namespace, imp.module_path)
        }
        _ => {
            // TS/JS/TSX/Python — use the standard generator
            imports::generate_import_line(
                lang,
                &imp.module_path,
                &imp.names,
                imp.default_import.as_deref(),
                imp.kind == ImportKind::Type,
            )
        }
    }
}
