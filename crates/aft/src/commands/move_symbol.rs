//! Handler for the `move_symbol` command: move a top-level symbol from one file
//! to another with full import rewiring across all consumer files.
//!
//! Flow: resolve symbol → verify top-level → checkpoint → discover consumers
//! → extract symbol text → remove from source → add to destination → rewrite
//! imports in every consumer → format/validate all files → return results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::backup::CapturedRegularFile;
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::edit;
use crate::imports;
use crate::lsp_hints;
use crate::parser::{detect_language, grammar_for, parse_source_with_cached_parser, LangId};
use crate::protocol::{RawRequest, Response};
use crate::symbols::SymbolKind;

/// Handle a `move_symbol` request.
///
/// Params:
///   - `file` (string, required) — source file containing the symbol
///   - `symbol` (string, required) — name of the symbol to move
///   - `destination` (string, required) — target file path
///   - `scope` (string, optional) — scope qualifier for disambiguation
///
/// On success: `{ ok, files_modified, consumers_updated, checkpoint_name,
///   results: [{ file, syntax_valid, formatted }] }`
/// On failure after partial write: `{ error with failed_file, rolled_back }`
pub fn handle_move_symbol(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();

    // --- Extract and validate params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_symbol: missing required param 'file'",
            );
        }
    };

    let symbol_name = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_symbol: missing required param 'symbol'",
            );
        }
    };

    let destination = match req.params.get("destination").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_symbol: missing required param 'destination'",
            );
        }
    };

    let scope = req.params.get("scope").and_then(|v| v.as_str());

    let source_path_raw = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let dest_path_raw = match ctx.validate_path(&req.id, Path::new(destination)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    if !source_path_raw.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("source file not found: {}", file),
        );
    }

    // Canonicalize paths to match callgraph's canonicalized paths
    // (on macOS, /var/folders → /private/var/folders)
    let source_canon =
        std::fs::canonicalize(&source_path_raw).unwrap_or_else(|_| source_path_raw.clone());
    let dest_canon = if dest_path_raw.exists() {
        std::fs::canonicalize(&dest_path_raw).unwrap_or_else(|_| dest_path_raw.clone())
    } else if let Some(parent) = dest_path_raw.parent() {
        // Destination may not exist yet — canonicalize its parent
        let canon_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        canon_parent.join(dest_path_raw.file_name().unwrap_or_default())
    } else {
        dest_path_raw.clone()
    };
    let source_path: &Path = &source_canon;
    let dest_path: &Path = &dest_canon;

    if source_path == dest_path {
        return Response::error(
            &req.id,
            "invalid_request",
            "move_symbol: source and destination are the same file",
        );
    }

    let source_lang = match detect_language(source_path) {
        Some(lang @ (LangId::TypeScript | LangId::Tsx | LangId::JavaScript)) => lang,
        Some(lang) => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "move_symbol currently supports TypeScript/JavaScript only; got {:?}",
                    lang
                ),
            );
        }
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                "move_symbol currently supports TypeScript/JavaScript only; got unknown",
            );
        }
    };

    match detect_language(dest_path) {
        Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript) => {}
        Some(lang) => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "move_symbol currently supports TypeScript/JavaScript only; got {:?}",
                    lang
                ),
            );
        }
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                "move_symbol currently supports TypeScript/JavaScript only; got unknown",
            );
        }
    }

    // --- Resolve symbol ---
    let matches = match ctx.provider().resolve_symbol(&source_path, symbol_name) {
        Ok(m) => m,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Disambiguation
    let filtered = if matches.len() > 1 {
        if let Some(scope_filter) = scope {
            matches
                .into_iter()
                .filter(|m| {
                    m.symbol.scope_chain.iter().any(|s| s == scope_filter)
                        || m.symbol.parent.as_deref() == Some(scope_filter)
                })
                .collect()
        } else {
            matches
        }
    } else {
        matches
    };

    // LSP-enhanced disambiguation (S03)
    let filtered = if let Some(hints) = lsp_hints::parse_lsp_hints(req) {
        lsp_hints::apply_lsp_disambiguation(filtered, &hints)
    } else {
        filtered
    };

    if filtered.is_empty() {
        return Response::error(
            &req.id,
            "symbol_not_found",
            format!("symbol '{}' not found in {}", symbol_name, file),
        );
    }

    if filtered.len() > 1 {
        let candidates: Vec<serde_json::Value> = filtered
            .iter()
            .map(|m| {
                let sym = &m.symbol;
                let qualified = if sym.scope_chain.is_empty() {
                    sym.name.clone()
                } else {
                    format!("{}::{}", sym.scope_chain.join("::"), sym.name)
                };
                let kind_str = serde_json::to_value(&sym.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("{:?}", sym.kind).to_lowercase());
                serde_json::json!({
                    "name": sym.name,
                    "qualified": qualified,
                    "line": sym.range.start_line + 1,
                    "kind": kind_str,
                })
            })
            .collect();

        return Response::error_with_data(
            &req.id,
            "ambiguous_symbol",
            format!("symbol '{}' is ambiguous in {}", symbol_name, file),
            serde_json::json!({ "candidates": candidates }),
        );
    }

    let target = &filtered[0].symbol;

    // --- Top-level guard (D100) ---
    if !target.scope_chain.is_empty() || target.kind == SymbolKind::Method {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "move_symbol: cannot move non-top-level symbol '{}' (kind: {:?}, scope: [{}]). Only top-level declarations can be moved.",
                symbol_name,
                target.kind,
                target.scope_chain.join(", ")
            ),
        );
    }

    // --- Read source file ---
    let mut captures = HashMap::new();
    let source_content = match CapturedRegularFile::read_text(source_path) {
        Ok(Some((capture, source))) => {
            captures.insert(source_path.to_path_buf(), capture);
            source
        }
        Ok(None) => match std::fs::read_to_string(source_path) {
            Ok(source) => source,
            Err(error) => {
                return Response::error(&req.id, "file_not_found", format!("{}: {}", file, error));
            }
        },
        Err(error) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, error));
        }
    };

    // --- Extract symbol text from source ---
    let raw_start_byte = edit::line_col_to_byte(
        &source_content,
        target.range.start_line,
        target.range.start_col,
    );
    let end_byte =
        edit::line_col_to_byte(&source_content, target.range.end_line, target.range.end_col);

    // For TS/JS, the parser reports the symbol range starting at the inner
    // declaration (e.g. `function greet(...)`) not the wrapping `export
    // statement`. If we use raw_start_byte directly, two bugs follow:
    //   1. The destination loses its `export` keyword (we'd then re-add it
    //      via `prepare_exported_symbol`, which DOES work for the destination).
    //   2. The source removal leaves the trailing `export ` behind, which
    //      then attaches to the next declaration when blank-line cleanup
    //      collapses the gap. Repro: moving `greet` out of
    //         `export function greet(...) {}\n\nfunction other(): number {}\n`
    //      produced `export function other(): number {}` in the source.
    //
    // Fix: when target.exported is true, expand start_byte backwards to
    // include the `export` keyword (and `default` if present). We walk over
    // whitespace then look for the literal token, which is robust because the
    // parser already told us this declaration IS exported — there must be an
    // `export` keyword somewhere immediately before it.
    let start_byte = if target.exported
        && matches!(
            source_lang,
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript
        ) {
        find_export_keyword_start(&source_content, raw_start_byte).unwrap_or(raw_start_byte)
    } else {
        raw_start_byte
    };

    let symbol_text = match source_content.get(start_byte..end_byte) {
        Some(symbol_text) => symbol_text,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "move_symbol: symbol byte range [{}..{}) is not on UTF-8 boundaries",
                    start_byte, end_byte
                ),
            );
        }
    };

    // Prepare the text to add to destination: ensure it has export prefix.
    // When start_byte was extended above, the symbol_text already includes
    // `export`; prepare_exported_symbol's idempotency check leaves it alone.
    let moved_symbol_is_default = symbol_text.trim_start().starts_with("export default");
    let dest_symbol_text = prepare_exported_symbol(symbol_text);

    // Prepare source with symbol removed
    let mut new_source = match remove_symbol_from_source(&source_content, start_byte, end_byte) {
        Ok(s) => s,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    // --- Read destination file (may not exist yet) ---
    let dest_content = if dest_path.exists() {
        match CapturedRegularFile::read_text(dest_path) {
            Ok(Some((capture, content))) => {
                captures.insert(dest_path.to_path_buf(), capture);
                content
            }
            Ok(None) => std::fs::read_to_string(dest_path).unwrap_or_default(),
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    // Prepare new destination content
    let new_dest = append_symbol_to_dest(&dest_content, &dest_symbol_text);

    // --- Discover consumers through the persisted callgraph store (best-effort) ---
    // move_symbol only supports TS/JS source and destination, and
    // `collect_ts_js_files` (below) independently brute-walks every TS/JS file as
    // a candidate consumer for import rewriting. The store query is therefore a
    // best-effort enrichment that can only surface NON-TS/JS callers, which the
    // TS/JS import rewriter cannot act on. A still-building, unavailable, or
    // errored store is never a reason to refuse the move: we proceed with no
    // extra consumers and let the TS/JS brute-walk do the rewriting. Only a
    // project with no resolvable root is fatal.
    let (project_root, consumers) = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => {
            let rel_source = source_path
                .strip_prefix(store.project_root())
                .unwrap_or(source_path);
            let sites = store
                .node_for(rel_source, symbol_name)
                .and_then(|node| store.direct_callers_of(Path::new(&node.file), &node.symbol))
                .unwrap_or_default();
            (store.project_root().to_path_buf(), sites)
        }
        _ => {
            let project_root = ctx.canonical_cache_root_opt().or_else(|| {
                ctx.config()
                    .project_root
                    .clone()
                    .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
            });
            let Some(project_root) = project_root else {
                return Response::error(
                    &req.id,
                    "not_configured",
                    "move_symbol: project not configured — send 'configure' first",
                );
            };
            (project_root, Vec::new())
        }
    };

    // Collect consumer files that need import rewriting.
    let mut consumer_files: Vec<PathBuf> = consumers
        .iter()
        .map(|site| {
            let p = PathBuf::from(&site.caller.file);
            if p.is_absolute() {
                p
            } else {
                project_root.join(&p)
            }
        })
        .filter(|p| p != source_path && p != dest_path)
        .collect();
    collect_ts_js_files(&project_root, &mut consumer_files, source_path, dest_path);
    consumer_files.sort();
    consumer_files.dedup();

    // `lang` already detected above for the export-keyword extension.

    // Compute every consumer change before creating backups or writing files.
    // Namespace imports need semantic usage checks: clean property accesses can
    // be rewritten, while dynamic or value-level uses must reject the whole move.
    let mut unsupported_consumer_usages: Vec<(PathBuf, Vec<String>)> = Vec::new();

    // If the original source still references the moved symbol after removing
    // its declaration, it is also a consumer. Run the same import-rewrite path
    // against the post-removal source text so those local references resolve.
    let mut source_rewritten_as_consumer = false;
    match rewrite_consumer_imports(
        &new_source,
        source_path,
        source_path,
        dest_path,
        symbol_name,
        Some(source_lang),
        moved_symbol_is_default,
        &project_root,
    ) {
        Ok(Some(rewritten)) => {
            new_source = rewritten;
            source_rewritten_as_consumer = true;
        }
        Ok(None) => {}
        Err(usages) => unsupported_consumer_usages.push((source_path.to_path_buf(), usages)),
    }

    // --- Compute consumer rewrites ---
    let mut consumer_rewrites: Vec<(PathBuf, String, String)> = Vec::new(); // (path, original, new)
    for consumer_file in &consumer_files {
        if !consumer_file.exists() {
            continue;
        }
        let (consumer_content, consumer_capture) =
            match CapturedRegularFile::read_text(consumer_file) {
                Ok(Some((capture, content))) => (content, Some(capture)),
                Ok(None) => match std::fs::read_to_string(consumer_file) {
                    Ok(content) => (content, None),
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

        match rewrite_consumer_imports(
            &consumer_content,
            consumer_file,
            source_path,
            dest_path,
            symbol_name,
            detect_language(consumer_file),
            moved_symbol_is_default,
            &project_root,
        ) {
            Ok(Some(rewritten)) => {
                if let Some(capture) = consumer_capture {
                    captures.insert(consumer_file.clone(), capture);
                }
                consumer_rewrites.push((consumer_file.clone(), consumer_content, rewritten));
            }
            Ok(None) => {}
            Err(usages) => {
                unsupported_consumer_usages.push((consumer_file.clone(), usages));
            }
        }
    }

    if !unsupported_consumer_usages.is_empty() {
        unsupported_consumer_usages.sort_by(|left, right| left.0.cmp(&right.0));
        let files: Vec<String> = unsupported_consumer_usages
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        let usages: Vec<serde_json::Value> = unsupported_consumer_usages
            .iter()
            .map(|(path, shapes)| {
                serde_json::json!({
                    "file": path.display().to_string(),
                    "shapes": shapes,
                })
            })
            .collect();
        return Response::error_with_data(
            &req.id,
            "unsupported_namespace_usage",
            format!(
                "move_symbol: cannot safely rewrite namespace usage in: {}",
                files.join(", ")
            ),
            serde_json::json!({ "files": files, "usages": usages }),
        );
    }

    // --- Create checkpoint (D105) ---
    let checkpoint_name = format!("move_symbol-{symbol_name}");
    {
        let mut all_files: Vec<PathBuf> = vec![source_path.to_path_buf()];
        if dest_path.exists() {
            all_files.push(dest_path.to_path_buf());
        }
        for (path, _, _) in &consumer_rewrites {
            all_files.push(path.clone());
        }

        let backup_store = ctx.backup().lock();
        let mut cp_store = ctx.checkpoint().lock();
        if let Err(e) = cp_store.create_from_captures(
            req.session(),
            &checkpoint_name,
            all_files,
            &backup_store,
            &mut captures,
        ) {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    }

    let mut backup_ids: Vec<String> = Vec::new();
    let dest_existed = dest_path.exists();
    {
        let mut files_to_backup: Vec<PathBuf> = vec![source_path.to_path_buf()];
        if dest_existed {
            files_to_backup.push(dest_path.to_path_buf());
        }
        for (path, _, _) in &consumer_rewrites {
            files_to_backup.push(path.clone());
        }
        files_to_backup.sort();
        files_to_backup.dedup();

        let mut backup_store = ctx.backup().lock();
        for path in files_to_backup {
            let backup_result = if let Some(capture) = captures.get(&path) {
                backup_store.snapshot_with_op_from_capture(
                    req.session(),
                    &path,
                    "move_symbol: pre-move backup",
                    Some(&op_id),
                    capture,
                )
            } else {
                backup_store.snapshot_with_op(
                    req.session(),
                    &path,
                    "move_symbol: pre-move backup",
                    Some(&op_id),
                )
            };
            match backup_result {
                Ok(Some(id)) => backup_ids.push(id),
                Ok(None) => {}
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            }
        }
        if !dest_existed {
            match backup_store.snapshot_op_tombstone(
                req.session(),
                &op_id,
                dest_path,
                "move_symbol: destination file created during move",
            ) {
                Ok(Some(id)) => backup_ids.push(id),
                Ok(None) => {}
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            }
        }
    }

    // --- Apply mutations ---
    // Track files for rollback
    let mut written_files: Vec<PathBuf> = Vec::new();
    let mut new_files: Vec<PathBuf> = Vec::new();
    let mut results: Vec<serde_json::Value> = Vec::new();

    // 1. Write source file (symbol removed)
    match edit::write_format_validate_deferred(
        &source_path,
        &new_source,
        &ctx.config(),
        &req.params,
    ) {
        // A rolled-back write means the result was invalid syntax and the file
        // was reverted (symbol NOT removed). Continuing would add the symbol to
        // the destination too, leaving it defined in BOTH files. Treat it like a
        // write failure: restore and bail.
        Ok(wr) if wr.rolled_back => {
            if restore_checkpoint(ctx, req.session(), &checkpoint_name) {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return move_error(
                &req.id,
                file,
                &written_files,
                &new_files,
                "removing the symbol from the source produced invalid syntax; the move was rolled back and nothing changed",
            );
        }
        Ok(wr) => {
            if let Ok(final_content) = std::fs::read_to_string(source_path) {
                ctx.lsp_notify_file_changed(source_path, &final_content);
            }

            written_files.push(source_path.to_path_buf());
            results.push(serde_json::json!({
                "file": file,
                "syntax_valid": wr.syntax_valid,
                "formatted": wr.formatted,
            }));
        }
        Err(e) => {
            if restore_checkpoint(ctx, req.session(), &checkpoint_name) {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return move_error(
                &req.id,
                file,
                &written_files,
                &new_files,
                &format!("failed to write source: {}", e),
            );
        }
    }

    // 2. Write destination file (symbol added)
    match edit::write_format_validate_deferred(&dest_path, &new_dest, &ctx.config(), &req.params) {
        // CRITICAL: the source already had the symbol removed. If the
        // destination write is rolled back (invalid syntax — e.g. moving
        // TS-only syntax into a .js file), the symbol would be defined NOWHERE —
        // silent data loss with success:true. Restore everything and fail, same
        // as the Err branch below.
        Ok(wr) if wr.rolled_back => {
            let mut files_to_delete = new_files.clone();
            if !dest_existed {
                files_to_delete.push(dest_path.to_path_buf());
            }
            let restored = restore_checkpoint(ctx, req.session(), &checkpoint_name);
            cleanup_new_files(&files_to_delete);
            if restored {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return move_error(
                &req.id,
                destination,
                &written_files,
                &new_files,
                "adding the symbol to the destination produced invalid syntax; the move was rolled back and nothing changed",
            );
        }
        Ok(wr) => {
            if let Ok(final_content) = std::fs::read_to_string(dest_path) {
                ctx.lsp_notify_file_changed(dest_path, &final_content);
            }

            if dest_existed {
                written_files.push(dest_path.to_path_buf());
            } else {
                new_files.push(dest_path.to_path_buf());
            }
            results.push(serde_json::json!({
                "file": destination,
                "syntax_valid": wr.syntax_valid,
                "formatted": wr.formatted,
            }));
        }
        Err(e) => {
            let mut files_to_delete = new_files.clone();
            if !dest_existed {
                files_to_delete.push(dest_path.to_path_buf());
            }
            let restored = restore_checkpoint(ctx, req.session(), &checkpoint_name);
            cleanup_new_files(&files_to_delete);
            if restored {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return move_error(
                &req.id,
                destination,
                &written_files,
                &files_to_delete,
                &format!("failed to write destination: {}", e),
            );
        }
    }

    // 3. Write consumer files (imports rewritten)
    let mut consumers_updated = usize::from(source_rewritten_as_consumer);
    for (path, _original, new_content) in &consumer_rewrites {
        match edit::write_format_validate_deferred(&path, new_content, &ctx.config(), &req.params) {
            // A rolled-back consumer rewrite leaves the move half-applied (this
            // consumer still imports from the old location while others were
            // updated). Restore everything and fail, same as the Err branch.
            Ok(wr) if wr.rolled_back => {
                let restored = restore_checkpoint(ctx, req.session(), &checkpoint_name);
                cleanup_new_files(&new_files);
                if restored {
                    ctx.backup()
                        .lock()
                        .discard_operation_entries(req.session(), &op_id);
                }
                return move_error(
                    &req.id,
                    &path.display().to_string(),
                    &written_files,
                    &new_files,
                    "rewriting a consumer's import produced invalid syntax; the move was rolled back and nothing changed",
                );
            }
            Ok(wr) => {
                if let Ok(final_content) = std::fs::read_to_string(&path) {
                    ctx.lsp_notify_file_changed(path, &final_content);
                }

                written_files.push(path.clone());
                consumers_updated += 1;
                results.push(serde_json::json!({
                    "file": path.display().to_string(),
                    "syntax_valid": wr.syntax_valid,
                    "formatted": wr.formatted,
                }));
            }
            Err(e) => {
                let restored = restore_checkpoint(ctx, req.session(), &checkpoint_name);
                cleanup_new_files(&new_files);
                if restored {
                    ctx.backup()
                        .lock()
                        .discard_operation_entries(req.session(), &op_id);
                }
                return move_error(
                    &req.id,
                    &path.display().to_string(),
                    &written_files,
                    &new_files,
                    &format!("failed to write consumer: {}", e),
                );
            }
        }
    }

    let paths: Vec<PathBuf> = std::iter::once(source_path.to_path_buf())
        .chain(std::iter::once(dest_path.to_path_buf()))
        .chain(consumer_rewrites.iter().map(|(path, _, _)| path.clone()))
        .collect();
    let validation = edit::validate_written_files(&paths, &ctx.config(), &req.params);
    if let Some(validation) = &validation {
        for (path, result) in paths.iter().zip(&mut results) {
            validation.append_to(path, result);
        }
    }
    let files_modified = results.len();

    log::debug!(
        "move_symbol: {} from {} to {} ({} consumers updated)",
        symbol_name,
        file,
        destination,
        consumers_updated
    );

    let mut result = serde_json::json!({
        "ok": true,
        "files_modified": files_modified,
        "consumers_updated": consumers_updated,
        "checkpoint_name": checkpoint_name,
        "backup_ids": backup_ids,
        "results": results,
    });
    if let Some(validation) = validation {
        result["output"] = serde_json::json!(validation.summary());
    }
    Response::success(&req.id, result)
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute a relative import path from `from_file` to `to_file`.
///
/// Given a consumer file and a target module file, computes the relative import
/// path suitable for TS/JS/TSX imports. Strips file extensions for TS/JS/TSX.
///
/// Examples:
/// - same dir: `./utils`
/// - parent dir: `../shared/utils`
/// - deeply nested: `../../lib/helpers`
pub fn compute_relative_import_path(from_file: &Path, to_file: &Path) -> String {
    // We want the path from from_file's directory to to_file
    let from_dir = from_file.parent().unwrap_or(Path::new(""));
    let to_dir = to_file.parent().unwrap_or(Path::new(""));
    let to_stem = to_file
        .file_stem()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("");

    // Compute relative path from from_dir to to_dir
    let rel_dir = compute_relative_dir(from_dir, to_dir);

    if rel_dir.is_empty() || rel_dir == "." {
        format!("./{}", to_stem)
    } else if rel_dir.starts_with("..") {
        format!("{}/{}", rel_dir, to_stem)
    } else {
        format!("./{}/{}", rel_dir, to_stem)
    }
}

/// Compute the relative directory path from `from` to `to`.
fn compute_relative_dir(from: &Path, to: &Path) -> String {
    // Normalize to components
    let from_parts: Vec<&str> = from
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let to_parts: Vec<&str> = to
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    // Find common prefix length
    let common_len = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let ups = from_parts.len() - common_len;
    let downs = &to_parts[common_len..];

    let mut parts: Vec<&str> = Vec::new();
    for _ in 0..ups {
        parts.push("..");
    }
    for d in downs {
        parts.push(d);
    }

    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Check if a path string refers to the same file as `target`, accounting for
/// relative path variations (with or without extension, ./ prefix, etc.).
fn import_path_matches_file(import_module: &str, consumer_file: &Path, target_file: &Path) -> bool {
    // Only handle relative imports
    if !import_module.starts_with('.') {
        return false;
    }

    let consumer_dir = consumer_file.parent().unwrap_or(Path::new(""));
    let resolved = consumer_dir.join(import_module);

    // Try exact match (with extension already in import path)
    if paths_equivalent(&resolved, target_file) {
        return true;
    }

    // Try adding common extensions
    for ext in &["ts", "tsx", "js", "jsx"] {
        let with_ext = resolved.with_extension(ext);
        if paths_equivalent(&with_ext, target_file) {
            return true;
        }
    }

    // Try index file pattern: import './dir' -> './dir/index.ts'
    let as_index = resolved.join("index");
    for ext in &["ts", "tsx", "js", "jsx"] {
        let with_ext = as_index.with_extension(ext);
        if paths_equivalent(&with_ext, target_file) {
            return true;
        }
    }

    false
}

/// Compare two paths for equivalence, normalizing components.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    let norm_a = normalize_path(a);
    let norm_b = normalize_path(b);
    norm_a == norm_b
}

/// Normalize a path by resolving `.` and `..` components without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<std::path::Component> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                if let Some(last) = parts.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        parts.pop();
                        continue;
                    }
                }
                parts.push(comp);
            }
            std::path::Component::CurDir => {} // skip
            _ => parts.push(comp),
        }
    }
    parts.iter().collect()
}

/// For TS/JS exported declarations, walk backwards from `decl_start` to find
/// the start of the `export` (or `export default`) keyword that the parser
/// reported as wrapping this declaration. Returns the byte offset of the `e`
/// in `export`, or `None` if no plausible export keyword is found within a
/// reasonable lookback (in which case the caller falls back to decl_start).
///
/// Why text-level: the symbol resolver already gave us `target.exported = true`
/// based on AST analysis (it walked export_statement parents). At this point
/// we trust that an `export` keyword exists immediately before the declaration
/// — we just need to locate its byte position to extend the cut range.
///
/// Handles:
///   - `export function f() {}`           — single keyword
///   - `export default function f() {}`   — `default` between export and decl
///   - `export\n  default\n  function f` — newlines/whitespace between tokens
///
/// Does NOT handle (returns None, falls back to decl_start):
///   - `export { foo } from '...'`        — re-exports (parser shouldn't mark
///     these as `exported: true` for the inner symbol anyway)
///   - Any case where lookback exceeds 200 bytes without finding `export`
fn find_export_keyword_start(source: &str, decl_start: usize) -> Option<usize> {
    if decl_start == 0 {
        return None;
    }
    let bytes = source.as_bytes();
    // Bound the lookback to keep this O(1) in practice. 200 bytes is enough
    // for `export default async function /* comment */` style headers without
    // scanning unrelated code on pathological inputs.
    let window_start = decl_start.saturating_sub(200);

    // Scan the window for the LAST `export` token whose start lands on a
    // word boundary (i.e. preceded by start-of-file, whitespace, or a
    // newline) and whose match is followed by whitespace or `default`.
    // Iterate from window.len()-6 down to 0 so we have room for the 6-byte
    // "export" match starting at i.
    let window = &bytes[window_start..decl_start];
    if window.len() < 6 {
        return None;
    }
    let mut i = window.len() - 6 + 1; // +1 because we decrement on entry
    while i > 0 {
        i -= 1;
        if window[i] == b'e' {
            // Try to match "export" starting here
            if window.get(i..i + 6) == Some(b"export") {
                // Word-boundary check on the LEFT (no identifier char before)
                let left_ok = i == 0
                    || !matches!(window[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$');
                // Word-boundary check on the RIGHT (whitespace/newline)
                let right_ok = window
                    .get(i + 6)
                    .is_some_and(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
                if left_ok && right_ok {
                    let abs = window_start + i;
                    // Verify there's only whitespace and/or `default` between
                    // the keyword end and decl_start. If something else lives
                    // there, this isn't the export keyword wrapping our decl.
                    let between = &bytes[abs + 6..decl_start];
                    let s = std::str::from_utf8(between).ok()?.trim();
                    if s.is_empty() || s == "default" {
                        return Some(abs);
                    }
                }
            }
        }
    }
    None
}

/// Prepare the symbol text for the destination file.
/// Ensures it has an `export` keyword prefix.
fn prepare_exported_symbol(symbol_text: &str) -> String {
    let trimmed = symbol_text.trim();

    // If it already starts with 'export', use as-is
    if trimmed.starts_with("export default")
        || trimmed.starts_with("export {")
        || trimmed.starts_with("export *")
        || trimmed.starts_with("export ")
    {
        return trimmed.to_string();
    }

    // Add export prefix
    format!("export {}", trimmed)
}

/// Remove a symbol from source content, cleaning up surrounding whitespace.
fn remove_symbol_from_source(
    source: &str,
    start_byte: usize,
    end_byte: usize,
) -> Result<String, crate::error::AftError> {
    // Extend backwards to include any preceding blank lines
    let mut actual_start = start_byte;

    // If the symbol starts with "export", we may need to look further back.
    // Walk backwards from start to find the beginning of the line
    while actual_start > 0 && source.as_bytes()[actual_start - 1] != b'\n' {
        actual_start -= 1;
    }

    // Check if the content before the symbol on this line is just whitespace
    let line_prefix = &source[actual_start..start_byte];
    if line_prefix.trim().is_empty() {
        // Use the line start
    } else {
        // There's meaningful content before it on this line, use original start
        actual_start = start_byte;
    }

    // Extend end to include trailing newline
    let mut actual_end = end_byte;
    let bytes = source.as_bytes();

    // Skip any trailing whitespace on the same line
    while actual_end < bytes.len() && (bytes[actual_end] == b' ' || bytes[actual_end] == b'\t') {
        actual_end += 1;
    }
    // Skip the newline
    if actual_end < bytes.len() && bytes[actual_end] == b'\n' {
        actual_end += 1;
    } else if actual_end < bytes.len() && bytes[actual_end] == b'\r' {
        actual_end += 1;
        if actual_end < bytes.len() && bytes[actual_end] == b'\n' {
            actual_end += 1;
        }
    }

    // Skip one additional blank line if present (to clean up double-spacing)
    let peek_end = actual_end;
    if peek_end < bytes.len() && bytes[peek_end] == b'\n' {
        actual_end = peek_end + 1;
    } else if peek_end < bytes.len() && bytes[peek_end] == b'\r' {
        actual_end = peek_end + 1;
        if actual_end < bytes.len() && bytes[actual_end] == b'\n' {
            actual_end += 1;
        }
    }

    edit::replace_byte_range(source, actual_start, actual_end, "")
}

/// Append a symbol to the destination file content.
fn append_symbol_to_dest(dest_content: &str, symbol_text: &str) -> String {
    if dest_content.is_empty() {
        format!("{}\n", symbol_text)
    } else {
        let trimmed_dest = dest_content.trim_end();
        format!("{}\n\n{}\n", trimmed_dest, symbol_text)
    }
}

/// Collect TS/JS/TSX files in the project to scan for references to the moved
/// symbol.
///
/// Respects `.gitignore` / `.aftignore` (so gitignored compiled output under
/// `dist/`/`build/` is NOT rewritten — the corruption bug this guards against)
/// and skips heavy non-source dirs (node_modules, target, .git, dist, build, …).
/// Unlike the shared `walk_project_files`, it deliberately includes HIDDEN dirs
/// (`.hidden(false)`), so tracked consumers in `.storybook/`, `.config/`, etc.
/// are still found and rewritten — otherwise the move leaves dangling imports in
/// them. The `dist`/`build` name-exclusion is kept (a deliberate prior decision:
/// un-gitignored compiled output stays untouched); a tracked *source* dir named
/// literally `build/` is a known, rare limitation.
fn collect_ts_js_files(root: &Path, out: &mut Vec<PathBuf>, source_path: &Path, dest_path: &Path) {
    use ignore::WalkBuilder;

    // Prevent a disappearing child mount from making ReadDir::drop abort on ENXIO.
    let walker = WalkBuilder::new(root)
        .same_file_system(true)
        .hidden(false) // include .storybook/, .config/, etc. (tracked consumers)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return !matches!(
                    entry.file_name().to_string_lossy().as_ref(),
                    "node_modules"
                        | "target"
                        | ".git"
                        | "__pycache__"
                        | ".tox"
                        | ".venv"
                        | "venv"
                        | "dist"
                        | "build"
                );
            }
            true
        })
        .build();

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if !matches!(
            detect_language(&path),
            Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript)
        ) {
            continue;
        }
        let canon = std::fs::canonicalize(&path).unwrap_or(path);
        if canon != source_path && canon != dest_path {
            out.push(canon);
        }
    }
}

/// Rewrite imports and supported namespace-member references to the destination.
/// Unsupported namespace usage is returned so the caller can reject before any write.
fn rewrite_consumer_imports(
    consumer_content: &str,
    consumer_file: &Path,
    source_file: &Path,
    dest_file: &Path,
    symbol_name: &str,
    lang: Option<LangId>,
    moved_symbol_is_default: bool,
    project_root: &Path,
) -> Result<Option<String>, Vec<String>> {
    let Some(lang) = lang else {
        return Ok(None);
    };

    // Only handle TS/JS/TSX for now (the primary use case).
    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        return Ok(None);
    }

    // Import and re-export syntax requires these literal keywords. A script
    // with neither cannot consume another file's export. The source itself is
    // exempt: its remaining local references can require a new import even
    // when it had no module declarations before the move.
    if !consumer_content.contains("import")
        && !consumer_content.contains("export")
        && !paths_equivalent(consumer_file, source_file)
    {
        return Ok(None);
    }

    // Parse imports from the content passed in. For the source file this is
    // the post-removal text, not the original on-disk content.
    let Some((tree, block)) = parse_imports_from_content(consumer_content, lang) else {
        return Ok(None);
    };

    let content = consumer_content;
    let new_import_path = compute_relative_import_path(consumer_file, dest_file);

    // Find imports from the source file that reference the moved symbol.
    let mut matching_imports: Vec<(usize, &imports::ImportStatement)> = block
        .imports
        .iter()
        .enumerate()
        .filter(|(_, imp)| import_path_matches_file(&imp.module_path, consumer_file, source_file))
        .collect();
    matching_imports.sort_by(|left, right| right.1.byte_range.start.cmp(&left.1.byte_range.start));

    let namespace_aliases = matching_imports
        .iter()
        .filter_map(|(_, import)| import.namespace_import.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let existing_moved_binding = (!moved_symbol_is_default)
        .then(|| {
            matching_imports.iter().find_map(|(_, import)| {
                (import.kind == imports::ImportKind::Value)
                    .then(|| {
                        import
                            .names
                            .iter()
                            .find(|name| imports::specifier_imported_name(name) == symbol_name)
                            .map(|name| imports::specifier_local_name(name).to_string())
                    })
                    .flatten()
            })
        })
        .flatten();
    let namespace_plan = build_namespace_rewrite_plan(
        content,
        &tree,
        &block,
        &namespace_aliases,
        consumer_file,
        dest_file,
        symbol_name,
        existing_moved_binding.as_deref(),
    )?;

    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    for (_, imp) in &matching_imports {
        let moved_bindings = moved_bindings_for_import(imp, symbol_name, moved_symbol_is_default);
        if moved_bindings.is_empty() {
            continue;
        }

        // Check if this import has other symbols besides the moved one. Default
        // imports are only moved when the target declaration was the default
        // export, preserving the consumer's local alias (`import Bar ...`).
        let remaining_names: Vec<String> = imp
            .names
            .iter()
            .filter(|name| !moved_bindings.named.iter().any(|moved| moved == *name))
            .cloned()
            .collect();
        let remaining_default = if moved_bindings.default_import.is_some() {
            None
        } else {
            imp.default_import.clone()
        };
        let remaining_namespace = if moved_bindings.namespace_import.is_some() {
            None
        } else {
            imp.namespace_import.clone()
        };

        let type_only = imp.kind == imports::ImportKind::Type;
        let attribute_clause = imports::es_import_attribute_clause(imp);
        // Both the kept and the moved statement replace this one, so both keep
        // its quote and terminator.
        let style = imports::quotes::statement_style(&imp.raw_text);
        let moved_import = generate_import_for_bindings(
            lang,
            &new_import_path,
            &moved_bindings,
            type_only,
            attribute_clause,
            style,
        );

        if remaining_names.is_empty()
            && remaining_default.is_none()
            && remaining_namespace.is_none()
        {
            edits.push((imp.byte_range.clone(), moved_import));
        } else {
            let kept_import =
                imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                    lang,
                    &imp.module_path,
                    &remaining_names,
                    remaining_default.as_deref(),
                    remaining_namespace.as_deref(),
                    type_only,
                    attribute_clause,
                    Some(style),
                );
            edits.push((
                imp.byte_range.clone(),
                format!("{kept_import}\n{moved_import}"),
            ));
        }
    }

    {
        let root = tree.root_node();
        let mut exports = Vec::new();
        let mut cursor = root.walk();
        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.kind() == "export_statement"
                    && export_path_matches_file(content, &node, consumer_file, source_file)
                {
                    exports.push(node);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        exports.sort_by(|left, right| right.byte_range().start.cmp(&left.byte_range().start));
        for node in exports {
            if export_statement_is_namespace_reexport(content, &node) {
                return Err(vec!["namespace re-export (`export * as ...`)".to_string()]);
            }
            if export_statement_has_wildcard(content, &node) {
                return Err(vec!["wildcard re-export (`export * from ...`)".to_string()]);
            }
            if !export_statement_contains_name(content, &node, symbol_name) {
                continue;
            }
            let Some(module_range) = export_module_string_range(content, &node) else {
                continue;
            };
            let Some((moved_specs, remaining_specs)) =
                partition_export_specifiers(content, &node, symbol_name)
            else {
                continue;
            };
            if remaining_specs.is_empty() {
                edits.push((module_range, new_import_path.clone()));
            } else {
                let old_path = &content[module_range];
                let style = imports::quotes::statement_style(&content[node.byte_range()]);
                let quote = style.quote;
                let terminator = if style.semicolon { ";" } else { "" };
                let replacement = format!(
                    "export {{ {} }} from {quote}{}{quote}{terminator}\nexport {{ {} }} from {quote}{}{quote}{terminator}",
                    remaining_specs.join(", "),
                    old_path,
                    moved_specs.join(", "),
                    new_import_path
                );
                edits.push((node.byte_range(), replacement));
            }
        }
    }

    if paths_equivalent(consumer_file, source_file)
        && content_references_identifier(content, lang, symbol_name)
    {
        if let Some(insert_edit) = build_add_moved_import_edit(
            content,
            &tree,
            &block,
            consumer_file,
            dest_file,
            symbol_name,
            lang,
            moved_symbol_is_default,
            project_root,
        ) {
            edits.push(insert_edit);
        }
    }

    if let Some(binding_name) = namespace_plan.binding_name.as_deref() {
        for range in namespace_plan.member_ranges {
            edits.push((range, binding_name.to_string()));
        }
        if let Some(import_specifier) = namespace_plan.import_specifier.as_deref() {
            if let Some(import_edit) = build_add_moved_import_edit(
                content,
                &tree,
                &block,
                consumer_file,
                dest_file,
                import_specifier,
                lang,
                false,
                project_root,
            ) {
                edits.push(import_edit);
            }
        }
    }

    edits.sort_by(|left, right| right.0.start.cmp(&left.0.start));
    let mut result = content.to_string();
    for (range, replacement) in edits {
        result = format!(
            "{}{}{}",
            &result[..range.start],
            replacement,
            &result[range.end..]
        );
    }

    if result == content {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}

#[derive(Debug, Default)]
struct NamespaceRewritePlan {
    member_ranges: Vec<std::ops::Range<usize>>,
    binding_name: Option<String>,
    import_specifier: Option<String>,
}

fn build_namespace_rewrite_plan(
    source: &str,
    tree: &tree_sitter::Tree,
    import_block: &imports::ImportBlock,
    namespace_aliases: &std::collections::BTreeSet<String>,
    consumer_file: &Path,
    dest_file: &Path,
    symbol_name: &str,
    existing_moved_binding: Option<&str>,
) -> Result<NamespaceRewritePlan, Vec<String>> {
    if namespace_aliases.is_empty() {
        return Ok(NamespaceRewritePlan::default());
    }

    let mut member_ranges = Vec::new();
    let mut namespace_object_ranges = Vec::new();
    let mut unsupported_shapes = std::collections::BTreeSet::new();
    collect_namespace_symbol_usages(
        source,
        tree.root_node(),
        namespace_aliases,
        symbol_name,
        &mut member_ranges,
        &mut namespace_object_ranges,
        &mut unsupported_shapes,
    );

    if !unsupported_shapes.is_empty() {
        return Err(unsupported_shapes.into_iter().collect());
    }
    if member_ranges.is_empty() {
        return Ok(NamespaceRewritePlan::default());
    }

    member_ranges.sort_by_key(|range| (range.start, range.end));
    member_ranges.dedup();
    namespace_object_ranges.sort_by_key(|range| (range.start, range.end));
    namespace_object_ranges.dedup();

    let existing_destination_binding =
        existing_named_binding_from_file(import_block, consumer_file, dest_file, symbol_name);
    if let Some(binding) = existing_moved_binding.or(existing_destination_binding.as_deref()) {
        if name_is_shadowed_at_namespace_sites(
            source,
            tree.root_node(),
            &namespace_object_ranges,
            binding,
        ) {
            return Err(vec![format!(
                "namespace member replacement collides with existing binding `{binding}`"
            )]);
        }
        return Ok(NamespaceRewritePlan {
            member_ranges,
            binding_name: Some(binding.to_string()),
            import_specifier: None,
        });
    }

    let root = tree.root_node();
    let collides = import_block_binds_name(import_block, symbol_name)
        || root_scope_binds_name(&root, source, symbol_name)
        || node_references_identifier(&root, source, symbol_name)
        || name_is_shadowed_at_namespace_sites(source, root, &namespace_object_ranges, symbol_name);

    let binding_name = if collides {
        let mut identifiers = std::collections::BTreeSet::new();
        collect_identifier_names(source, root, &mut identifiers);
        next_available_import_alias(symbol_name, &identifiers)
    } else {
        symbol_name.to_string()
    };
    let import_specifier = if binding_name == symbol_name {
        symbol_name.to_string()
    } else {
        format!("{symbol_name} as {binding_name}")
    };

    Ok(NamespaceRewritePlan {
        member_ranges,
        binding_name: Some(binding_name),
        import_specifier: Some(import_specifier),
    })
}

fn collect_namespace_symbol_usages(
    source: &str,
    node: tree_sitter::Node,
    namespace_aliases: &std::collections::BTreeSet<String>,
    symbol_name: &str,
    member_ranges: &mut Vec<std::ops::Range<usize>>,
    namespace_object_ranges: &mut Vec<std::ops::Range<usize>>,
    unsupported_shapes: &mut std::collections::BTreeSet<String>,
) {
    if matches!(node.kind(), "identifier" | "shorthand_property_identifier") {
        let alias = source[node.byte_range()].trim();
        if namespace_aliases.contains(alias)
            && !node_is_inside_import_statement(node)
            && !identifier_is_shadowed_by_enclosing_scope(
                &node,
                source,
                alias,
                IdentifierNamespace::Value,
            )
        {
            match node.parent() {
                Some(parent)
                    if parent.kind() == "member_expression"
                        && parent
                            .child_by_field_name("object")
                            .is_some_and(|object| same_node(&object, &node)) =>
                {
                    if parent
                        .child_by_field_name("property")
                        .is_some_and(|property| source[property.byte_range()].trim() == symbol_name)
                    {
                        member_ranges.push(parent.byte_range());
                        namespace_object_ranges.push(node.byte_range());
                    }
                }
                Some(parent)
                    if parent.kind() == "subscript_expression"
                        && parent
                            .child_by_field_name("object")
                            .is_some_and(|object| same_node(&object, &node)) =>
                {
                    unsupported_shapes.insert(format!("computed namespace access `{alias}[...]`"));
                }
                _ => {
                    unsupported_shapes
                        .insert(format!("namespace binding `{alias}` used as a value"));
                }
            }
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_namespace_symbol_usages(
                source,
                cursor.node(),
                namespace_aliases,
                symbol_name,
                member_ranges,
                namespace_object_ranges,
                unsupported_shapes,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn node_is_inside_import_statement(mut node: tree_sitter::Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "import_statement" {
            return true;
        }
        node = parent;
    }
    false
}

fn existing_named_binding_from_file(
    import_block: &imports::ImportBlock,
    consumer_file: &Path,
    target_file: &Path,
    symbol_name: &str,
) -> Option<String> {
    import_block.imports.iter().find_map(|import| {
        if import.kind != imports::ImportKind::Value
            || !import_path_matches_file(&import.module_path, consumer_file, target_file)
        {
            return None;
        }
        import
            .names
            .iter()
            .find(|name| imports::specifier_imported_name(name) == symbol_name)
            .map(|name| imports::specifier_local_name(name).to_string())
    })
}

fn import_block_binds_name(import_block: &imports::ImportBlock, name: &str) -> bool {
    import_block.imports.iter().any(|import| {
        import.default_import.as_deref() == Some(name)
            || import.namespace_import.as_deref() == Some(name)
            || import
                .names
                .iter()
                .any(|specifier| imports::specifier_local_name(specifier) == name)
    })
}

fn root_scope_binds_name(root: &tree_sitter::Node, source: &str, name: &str) -> bool {
    (0..root.child_count()).any(|index| {
        root.child(index as u32).is_some_and(|child| {
            direct_scope_child_binds_name(&child, source, name, IdentifierNamespace::Value)
                || direct_scope_child_binds_name(&child, source, name, IdentifierNamespace::Type)
        })
    })
}

fn name_is_shadowed_at_namespace_sites(
    source: &str,
    node: tree_sitter::Node,
    namespace_object_ranges: &[std::ops::Range<usize>],
    name: &str,
) -> bool {
    if namespace_object_ranges
        .iter()
        .any(|range| range.start == node.start_byte() && range.end == node.end_byte())
        && identifier_is_shadowed_by_enclosing_scope(
            &node,
            source,
            name,
            IdentifierNamespace::Value,
        )
    {
        return true;
    }

    (0..node.child_count()).any(|index| {
        node.child(index as u32).is_some_and(|child| {
            name_is_shadowed_at_namespace_sites(source, child, namespace_object_ranges, name)
        })
    })
}

fn collect_identifier_names(
    source: &str,
    node: tree_sitter::Node,
    names: &mut std::collections::BTreeSet<String>,
) {
    if node.kind().contains("identifier") {
        let name = source[node.byte_range()].trim();
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index as u32) {
            collect_identifier_names(source, child, names);
        }
    }
}

fn next_available_import_alias(
    symbol_name: &str,
    identifiers: &std::collections::BTreeSet<String>,
) -> String {
    for suffix in 2usize.. {
        let candidate = format!("{symbol_name}_{suffix}");
        if !identifiers.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("the monotonically increasing alias suffix must eventually be unused")
}

fn export_statement_is_namespace_reexport(source: &str, node: &tree_sitter::Node) -> bool {
    node.kind() == "export_statement"
        && source[node.byte_range()]
            .trim_start()
            .starts_with("export * as ")
}

fn export_path_matches_file(
    source: &str,
    node: &tree_sitter::Node,
    consumer_file: &Path,
    source_file: &Path,
) -> bool {
    export_module_path(source, node)
        .as_deref()
        .is_some_and(|path| import_path_matches_file(path, consumer_file, source_file))
}

fn export_module_path(source: &str, node: &tree_sitter::Node) -> Option<String> {
    export_module_string_range(source, node).map(|range| source[range].to_string())
}

fn export_module_string_range(
    source: &str,
    node: &tree_sitter::Node,
) -> Option<std::ops::Range<usize>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if child.kind() == "string" {
            let raw = &source[child.byte_range()];
            let start = child.byte_range().start + 1;
            let end = child.byte_range().end.saturating_sub(1);
            if (raw.starts_with('\'') || raw.starts_with('"'))
                && (raw.ends_with('\'') || raw.ends_with('"'))
                && start <= end
            {
                return Some(start..end);
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn export_statement_has_wildcard(source: &str, node: &tree_sitter::Node) -> bool {
    source[node.byte_range()].contains("export *")
}

fn export_statement_contains_name(
    source: &str,
    node: &tree_sitter::Node,
    symbol_name: &str,
) -> bool {
    partition_export_specifiers(source, node, symbol_name)
        .is_some_and(|(moved, _)| !moved.is_empty())
}

fn partition_export_specifiers(
    source: &str,
    node: &tree_sitter::Node,
    symbol_name: &str,
) -> Option<(Vec<String>, Vec<String>)> {
    let text = &source[node.byte_range()];
    let open = text.find('{')?;
    let close = text[open + 1..].find('}').map(|idx| open + 1 + idx)?;
    let mut moved = Vec::new();
    let mut remaining = Vec::new();
    for spec in text[open + 1..close]
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if imports::specifier_matches(spec, symbol_name) {
            moved.push(spec.to_string());
        } else {
            remaining.push(spec.to_string());
        }
    }
    Some((moved, remaining))
}

#[derive(Debug, Default)]
struct MovedImportBindings {
    named: Vec<String>,
    default_import: Option<String>,
    namespace_import: Option<String>,
}

impl MovedImportBindings {
    fn is_empty(&self) -> bool {
        self.named.is_empty() && self.default_import.is_none() && self.namespace_import.is_none()
    }
}

fn moved_bindings_for_import(
    imp: &imports::ImportStatement,
    symbol_name: &str,
    moved_symbol_is_default: bool,
) -> MovedImportBindings {
    let named = if moved_symbol_is_default {
        Vec::new()
    } else {
        imp.names
            .iter()
            .filter(|n| imports::specifier_matches(n, symbol_name))
            .cloned()
            .collect()
    };

    let default_import = if moved_symbol_is_default {
        imp.default_import.clone()
    } else {
        None
    };

    MovedImportBindings {
        named,
        default_import,
        namespace_import: None,
    }
}

fn generate_import_for_bindings(
    lang: LangId,
    module_path: &str,
    bindings: &MovedImportBindings,
    type_only: bool,
    attribute_clause: Option<&str>,
    style: imports::quotes::EsImportStyle,
) -> String {
    imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
        lang,
        module_path,
        &bindings.named,
        bindings.default_import.as_deref(),
        bindings.namespace_import.as_deref(),
        type_only,
        attribute_clause,
        Some(style),
    )
}

fn parse_imports_from_content(
    content: &str,
    lang: LangId,
) -> Option<(tree_sitter::Tree, imports::ImportBlock)> {
    #[cfg(test)]
    MOVE_CONSUMER_PARSES.with(|count| count.set(count.get() + 1));
    let tree = parse_source_with_cached_parser(Path::new("consumer"), content, lang).ok()?;
    let block = imports::parse_imports(content, &tree, lang);
    Some((tree, block))
}

#[cfg(test)]
thread_local! {
    static MOVE_CONSUMER_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn build_add_moved_import_edit(
    content: &str,
    tree: &tree_sitter::Tree,
    block: &imports::ImportBlock,
    consumer_file: &Path,
    dest_file: &Path,
    symbol_name: &str,
    lang: LangId,
    moved_symbol_is_default: bool,
    project_root: &Path,
) -> Option<(std::ops::Range<usize>, String)> {
    let new_import_path = compute_relative_import_path(consumer_file, dest_file);
    let names = if moved_symbol_is_default {
        Vec::new()
    } else {
        vec![symbol_name.to_string()]
    };
    let default_import = moved_symbol_is_default.then_some(symbol_name);

    if imports::is_duplicate(block, &new_import_path, &names, default_import, false) {
        return None;
    }

    // Match add_import's same-module merge behavior so namespace rewrites do
    // not create a second named-import statement when the destination is already
    // imported. Preserve the existing module spelling (for example `./dest.js`).
    if !moved_symbol_is_default {
        if let Some(existing) = block.imports.iter().find(|import| {
            let es_import = matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript);
            import.kind == imports::ImportKind::Value
                && import.namespace_import.is_none()
                && (es_import || import.default_import.is_none())
                && (es_import || !import.names.is_empty())
                && import_path_matches_file(&import.module_path, consumer_file, dest_file)
        }) {
            let requested = &names[0];
            let requested_imported = imports::specifier_imported_name(requested);
            let requested_local = imports::specifier_local_name(requested);
            if existing.names.iter().any(|name| {
                imports::specifier_imported_name(name) == requested_imported
                    && imports::specifier_local_name(name) == requested_local
            }) {
                return None;
            }

            let merged_names =
                super::add_import::merge_named_import_specifiers(&existing.names, &names);
            let merged_line =
                imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                    lang,
                    &existing.module_path,
                    &merged_names,
                    existing.default_import.as_deref(),
                    existing.namespace_import.as_deref(),
                    false,
                    imports::es_import_attribute_clause(existing),
                    Some(imports::quotes::statement_style(&existing.raw_text)),
                );
            return Some((existing.byte_range.clone(), merged_line));
        }
    }

    let group = imports::classify_group(lang, &new_import_path);
    let (insert_offset, needs_blank_before, needs_blank_after) =
        imports::find_insertion_point(content, block, group, &new_import_path, false);
    let import_line = if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        let mut clauses = block
            .imports
            .iter()
            .filter(|import| {
                import_path_matches_file(&import.module_path, consumer_file, dest_file)
            })
            .filter_map(imports::es_import_attribute_clause);
        let inherited_clause = clauses
            .next()
            .filter(|first| clauses.all(|item| item == *first));
        imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
            lang,
            &new_import_path,
            &names,
            default_import,
            None,
            false,
            inherited_clause,
            Some(imports::quotes::preferred_style(
                content,
                tree,
                lang,
                consumer_file,
                Some(project_root),
            )),
        )
    } else {
        imports::generate_import_line(lang, &new_import_path, &names, default_import, false)
    };

    let mut insert_text = String::new();
    if needs_blank_before {
        insert_text.push('\n');
    }
    insert_text.push_str(&import_line);
    insert_text.push('\n');
    if needs_blank_after {
        insert_text.push('\n');
    }

    Some((insert_offset..insert_offset, insert_text))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentifierNamespace {
    Value,
    Type,
}

fn content_references_identifier(content: &str, lang: LangId, symbol_name: &str) -> bool {
    let grammar = grammar_for(lang);
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return false;
    }
    let Some(tree) = parser.parse(content.as_bytes(), None) else {
        return false;
    };

    let root = tree.root_node();
    if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        node_references_identifier(&root, content, symbol_name)
    } else {
        node_references_identifier_naive(&root, content, symbol_name)
    }
}

fn node_references_identifier(node: &tree_sitter::Node, content: &str, symbol_name: &str) -> bool {
    if is_reference_identifier_node(node.kind()) && node_text_matches(content, node, symbol_name) {
        let namespace = reference_namespace(node);
        if !identifier_is_binding_position(node, content, symbol_name, namespace)
            && !identifier_is_shadowed_by_enclosing_scope(node, content, symbol_name, namespace)
        {
            return true;
        }
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if node_references_identifier(&child, content, symbol_name) {
                return true;
            }
        }
    }

    false
}

fn node_references_identifier_naive(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    if is_reference_identifier_node(node.kind()) && node_text_matches(content, node, symbol_name) {
        return true;
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if node_references_identifier_naive(&child, content, symbol_name) {
                return true;
            }
        }
    }

    false
}

fn is_reference_identifier_node(kind: &str) -> bool {
    matches!(kind, "identifier" | "type_identifier" | "jsx_identifier")
}

fn reference_namespace(node: &tree_sitter::Node) -> IdentifierNamespace {
    if node.kind() == "type_identifier" {
        IdentifierNamespace::Type
    } else {
        IdentifierNamespace::Value
    }
}

fn identifier_is_binding_position(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "import_statement" {
            return true;
        }

        if declaration_name_matches_target(&parent, node, content, symbol_name, namespace) {
            return true;
        }

        match parent.kind() {
            "variable_declarator" if namespace == IdentifierNamespace::Value => {
                if parent.child_by_field_name("name").is_some_and(|name| {
                    pattern_contains_binding_node(&name, node, content, symbol_name)
                }) {
                    return true;
                }
            }
            "formal_parameters" | "required_parameter" | "optional_parameter" | "rest_pattern"
            | "assignment_pattern" | "object_pattern" | "array_pattern" | "pair_pattern"
                if namespace == IdentifierNamespace::Value =>
            {
                if pattern_contains_binding_node(&parent, node, content, symbol_name) {
                    return true;
                }
            }
            "catch_clause" if namespace == IdentifierNamespace::Value => {
                if parent
                    .child_by_field_name("parameter")
                    .is_some_and(|param| {
                        pattern_contains_binding_node(&param, node, content, symbol_name)
                    })
                {
                    return true;
                }
            }
            "type_parameter" if namespace == IdentifierNamespace::Type => {
                if type_parameter_name_matches_target(&parent, node, content, symbol_name) {
                    return true;
                }
            }
            _ => {}
        }

        current = parent;
    }

    false
}

fn identifier_is_shadowed_by_enclosing_scope(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        if scope_introduces_shadowing_binding(&parent, node, content, symbol_name, namespace) {
            return true;
        }
        current = parent;
    }

    false
}

fn scope_introduces_shadowing_binding(
    scope: &tree_sitter::Node,
    reference: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    match scope.kind() {
        kind if is_function_like_scope(kind) => {
            function_scope_binds_name(scope, reference, content, symbol_name, namespace)
        }
        "statement_block" | "switch_body" => {
            block_scope_binds_name(scope, content, symbol_name, namespace)
        }
        "catch_clause" => catch_scope_binds_name(scope, reference, content, symbol_name, namespace),
        "for_statement" | "for_in_statement" | "for_of_statement" => {
            loop_scope_binds_name(scope, reference, content, symbol_name, namespace)
        }
        _ => false,
    }
}

fn is_function_like_scope(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}

fn function_scope_binds_name(
    scope: &tree_sitter::Node,
    reference: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let Some(body) = scope.child_by_field_name("body") else {
        return false;
    };
    if !node_contains(&body, reference) {
        return false;
    }

    match namespace {
        IdentifierNamespace::Value => {
            function_inner_name_binds_name(scope, content, symbol_name)
                || scope
                    .child_by_field_name("parameters")
                    .is_some_and(|params| pattern_binds_name(&params, content, symbol_name))
                || function_var_declarations_bind_name(scope, content, symbol_name)
        }
        IdentifierNamespace::Type => {
            class_declaration_name_matches(scope, content, symbol_name)
                || scope
                    .child_by_field_name("type_parameters")
                    .is_some_and(|params| type_parameters_bind_name(&params, content, symbol_name))
        }
    }
}

fn function_inner_name_binds_name(
    scope: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    if scope.kind() == "method_definition" {
        return false;
    }
    name_field_matches(scope, content, symbol_name)
}

fn class_declaration_name_matches(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "abstract_class_declaration" | "class"
    ) && name_field_matches(node, content, symbol_name)
}

fn function_var_declarations_bind_name(
    scope: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    scope
        .child_by_field_name("body")
        .is_some_and(|body| var_declarations_bind_name_in_scope(&body, content, symbol_name))
}

fn var_declarations_bind_name_in_scope(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    let child_count = node.child_count();
    for i in 0..child_count {
        let Some(child) = node.child(i as u32) else {
            continue;
        };
        if is_function_like_scope(child.kind()) {
            continue;
        }
        if child.kind() == "variable_declaration"
            && declaration_binds_name(&child, content, symbol_name, IdentifierNamespace::Value)
        {
            return true;
        }
        if var_declarations_bind_name_in_scope(&child, content, symbol_name) {
            return true;
        }
    }

    false
}

fn block_scope_binds_name(
    block: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let child_count = block.child_count();
    for i in 0..child_count {
        let Some(child) = block.child(i as u32) else {
            continue;
        };
        if direct_scope_child_binds_name(&child, content, symbol_name, namespace) {
            return true;
        }
    }

    false
}

fn direct_scope_child_binds_name(
    child: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    if declaration_binds_name(child, content, symbol_name, namespace) {
        return true;
    }

    if child.kind() == "export_statement" {
        let child_count = child.child_count();
        for i in 0..child_count {
            if let Some(export_child) = child.child(i as u32) {
                if declaration_binds_name(&export_child, content, symbol_name, namespace) {
                    return true;
                }
            }
        }
    }

    false
}

fn catch_scope_binds_name(
    scope: &tree_sitter::Node,
    reference: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    if namespace != IdentifierNamespace::Value {
        return false;
    }

    let Some(body) = scope.child_by_field_name("body") else {
        return false;
    };
    if !node_contains(&body, reference) {
        return false;
    }

    scope
        .child_by_field_name("parameter")
        .is_some_and(|param| pattern_binds_name(&param, content, symbol_name))
}

fn loop_scope_binds_name(
    scope: &tree_sitter::Node,
    reference: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    if namespace != IdentifierNamespace::Value
        || !reference_is_in_loop_bound_scope(scope, reference)
    {
        return false;
    }

    ["initializer", "left"].iter().any(|field| {
        scope
            .child_by_field_name(field)
            .is_some_and(|node| loop_header_declares_name(&node, content, symbol_name))
    })
}

fn reference_is_in_loop_bound_scope(
    scope: &tree_sitter::Node,
    reference: &tree_sitter::Node,
) -> bool {
    ["condition", "increment", "body"].iter().any(|field| {
        scope
            .child_by_field_name(field)
            .is_some_and(|node| node_contains(&node, reference))
    })
}

fn loop_header_declares_name(node: &tree_sitter::Node, content: &str, symbol_name: &str) -> bool {
    matches!(
        node.kind(),
        "lexical_declaration" | "variable_declaration" | "variable_declarator"
    ) && declaration_binds_name(node, content, symbol_name, IdentifierNamespace::Value)
}

fn declaration_binds_name(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    match node.kind() {
        "variable_declarator" if namespace == IdentifierNamespace::Value => node
            .child_by_field_name("name")
            .is_some_and(|name| pattern_binds_name(&name, content, symbol_name)),
        "lexical_declaration" | "variable_declaration"
            if namespace == IdentifierNamespace::Value =>
        {
            node_children_bind_name(node, content, symbol_name, namespace)
        }
        "function_declaration" | "generator_function_declaration" => {
            namespace == IdentifierNamespace::Value
                && name_field_matches(node, content, symbol_name)
        }
        "class_declaration" | "abstract_class_declaration" | "class" | "enum_declaration" => {
            name_field_matches(node, content, symbol_name)
        }
        "interface_declaration" | "type_alias_declaration" => {
            namespace == IdentifierNamespace::Type && name_field_matches(node, content, symbol_name)
        }
        _ => false,
    }
}

fn node_children_bind_name(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if declaration_binds_name(&child, content, symbol_name, namespace) {
                return true;
            }
        }
    }
    false
}

fn declaration_name_matches_target(
    declaration: &tree_sitter::Node,
    target: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
    namespace: IdentifierNamespace,
) -> bool {
    let name_binds_namespace = match declaration.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function" => namespace == IdentifierNamespace::Value,
        "class_declaration" | "abstract_class_declaration" | "class" | "enum_declaration" => true,
        "interface_declaration" | "type_alias_declaration" => {
            namespace == IdentifierNamespace::Type
        }
        _ => false,
    };

    name_binds_namespace
        && declaration.child_by_field_name("name").is_some_and(|name| {
            same_node(&name, target) && node_text_matches(content, target, symbol_name)
        })
}

fn type_parameter_name_matches_target(
    type_parameter: &tree_sitter::Node,
    target: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    type_parameter
        .child_by_field_name("name")
        .is_some_and(|name| {
            same_node(&name, target) && node_text_matches(content, target, symbol_name)
        })
}

fn type_parameters_bind_name(node: &tree_sitter::Node, content: &str, symbol_name: &str) -> bool {
    if node.kind() == "type_parameter" && name_field_matches(node, content, symbol_name) {
        return true;
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if type_parameters_bind_name(&child, content, symbol_name) {
                return true;
            }
        }
    }
    false
}

fn pattern_binds_name(pattern: &tree_sitter::Node, content: &str, symbol_name: &str) -> bool {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            node_text_matches(content, pattern, symbol_name)
        }
        "required_parameter" | "optional_parameter" => pattern
            .child_by_field_name("pattern")
            .is_some_and(|child| pattern_binds_name(&child, content, symbol_name)),
        "assignment_pattern" => pattern
            .child_by_field_name("left")
            .is_some_and(|child| pattern_binds_name(&child, content, symbol_name)),
        "pair_pattern" => pattern
            .child_by_field_name("value")
            .or_else(|| pattern.child_by_field_name("key"))
            .is_some_and(|child| pattern_binds_name(&child, content, symbol_name)),
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            node_children_pattern_bind_name(pattern, content, symbol_name)
        }
        kind if kind.ends_with("_pattern") => {
            node_children_pattern_bind_name(pattern, content, symbol_name)
        }
        _ => false,
    }
}

fn node_children_pattern_bind_name(
    node: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if pattern_binds_name(&child, content, symbol_name) {
                return true;
            }
        }
    }
    false
}

fn pattern_contains_binding_node(
    pattern: &tree_sitter::Node,
    target: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    if !node_contains(pattern, target) {
        return false;
    }

    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            same_node(pattern, target) && node_text_matches(content, target, symbol_name)
        }
        "required_parameter" | "optional_parameter" => {
            pattern.child_by_field_name("pattern").is_some_and(|child| {
                pattern_contains_binding_node(&child, target, content, symbol_name)
            })
        }
        "assignment_pattern" => pattern.child_by_field_name("left").is_some_and(|child| {
            pattern_contains_binding_node(&child, target, content, symbol_name)
        }),
        "pair_pattern" => pattern
            .child_by_field_name("value")
            .or_else(|| pattern.child_by_field_name("key"))
            .is_some_and(|child| {
                pattern_contains_binding_node(&child, target, content, symbol_name)
            }),
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            node_children_pattern_contain_binding_node(pattern, target, content, symbol_name)
        }
        kind if kind.ends_with("_pattern") => {
            node_children_pattern_contain_binding_node(pattern, target, content, symbol_name)
        }
        _ => false,
    }
}

fn node_children_pattern_contain_binding_node(
    node: &tree_sitter::Node,
    target: &tree_sitter::Node,
    content: &str,
    symbol_name: &str,
) -> bool {
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            if pattern_contains_binding_node(&child, target, content, symbol_name) {
                return true;
            }
        }
    }
    false
}

fn name_field_matches(node: &tree_sitter::Node, content: &str, symbol_name: &str) -> bool {
    node.child_by_field_name("name")
        .is_some_and(|name| node_text_matches(content, &name, symbol_name))
}

fn same_node(a: &tree_sitter::Node, b: &tree_sitter::Node) -> bool {
    a.kind() == b.kind() && a.start_byte() == b.start_byte() && a.end_byte() == b.end_byte()
}

fn node_contains(container: &tree_sitter::Node, node: &tree_sitter::Node) -> bool {
    container.start_byte() <= node.start_byte() && node.end_byte() <= container.end_byte()
}

fn node_text_matches(content: &str, node: &tree_sitter::Node, expected: &str) -> bool {
    content
        .get(node.byte_range())
        .is_some_and(|text| text == expected)
}

/// Extract an alias for a symbol from an import statement's raw text.
///
/// Looks for `symbol_name as alias` pattern in the import text.
#[cfg(test)]
fn extract_alias(raw_text: &str, symbol_name: &str) -> Option<String> {
    // Look for `symbolName as aliasName` pattern
    let pattern = format!("{} as ", symbol_name);
    if let Some(pos) = raw_text.find(&pattern) {
        let after = &raw_text[pos + pattern.len()..];
        // Extract the alias identifier (until comma, brace, whitespace, or semicolon)
        let alias: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if !alias.is_empty() {
            return Some(alias);
        }
    }
    None
}

/// Restore a checkpoint by name, scoped to the caller's session.
fn restore_checkpoint(ctx: &AppContext, session: &str, name: &str) -> bool {
    let mut cp_store = ctx.checkpoint().lock();
    if let Err(e) = cp_store.restore(session, name) {
        log::debug!(
            "move_symbol rollback: failed to restore checkpoint '{}': {}",
            name,
            e
        );
        false
    } else {
        true
    }
}

/// Delete new files that were created during the operation.
fn cleanup_new_files(new_files: &[PathBuf]) {
    for path in new_files {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                log::debug!(
                    "move_symbol rollback: failed to delete new file {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }
}

/// Build a structured error response for a failed move operation.
fn move_error(
    req_id: &str,
    failed_file: &str,
    written_files: &[PathBuf],
    new_files: &[PathBuf],
    message: &str,
) -> Response {
    let mut rolled_back: Vec<serde_json::Value> = written_files
        .iter()
        .map(|p| {
            serde_json::json!({
                "file": p.display().to_string(),
                "action": "restored",
            })
        })
        .collect();

    for p in new_files {
        rolled_back.push(serde_json::json!({
            "file": p.display().to_string(),
            "action": "deleted",
        }));
    }

    log::debug!(
        "move_symbol failed at {}: {} — rolled back {} files",
        failed_file,
        message,
        rolled_back.len()
    );

    Response::error_with_data(
        req_id,
        "move_symbol_failed",
        message,
        serde_json::json!({
            "failed_file": failed_file,
            "rolled_back": rolled_back,
        }),
    )
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_same_directory() {
        let from = Path::new("src/components/Button.ts");
        let to = Path::new("src/components/utils.ts");
        assert_eq!(compute_relative_import_path(from, to), "./utils");
    }

    #[test]
    fn move_consumer_prefilter_skips_scripts_without_module_declarations() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.ts");
        let dest = root.path().join("dest.ts");
        std::fs::write(&source, "export function moved() {}\n").unwrap();
        std::fs::write(&dest, "").unwrap();
        let consumer = root.path().join("consumer.ts");
        MOVE_CONSUMER_PARSES.with(|count| count.set(0));
        for i in 0..512 {
            let script = format!("const script{i} = {}\n", "[0, 1, 2];\n".repeat(256));
            assert_eq!(
                rewrite_consumer_imports(
                    &script,
                    &consumer,
                    &source,
                    &dest,
                    "moved",
                    Some(LangId::TypeScript),
                    false,
                    root.path()
                ),
                Ok(None)
            );
        }
        let input = "import { moved } from './source';\nmoved();\n";
        let actual = rewrite_consumer_imports(
            input,
            &consumer,
            &source,
            &dest,
            "moved",
            Some(LangId::TypeScript),
            false,
            root.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(actual, "import { moved } from './dest';\nmoved();\n");
        assert_eq!(
            MOVE_CONSUMER_PARSES.with(|count| count.get()),
            1,
            "consumer parses"
        );
    }

    #[test]
    fn relative_path_parent_directory() {
        let from = Path::new("src/components/Button.ts");
        let to = Path::new("src/utils.ts");
        assert_eq!(compute_relative_import_path(from, to), "../utils");
    }

    #[test]
    fn relative_path_sibling_directory() {
        let from = Path::new("src/components/Button.ts");
        let to = Path::new("src/shared/utils.ts");
        assert_eq!(compute_relative_import_path(from, to), "../shared/utils");
    }

    #[test]
    fn relative_path_deeply_nested() {
        let from = Path::new("src/features/auth/components/Login.ts");
        let to = Path::new("src/lib/helpers.ts");
        assert_eq!(
            compute_relative_import_path(from, to),
            "../../../lib/helpers"
        );
    }

    #[test]
    fn relative_path_child_directory() {
        let from = Path::new("src/index.ts");
        let to = Path::new("src/utils/helpers.ts");
        assert_eq!(compute_relative_import_path(from, to), "./utils/helpers");
    }

    #[test]
    fn relative_path_strips_extension() {
        let from = Path::new("src/app.tsx");
        let to = Path::new("src/components/Header.tsx");
        assert_eq!(
            compute_relative_import_path(from, to),
            "./components/Header"
        );
    }

    #[test]
    fn prepare_exported_adds_export() {
        let text = "function doStuff() { return 42; }";
        assert_eq!(
            prepare_exported_symbol(text),
            "export function doStuff() { return 42; }"
        );
    }

    #[test]
    fn prepare_exported_preserves_existing() {
        let text = "export function doStuff() { return 42; }";
        assert_eq!(
            prepare_exported_symbol(text),
            "export function doStuff() { return 42; }"
        );
    }

    #[test]
    fn prepare_exported_preserves_export_reexports_and_default() {
        assert_eq!(
            prepare_exported_symbol("export default function doStuff() { return 42; }"),
            "export default function doStuff() { return 42; }"
        );
        assert_eq!(
            prepare_exported_symbol("export { doStuff } from './other';"),
            "export { doStuff } from './other';"
        );
        assert_eq!(
            prepare_exported_symbol("export * from './other';"),
            "export * from './other';"
        );
    }

    #[test]
    fn extract_alias_found() {
        let raw = "import { formatDate as fmtDate, other } from './utils';";
        assert_eq!(
            extract_alias(raw, "formatDate"),
            Some("fmtDate".to_string())
        );
    }

    #[test]
    fn extract_alias_not_found() {
        let raw = "import { formatDate, other } from './utils';";
        assert_eq!(extract_alias(raw, "formatDate"), None);
    }

    #[test]
    fn import_path_matches_same_dir() {
        let consumer = Path::new("src/components/Button.ts");
        let target = Path::new("src/components/utils.ts");
        assert!(import_path_matches_file("./utils", consumer, target));
    }

    #[test]
    fn import_path_matches_parent_dir() {
        let consumer = Path::new("src/components/Button.ts");
        let target = Path::new("src/service.ts");
        assert!(import_path_matches_file("../service", consumer, target));
    }

    #[test]
    fn import_path_no_match_different_file() {
        let consumer = Path::new("src/components/Button.ts");
        let target = Path::new("src/components/utils.ts");
        assert!(!import_path_matches_file("./other", consumer, target));
    }

    #[test]
    fn import_path_no_match_external() {
        let consumer = Path::new("src/components/Button.ts");
        let target = Path::new("src/components/utils.ts");
        assert!(!import_path_matches_file("react", consumer, target));
    }

    #[test]
    fn normalize_path_with_parent() {
        let p = Path::new("src/components/../utils.ts");
        assert_eq!(normalize_path(p), PathBuf::from("src/utils.ts"));
    }

    #[test]
    fn normalize_path_with_current() {
        let p = Path::new("src/./components/Button.ts");
        assert_eq!(normalize_path(p), PathBuf::from("src/components/Button.ts"));
    }

    #[test]
    fn remove_symbol_cleans_whitespace() {
        let source = "export function keep() {}\n\nexport function remove() {}\n\nexport function alsoKeep() {}\n";
        let start = source.find("export function remove").unwrap();
        let end = start + "export function remove() {}".len();
        let result = remove_symbol_from_source(source, start, end).unwrap();
        assert!(result.contains("export function keep()"));
        assert!(!result.contains("remove"));
        assert!(result.contains("export function alsoKeep()"));
    }

    #[test]
    fn append_to_empty_dest() {
        let result = append_symbol_to_dest("", "export function foo() {}");
        assert_eq!(result, "export function foo() {}\n");
    }

    #[test]
    fn append_to_existing_dest() {
        let result =
            append_symbol_to_dest("export function bar() {}\n", "export function foo() {}");
        assert_eq!(
            result,
            "export function bar() {}\n\nexport function foo() {}\n"
        );
    }

    // --- find_export_keyword_start ----------------------------------------

    #[test]
    fn find_export_keyword_simple_export() {
        // `export function greet(...)` — decl_start=7 (start of "function")
        let source = "export function greet() {}\n";
        assert_eq!(find_export_keyword_start(source, 7), Some(0));
    }

    #[test]
    fn find_export_keyword_export_default() {
        // `export default function f(...)` — decl_start=15 (start of "function")
        let source = "export default function f() {}\n";
        assert_eq!(find_export_keyword_start(source, 15), Some(0));
    }

    #[test]
    fn find_export_keyword_with_jsdoc_above() {
        // The JSDoc lives on prior lines; `export` is still the immediate
        // predecessor of `function`. Decl_start lands on `f` of `function`.
        let source = "/** doc */\nexport function f() {}\n";
        let decl_start = source.find("function").unwrap();
        assert_eq!(find_export_keyword_start(source, decl_start), Some(11));
    }

    #[test]
    fn find_export_keyword_no_export() {
        // Plain `function f()` — no preceding export keyword. Helper must
        // return None so the caller falls back to the raw decl_start.
        let source = "function f() {}\n";
        assert_eq!(find_export_keyword_start(source, 0), None);
    }

    #[test]
    fn find_export_keyword_unrelated_word_before() {
        // A `report` token that ENDS with the bytes "ort" but doesn't start
        // with `export` should not be matched. Decl_start lands after "report ".
        let source = "let report = 1;\nfunction f() {}\n";
        let decl_start = source.find("function").unwrap();
        // Walking backwards we'd see " " then "report ;1 = troper " etc. No
        // "export" token, so None.
        assert_eq!(find_export_keyword_start(source, decl_start), None);
    }

    #[test]
    fn find_export_keyword_does_not_match_substring() {
        // `_export` (with leading underscore) is part of a different
        // identifier; it must NOT match because the left word boundary
        // check sees `_` before `e`.
        let source = "let _export = 1;\nfunction f() {}\n";
        let decl_start = source.find("function").unwrap();
        assert_eq!(find_export_keyword_start(source, decl_start), None);
    }

    #[test]
    fn find_export_keyword_at_zero_decl_start() {
        // Edge case: caller passed 0 (decl is at start of file). No export
        // can be before position 0. Return None.
        assert_eq!(find_export_keyword_start("function f() {}", 0), None);
    }
}
