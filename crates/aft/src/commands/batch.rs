//! Handler for the `batch` command: atomic multi-edit with rollback.
//!
//! Accepts an array of edits (string-match or line-range), validates all against
//! the original content, then applies bottom-to-top to prevent line drift.
//! Takes a single auto-backup before applying. No file modification on failure.

use std::path::Path;

use crate::context::AppContext;
use crate::edit::{self, line_col_to_byte};
use crate::protocol::{RawRequest, Response};

/// A validated edit ready to apply, carrying byte offsets into the original content.
struct ResolvedEdit {
    byte_start: usize,
    byte_end: usize,
    replacement: String,
}

/// Handle a `batch` request.
///
/// Params:
///   - `file` (string, required) — target file path
///   - `edits` (array, required) — each element is either:
///       - `{ "match": "...", "replacement": "..." }` — string match-replace
///       - `{ "line_start": N, "line_end": N, "content": "..." }` — line range replacement (1-based, inclusive)
///
/// Returns on success: `{ file, edits_applied, syntax_valid?, backup_id? }`.
/// `syntax_valid` is absent when syntax validation could not run.
/// Returns on failure: error with the failing edit index.
pub fn handle_batch(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "batch: missing required param 'file'",
            );
        }
    };

    let edits = match req.params.get("edits").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "batch: missing required param 'edits' (expected array)",
            );
        }
    };

    if edits.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "batch: 'edits' array must not be empty",
        );
    }

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
        }
    };

    // Phase 1: Validate all edits and resolve to byte offsets
    let mut resolved: Vec<ResolvedEdit> = Vec::with_capacity(edits.len());

    for (i, edit_val) in edits.iter().enumerate() {
        match resolve_edit(&source, edit_val, i, &req.id) {
            Ok(r) => resolved.push(r),
            Err(resp) => return resp,
        }
    }

    // Phase 2: Auto-backup once before applying
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "batch: pre-batch backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Phase 3: Sort edits by byte_start descending (bottom-to-top) to prevent drift
    resolved.sort_by(|a, b| b.byte_start.cmp(&a.byte_start));

    // Phase 3.5: Detect overlapping byte ranges after sort (sorted descending by byte_start)
    for i in 0..resolved.len().saturating_sub(1) {
        // resolved[i] has a HIGHER byte_start than resolved[i+1]
        let higher = &resolved[i];
        let lower = &resolved[i + 1];
        // Overlap: lower's range extends into higher's range
        if lower.byte_end > higher.byte_start {
            return Response::error(
                &req.id,
                "overlapping_edits",
                format!(
                    "batch: edits overlap — edit at bytes [{}..{}) overlaps with edit at bytes [{}..{})",
                    lower.byte_start, lower.byte_end, higher.byte_start, higher.byte_end
                ),
            );
        }
    }

    // Phase 4: Apply all edits sequentially to the content
    let mut content = source.clone();
    for r in &resolved {
        content = match edit::replace_byte_range(&content, r.byte_start, r.byte_end, &r.replacement)
        {
            Ok(updated) => updated,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };
    }

    // Phase 5: Write, format, and validate via shared pipeline
    let mut write_result =
        match edit::write_format_validate(&path, &content, &ctx.config(), &req.params) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    log::debug!("batch: {} edits in {}", edits.len(), file);

    let mut result = serde_json::json!({
        "file": file,
        "edits_applied": edits.len(),
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

/// Resolve a single edit object to byte offsets against the original source.
///
/// Returns `Ok(ResolvedEdit)` on success, `Err(Response)` on validation failure.
fn resolve_edit(
    source: &str,
    edit_val: &serde_json::Value,
    index: usize,
    req_id: &str,
) -> Result<ResolvedEdit, Response> {
    // Detect edit type: match-replace or line-range
    // Accept both "match"/"replacement" and "oldString"/"newString" (backward compat)
    let match_str = edit_val
        .get("match")
        .or_else(|| edit_val.get("oldString"))
        .and_then(|v| v.as_str());

    if let Some(match_str) = match_str {
        // String match-replace with progressive fuzzy matching (same as edit_match)
        let replacement = edit_val
            .get("replacement")
            .or_else(|| edit_val.get("newString"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let fuzzy_matches = crate::fuzzy_match::find_all_fuzzy(source, match_str);

        if fuzzy_matches.is_empty() {
            return Err(Response::error(
                req_id,
                "batch_edit_failed",
                format!(
                    "batch: edit[{}] match '{}' not found in file",
                    index, match_str
                ),
            ));
        }

        if fuzzy_matches[0].pass > 1 {
            log::debug!(
                "batch: edit[{}] fuzzy match (pass {}) for '{}'",
                index,
                fuzzy_matches[0].pass,
                match_str
            );
        }

        if fuzzy_matches.len() > 1 {
            // Check if an occurrence index is specified to disambiguate
            if let Some(occ) = edit_val.get("occurrence").and_then(|v| v.as_u64()) {
                let occ = occ as usize;
                if occ >= fuzzy_matches.len() {
                    return Err(Response::error(
                        req_id,
                        "batch_edit_failed",
                        format!(
                            "batch: edit[{}] occurrence {} out of range (found {} occurrences)",
                            index,
                            occ,
                            fuzzy_matches.len()
                        ),
                    ));
                }
                let m = &fuzzy_matches[occ];
                return Ok(ResolvedEdit {
                    byte_start: m.byte_start,
                    byte_end: m.byte_start + m.byte_len,
                    replacement: replacement.to_string(),
                });
            }
            return Err(Response::error(
                req_id,
                "batch_edit_failed",
                format!(
                    "batch: edit[{}] match '{}' is ambiguous ({} occurrences, expected 1). Use 'occurrence' field (0-indexed) to select which one.",
                    index,
                    match_str,
                    fuzzy_matches.len()
                ),
            ));
        }

        let m = &fuzzy_matches[0];
        Ok(ResolvedEdit {
            byte_start: m.byte_start,
            byte_end: m.byte_start + m.byte_len,
            replacement: replacement.to_string(),
        })
    } else if edit_val.get("line_start").is_some() {
        // Line-range replacement
        let line_start_1based = edit_val
            .get("line_start")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .ok_or_else(|| {
                Response::error(
                    req_id,
                    "invalid_request",
                    format!(
                        "batch: edit[{}] 'line_start' must be a positive integer (1-based)",
                        index
                    ),
                )
            })?;
        if line_start_1based == 0 {
            return Err(Response::error(
                req_id,
                "invalid_request",
                format!("batch: edit[{}] 'line_start' must be >= 1 (1-based)", index),
            ));
        }
        let line_start = line_start_1based - 1;

        let line_end_1based = edit_val
            .get("line_end")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .ok_or_else(|| {
                Response::error(
                    req_id,
                    "invalid_request",
                    format!(
                        "batch: edit[{}] 'line_end' must be a positive integer (1-based)",
                        index
                    ),
                )
            })?;
        if line_end_1based == 0 {
            return Err(Response::error(
                req_id,
                "invalid_request",
                format!("batch: edit[{}] 'line_end' must be >= 1 (1-based)", index),
            ));
        }
        let line_end = line_end_1based - 1;

        let content = edit_val
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let lines: Vec<&str> = source.lines().collect();
        let total_lines = lines.len();

        // Allow line_start == total_lines for appending at end of file
        if line_start > total_lines {
            return Err(Response::error(
                req_id,
                "batch_edit_failed",
                format!(
                    "batch: edit[{}] line_start {} out of range (file has {} lines)",
                    index, line_start_1based, total_lines
                ),
            ));
        }

        // Append at EOF: line_start == total_lines means insert after last line
        if line_start == total_lines {
            let byte_pos = source.len();
            let mut replacement_str = content.to_string();
            if !source.ends_with('\n') && !replacement_str.starts_with('\n') {
                replacement_str.insert(0, '\n');
            }
            if !replacement_str.ends_with('\n') {
                replacement_str.push('\n');
            }
            return Ok(ResolvedEdit {
                byte_start: byte_pos,
                byte_end: byte_pos,
                replacement: replacement_str,
            });
        }

        // Allow pure insert: line_start == line_end + 1 means insert before line_start
        if line_start > line_end + 1 {
            return Err(Response::error(
                req_id,
                "invalid_request",
                format!(
                    "batch: edit[{}] line_start {} > line_end {}",
                    index, line_start_1based, line_end_1based
                ),
            ));
        }

        // Pure insert mode: line_start == line_end + 1 (zero-length range)
        if line_start == line_end + 1
            || (line_end == 0
                && line_start == 0
                && edit_val
                    .get("line_end")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    == Some(0)
                && line_start > line_end)
        {
            let byte_pos = line_col_to_byte(source, line_start as u32, 0);
            let mut replacement_str = content.to_string();
            if !replacement_str.ends_with('\n') {
                replacement_str.push('\n');
            }
            return Ok(ResolvedEdit {
                byte_start: byte_pos,
                byte_end: byte_pos,
                replacement: replacement_str,
            });
        }

        // Clamp line_end to last valid line instead of hard error
        let line_end = if line_end >= total_lines {
            total_lines - 1
        } else {
            line_end
        };

        // Convert line range to byte offsets
        let byte_start = line_col_to_byte(source, line_start as u32, 0);
        // line_end is inclusive: end byte is at the end of line_end (including its newline if present)
        let byte_end = line_col_to_byte(source, line_end.saturating_add(1) as u32, 0);

        // Empty content = delete lines entirely (no trailing newline added)
        // Non-empty content = if the replaced range had a trailing newline, auto-append
        // one to prevent the next line from merging.
        let mut replacement_str = content.to_string();
        if !replacement_str.is_empty() {
            let range_has_trailing_nl = byte_end > 0
                && byte_end <= source.len()
                && source.as_bytes()[byte_end - 1] == b'\n';
            if range_has_trailing_nl && !replacement_str.ends_with('\n') {
                replacement_str.push('\n');
            }
        }

        Ok(ResolvedEdit {
            byte_start,
            byte_end,
            replacement: replacement_str,
        })
    } else {
        Err(Response::error(
            req_id,
            "invalid_request",
            format!(
                "batch: edit[{}] must have either 'match' or 'line_start'/'line_end'",
                index
            ),
        ))
    }
}
