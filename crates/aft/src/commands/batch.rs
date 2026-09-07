//! Handler for the `batch` command: atomic multi-edit with rollback.
//!
//! Accepts an array of edits (string-match or line-range), validates all against
//! the original content, then applies bottom-to-top to prevent line drift.
//! Takes a single auto-backup before applying. No file modification on failure.

use std::path::Path;

use serde_json::Value;

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
            Ok(item_edits) => resolved.extend(item_edits),
            Err(resp) => return resp,
        }
    }

    // Phase 2: Sort edits by byte_start descending (bottom-to-top) to prevent drift
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

    if edit::wants_preview(&req.params) {
        let mut result = serde_json::json!({
            "file": file,
            "edits_applied": edits.len(),
            "formatted": false,
        });
        if source == content {
            result["no_op"] = serde_json::json!(true);
        }
        edit::attach_preview_diff(&mut result, &req.params, file, &source, &content);
        return Response::success(&req.id, result);
    }

    // Phase 5: Auto-backup once before applying
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

    // Phase 6: Write, format, and validate via shared pipeline
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

    // Aggregate diff counts across all applied edits, computed from the
    // original source vs the final (post-format) on-disk content. Without
    // this, batch responses reported `edits_applied` but +0/-0, so the
    // agent-facing summary said "Edited (+0/-0, N edits)" even when content
    // changed. Mirrors the edit_match find/replace path.
    if edit::wants_diff(&req.params) {
        let final_content = std::fs::read_to_string(&path).unwrap_or_else(|_| content.clone());
        result["diff"] = edit::compute_diff_for_response(&req.params, &source, &final_content);
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
    edit::attach_backup_skipped_reason(
        &mut result,
        ctx,
        req.session(),
        &op_id,
        Some(path.as_path()),
    );

    write_result.append_lsp_diagnostics_to(&mut result);
    write_result.append_reformatted_excerpt_to(&mut result);
    Response::success(&req.id, result)
}

/// Build the context shown for one ambiguous match.
fn build_context(source: &str, target_line: usize, margin: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = target_line.saturating_sub(margin);
    let end = (target_line + margin + 1).min(lines.len());
    lines[start..end].join("\n")
}

/// Resolve a single edit object to byte offsets against the original source.
///
/// Returns one or more resolved edits on success, or `Err(Response)` on validation failure.
fn resolve_edit(
    source: &str,
    edit_val: &serde_json::Value,
    index: usize,
    req_id: &str,
) -> Result<Vec<ResolvedEdit>, Response> {
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
        let raw_occurrence_value = edit_val.get("occurrence");
        let replace_all = edit_val
            .get("replace_all")
            .or_else(|| edit_val.get("replaceAll"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if replace_all && edit_val.get("occurrence").is_some() {
            return Err(Response::error(
                req_id,
                "invalid_request",
                format!(
                    "batch: edit[{}] 'replaceAll' and 'occurrence' are mutually exclusive",
                    index
                ),
            ));
        }

        let occurrence = match raw_occurrence_value {
            None => None,
            Some(Value::Number(number)) => {
                let value = number.as_u64().or_else(|| {
                    number.as_f64().and_then(|value| {
                        (value.is_finite() && value.fract() == 0.0 && value >= 1.0)
                            .then_some(value as u64)
                    })
                });
                // Width-independent bound: compare in u64 before converting, so the
                // full contract domain stays valid regardless of target usize width.
                match value {
                    Some(value) if value >= 1 && value - 1 <= usize::MAX as u64 => {
                        Some((value - 1) as usize)
                    }
                    _ => {
                        return Err(Response::error(
                            req_id,
                            "invalid_request",
                            format!(
                                "batch: edit[{}] 'occurrence' must be a positive integer (1-based)",
                                index
                            ),
                        ));
                    }
                }
            }
            Some(_) => {
                return Err(Response::error(
                    req_id,
                    "invalid_request",
                    format!(
                        "batch: edit[{}] 'occurrence' must be a positive integer (1-based)",
                        index
                    ),
                ));
            }
        };

        let fuzzy_matches = crate::fuzzy_match::find_all_fuzzy(source, match_str);

        // Fuzzy passes (>= 2) are line-based and set the byte range to include
        // the trailing newline after the last matched line, even when the user's
        // match had none. Applying `newString` verbatim over that range merges
        // the last replaced line with the next one (#83). Re-append a newline
        // when the matched range ended in one and the replacement does not —
        // mirrors edit_match.rs and batch's own line-range mode. The exact pass
        // (1) matches byte-for-byte, so its trailing newline is left untouched.
        let effective_replacement = |m: &crate::fuzzy_match::FuzzyMatch| -> String {
            let byte_end = m.byte_start + m.byte_len;
            let range_has_trailing_nl = m.pass >= 2
                && byte_end > 0
                && byte_end <= source.len()
                && source.as_bytes()[byte_end - 1] == b'\n';
            if range_has_trailing_nl && !replacement.is_empty() && !replacement.ends_with('\n') {
                format!("{replacement}\n")
            } else {
                replacement.to_string()
            }
        };

        if fuzzy_matches.is_empty() {
            let message = format!(
                "batch: edit[{}] match '{}' not found in file{}",
                index,
                match_str,
                crate::fuzzy_match::render_nearest_miss_detail(source, match_str),
            );
            return Err(Response::error(req_id, "batch_edit_failed", message));
        }

        if fuzzy_matches[0].pass > 1 {
            log::debug!(
                "batch: edit[{}] fuzzy match (pass {}) for '{}'",
                index,
                fuzzy_matches[0].pass,
                match_str
            );
        }

        if replace_all {
            return Ok(fuzzy_matches
                .iter()
                .map(|m| ResolvedEdit {
                    byte_start: m.byte_start,
                    byte_end: m.byte_start + m.byte_len,
                    replacement: effective_replacement(m),
                })
                .collect());
        }

        if let Some(occ) = occurrence {
            if occ >= fuzzy_matches.len() {
                return Err(Response::error(
                    req_id,
                    "batch_edit_failed",
                    format!(
                        "batch: edit[{}] occurrence {} out of range (found {} occurrences; 'occurrence' is 1-based)",
                        index,
                        occ + 1,
                        fuzzy_matches.len()
                    ),
                ));
            }
            let m = &fuzzy_matches[occ];
            return Ok(vec![ResolvedEdit {
                byte_start: m.byte_start,
                byte_end: m.byte_start + m.byte_len,
                replacement: effective_replacement(m),
            }]);
        }

        if fuzzy_matches.len() > 1 {
            let occurrences: Vec<serde_json::Value> = fuzzy_matches
                .iter()
                .enumerate()
                .map(|(occurrence_index, matched)| {
                    let line = source[..matched.byte_start].matches('\n').count();
                    let context = build_context(source, line, 2);
                    serde_json::json!({
                        "occurrence": occurrence_index + 1,
                        "line": line + 1,
                        "context": context,
                    })
                })
                .collect();
            let occurrence_positions = fuzzy_matches
                .iter()
                .map(|matched| matched.byte_start)
                .collect::<Vec<_>>();
            return Err(Response::error_with_data(
                req_id,
                "ambiguous_match",
                format!(
                    "batch: edits[{}] match '{}' is ambiguous ({} occurrences, expected 1). Use 'occurrence' (1-based) to select one, or 'replaceAll': true to replace every occurrence.{}",
                    index,
                    match_str,
                    fuzzy_matches.len(),
                    crate::fuzzy_match::render_occurrence_listing(source, &occurrence_positions),
                ),
                serde_json::json!({ "occurrences": occurrences }),
            ));
        }

        let m = &fuzzy_matches[0];
        Ok(vec![ResolvedEdit {
            byte_start: m.byte_start,
            byte_end: m.byte_start + m.byte_len,
            replacement: effective_replacement(m),
        }])
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
            // An empty file has no preceding line to separate from the append.
            // Only add a leading newline when existing content needs separation.
            if !source.is_empty() && !source.ends_with('\n') && !replacement_str.starts_with('\n') {
                replacement_str.insert(0, '\n');
            }
            if !replacement_str.ends_with('\n') {
                replacement_str.push('\n');
            }
            return Ok(vec![ResolvedEdit {
                byte_start: byte_pos,
                byte_end: byte_pos,
                replacement: replacement_str,
            }]);
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
            return Ok(vec![ResolvedEdit {
                byte_start: byte_pos,
                byte_end: byte_pos,
                replacement: replacement_str,
            }]);
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

        Ok(vec![ResolvedEdit {
            byte_start,
            byte_end,
            replacement: replacement_str,
        }])
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
