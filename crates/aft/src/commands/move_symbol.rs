//! Handler for the `move_symbol` command: move a top-level symbol from one file
//! to another with full import rewiring across all consumer files.
//!
//! Flow: resolve symbol → verify top-level → checkpoint → discover consumers
//! → extract symbol text → remove from source → add to destination → rewrite
//! imports in every consumer → format/validate all files → return results.

use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
use crate::imports;
use crate::lsp_hints;
use crate::parser::{detect_language, grammar_for, LangId};
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

    // --- Call graph guard (D089) ---
    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "move_symbol: project not configured — send 'configure' first",
            );
        }
    };

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
    let source_content = match std::fs::read_to_string(source_path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
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
    let dest_symbol_text = prepare_exported_symbol(symbol_text);

    // Prepare source with symbol removed
    let new_source = match remove_symbol_from_source(&source_content, start_byte, end_byte) {
        Ok(s) => s,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    // --- Read destination file (may not exist yet) ---
    let dest_content = if dest_path.exists() {
        std::fs::read_to_string(dest_path).unwrap_or_default()
    } else {
        String::new()
    };

    // Prepare new destination content
    let new_dest = append_symbol_to_dest(&dest_content, &dest_symbol_text);

    // --- Discover consumers via callers_of ---
    // Build file first to ensure it's indexed
    if let Err(e) = graph.build_file(source_path) {
        return Response::error(&req.id, e.code(), e.to_string());
    }

    let consumers = match graph.callers_of(
        source_path,
        symbol_name,
        1,
        ctx.config().max_callgraph_files,
    ) {
        Ok(result) => result.callers,
        // ProjectTooLarge MUST surface — silently proceeding would move the
        // symbol without rewriting consumer imports across the project and
        // leave the workspace in a broken state. Every other caller-discovery
        // failure (symbol not found, parse error, etc.) keeps the pre-0.15.1
        // "no callers found is fine" fallback because those scenarios produce
        // a zero-consumers result that is the same as a clean move.
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            return Response::error(&req.id, "project_too_large", format!("{}", err));
        }
        Err(_) => Vec::new(),
    };

    // Collect consumer files that need import rewriting
    // CallerGroup.file is relative to project root — resolve to absolute
    let project_root = graph.project_root().to_path_buf();
    let mut consumer_files: Vec<PathBuf> = consumers
        .iter()
        .map(|cg| {
            let p = PathBuf::from(&cg.file);
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

    // --- Compute consumer rewrites ---
    let mut consumer_rewrites: Vec<(PathBuf, String, String)> = Vec::new(); // (path, original, new)
    for consumer_file in &consumer_files {
        if !consumer_file.exists() {
            continue;
        }
        let consumer_content = match std::fs::read_to_string(consumer_file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let new_consumer = rewrite_consumer_imports(
            &consumer_content,
            consumer_file,
            source_path,
            dest_path,
            symbol_name,
            Some(source_lang),
        );

        if let Some(rewritten) = new_consumer {
            consumer_rewrites.push((consumer_file.clone(), consumer_content, rewritten));
        }
    }

    // --- Create checkpoint (D105) ---
    let checkpoint_name = format!("move_symbol:{}", symbol_name);
    {
        let mut all_files: Vec<PathBuf> = vec![source_path.to_path_buf()];
        if dest_path.exists() {
            all_files.push(dest_path.to_path_buf());
        }
        for (path, _, _) in &consumer_rewrites {
            all_files.push(path.clone());
        }

        let backup_store = ctx.backup().borrow();
        let mut cp_store = ctx.checkpoint().borrow_mut();
        if let Err(e) = cp_store.create(req.session(), &checkpoint_name, all_files, &backup_store) {
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

        let mut backup_store = ctx.backup().borrow_mut();
        for path in files_to_backup {
            match backup_store.snapshot_with_op(
                req.session(),
                &path,
                "move_symbol: pre-move backup",
                Some(&op_id),
            ) {
                Ok(id) => backup_ids.push(id),
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
                Ok(id) => backup_ids.push(id),
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
    match edit::write_format_validate(&source_path, &new_source, &ctx.config(), &req.params) {
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
                    .borrow_mut()
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
    match edit::write_format_validate(&dest_path, &new_dest, &ctx.config(), &req.params) {
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
                    .borrow_mut()
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
    let mut consumers_updated = 0;
    for (path, _original, new_content) in &consumer_rewrites {
        match edit::write_format_validate(&path, new_content, &ctx.config(), &req.params) {
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
                        .borrow_mut()
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

    let files_modified = results.len();

    log::debug!(
        "move_symbol: {} from {} to {} ({} consumers updated)",
        symbol_name,
        file,
        destination,
        consumers_updated
    );

    Response::success(
        &req.id,
        serde_json::json!({
            "ok": true,
            "files_modified": files_modified,
            "consumers_updated": consumers_updated,
            "checkpoint_name": checkpoint_name,
            "backup_ids": backup_ids,
            "results": results,
        }),
    )
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

fn collect_ts_js_files(root: &Path, out: &mut Vec<PathBuf>, source_path: &Path, dest_path: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("node_modules") {
                continue;
            }
            collect_ts_js_files(&path, out, source_path, dest_path);
        } else if matches!(
            detect_language(&path),
            Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript)
        ) {
            let canon = std::fs::canonicalize(&path).unwrap_or(path);
            if canon != source_path && canon != dest_path {
                out.push(canon);
            }
        }
    }
}

/// Rewrite a consumer file's imports to point to the new destination.
///
/// Finds imports from the source file that include the moved symbol,
/// and rewrites them to import from the destination instead.
/// Returns `None` if no changes needed.
fn rewrite_consumer_imports(
    consumer_content: &str,
    consumer_file: &Path,
    source_file: &Path,
    dest_file: &Path,
    symbol_name: &str,
    lang: Option<LangId>,
) -> Option<String> {
    let lang = lang?;

    // Only handle TS/JS/TSX for now (the primary use case)
    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        return None;
    }

    // Parse imports
    let (_source_text, _tree, block) = match imports::parse_file_imports(consumer_file, lang) {
        Ok(r) => r,
        Err(_) => return None,
    };

    // Use the consumer_content we already read (should match source_text)
    let content = consumer_content;

    // Find imports from the source file that reference the moved symbol
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    // Process imports in reverse order to maintain byte offsets
    let mut matching_imports: Vec<(usize, &imports::ImportStatement)> = block
        .imports
        .iter()
        .enumerate()
        .filter(|(_, imp)| import_path_matches_file(&imp.module_path, consumer_file, source_file))
        .collect();

    // Sort by byte range start descending so we edit from end to start
    matching_imports.sort_by(|a, b| b.1.byte_range.start.cmp(&a.1.byte_range.start));

    let mut made_changes = false;

    for (_, imp) in &matching_imports {
        // Match on the imported name (pre-`as`) so `import { foo as bar }`
        // still matches when we're moving `foo`. The TS/JS specifier is
        // stored verbatim (`"foo as bar"`); see `imports::specifier_matches`.
        let has_moved_symbol = imp
            .names
            .iter()
            .any(|n| imports::specifier_matches(n, symbol_name))
            || imp.default_import.as_deref() == Some(symbol_name);

        if !has_moved_symbol {
            continue;
        }

        let new_import_path = compute_relative_import_path(consumer_file, dest_file);

        // Check if this import has other symbols besides the moved one
        let remaining_names: Vec<String> = imp
            .names
            .iter()
            .filter(|n| !imports::specifier_matches(n, symbol_name))
            .cloned()
            .collect();
        let remaining_default = if imp.default_import.as_deref() == Some(symbol_name) {
            None
        } else {
            imp.default_import.clone()
        };

        let type_only = imp.kind == imports::ImportKind::Type;

        // Build the replacement text
        if remaining_names.is_empty() && remaining_default.is_none() {
            // All symbols in this import are moving — replace entire import with new path
            // Preserve the original import structure but change the path
            let new_import = generate_import_with_alias(
                &imp.raw_text,
                symbol_name,
                &new_import_path,
                type_only,
                lang,
            );
            edits.push((imp.byte_range.clone(), new_import));
        } else {
            // Some symbols remain — keep old import for remaining, add new import for moved
            let kept_import = imports::generate_import_line(
                lang,
                &imp.module_path,
                &remaining_names,
                remaining_default.as_deref(),
                type_only,
            );

            // Generate new import for the moved symbol
            let moved_import = generate_import_with_alias(
                &imp.raw_text,
                symbol_name,
                &new_import_path,
                type_only,
                lang,
            );

            let replacement = format!("{}\n{}", kept_import, moved_import);
            edits.push((imp.byte_range.clone(), replacement));
        }
    }

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar_for(lang)).is_ok() {
        if let Some(tree) = parser.parse(content, None) {
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

            exports.sort_by(|a, b| b.byte_range().start.cmp(&a.byte_range().start));
            for node in exports {
                if export_statement_has_wildcard(content, &node) {
                    // TODO: safely rewrite `export * from "..."` once moved-symbol
                    // provenance can distinguish which names are provided by the star.
                    crate::slog_warn!(
                        "move_symbol: leaving wildcard re-export unchanged in {}",
                        consumer_file.display()
                    );
                    continue;
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
                let new_import_path = compute_relative_import_path(consumer_file, dest_file);
                if remaining_specs.is_empty() {
                    edits.push((module_range, new_import_path));
                } else {
                    let old_path = &content[module_range];
                    let replacement = format!(
                        "export {{ {} }} from '{}';\nexport {{ {} }} from '{}';",
                        remaining_specs.join(", "),
                        old_path,
                        moved_specs.join(", "),
                        new_import_path
                    );
                    edits.push((node.byte_range(), replacement));
                }
            }
        }
    }

    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut result = content.to_string();
    for (range, replacement) in edits {
        result = format!(
            "{}{}{}",
            &result[..range.start],
            replacement,
            &result[range.end..]
        );
        made_changes = true;
    }

    if made_changes {
        Some(result)
    } else {
        None
    }
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

/// Generate an import statement preserving any alias from the original import text.
///
/// If the original import has `{ X as Y }`, the new import preserves the alias.
fn generate_import_with_alias(
    original_raw: &str,
    symbol_name: &str,
    new_module_path: &str,
    type_only: bool,
    _lang: LangId,
) -> String {
    // Check if the original import uses an alias for this symbol
    // Pattern: `X as Y` inside braces
    let alias = extract_alias(original_raw, symbol_name);

    let names = if let Some(alias_name) = &alias {
        vec![format!("{} as {}", symbol_name, alias_name)]
    } else {
        vec![symbol_name.to_string()]
    };

    let type_prefix = if type_only { "type " } else { "" };
    let names_str = names.join(", ");
    format!(
        "import {}{{ {} }} from '{}';",
        type_prefix, names_str, new_module_path
    )
}

/// Extract an alias for a symbol from an import statement's raw text.
///
/// Looks for `symbol_name as alias` pattern in the import text.
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
    let cp_store = ctx.checkpoint().borrow();
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
