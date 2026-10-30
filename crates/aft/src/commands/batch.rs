//! Handler for the `batch` command: atomic multi-edit with rollback.
//!
//! Accepts an array of edits (string-match or line-range), validates all against
//! the original content, then assembles resolved spans without line drift.
//! Takes a single auto-backup before applying. No file modification on failure.

use std::path::Path;

use serde_json::Value;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// A validated edit ready to apply, carrying byte offsets into the original content.
struct ResolvedEdit {
    byte_start: usize,
    byte_end: usize,
    replacement: String,
    fuzzy_match: Option<serde_json::Value>,
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

    let (source, observation) = match edit::read_edit_source(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
        }
    };

    // Phase 1: Validate all edits and resolve to byte offsets
    let mut resolved: Vec<ResolvedEdit> = Vec::with_capacity(edits.len());

    let line_offsets = edit::LineOffsets::new(&source);
    for (i, edit_val) in edits.iter().enumerate() {
        match resolve_edit_with_offsets(
            &source,
            &line_offsets,
            edit_val,
            i,
            &req.id,
            Some(path.as_path()),
        ) {
            Ok(item_edits) => resolved.extend(item_edits),
            Err(resp) => return resp,
        }
    }

    let fuzzy_matches: Vec<_> = resolved
        .iter()
        .filter_map(|r| r.fuzzy_match.clone())
        .collect();

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

    // Construct the final output in original-range order.
    let content = match build_batch_output(&source, &resolved) {
        Ok(content) => content,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    if edit::wants_preview(&req.params) {
        let mut result = serde_json::json!({
            "file": file,
            "edits_applied": edits.len(),
            "formatted": false,
        });
        if source == content {
            result["no_op"] = serde_json::json!(true);
        }
        if !fuzzy_matches.is_empty() {
            result["fuzzy_matches"] = serde_json::json!(fuzzy_matches);
        }
        edit::attach_preview_diff(&mut result, &req.params, file, &source, &content);
        return Response::success(&req.id, result);
    }

    if source == content {
        return edit::no_change_response(&req.id);
    }

    // Phase 5: Auto-backup once before applying
    let backup_id = match edit::auto_backup_observed(
        ctx,
        req.session(),
        &path,
        "batch: pre-batch backup",
        Some(&op_id),
        &observation,
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // Phase 6: Write, format, and validate via shared pipeline
    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let mut write_result = match edit::write_format_validate_observed(
        tool_policy,
        &path,
        &content,
        &ctx.config(),
        &req.params,
        &source,
        &observation,
    ) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if write_result.rolled_back {
        ctx.backup()
            .lock()
            .discard_operation_entries(req.session(), &op_id);
    }

    if let Some(final_content) = write_result.final_content(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, final_content, &req.params);
    }

    log::debug!("batch: {} edits in {}", edits.len(), file);

    let mut result = serde_json::json!({
        "file": file,
        "edits_applied": edits.len(),
        "formatted": write_result.formatted,
    });

    if !fuzzy_matches.is_empty() {
        result["fuzzy_matches"] = serde_json::json!(fuzzy_matches);
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
    let final_content = write_result.final_content(&path).unwrap_or(&content);
    edit::attach_edit_diff(&mut result, &req.params, file, &source, final_content);
    Response::success(&req.id, result)
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[test]
    fn empty_batch_match_is_rejected_before_expansion() {
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"replaceAll": true}),
        ] {
            let mut edit = serde_json::json!({"match": "", "replacement": "x"});
            edit.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let result = resolve_edit("é\ntext\n", &edit, 0, "empty-match");
            assert!(matches!(result, Err(response) if response.data["code"] == "invalid_request"));
        }
    }

    #[test]
    fn batch_rollback_discards_operation_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.ts");
        let original = "export function ok() { return 1; }\n";
        std::fs::write(&path, original).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
        );
        let req: RawRequest = serde_json::from_value(serde_json::json!({"id": "rollback", "command": "batch", "file": path, "edits": [{"match": original, "replacement": "function { broken"}]})).unwrap();
        let response = handle_batch(&req, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["rolled_back"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(ctx
            .backup()
            .lock()
            .preview_last_operation_paths(req.session())
            .is_err());
    }
}

fn build_batch_output(
    source: &str,
    resolved: &[ResolvedEdit],
) -> Result<String, crate::error::AftError> {
    let mut output_len = source.len();
    for r in resolved {
        edit::validate_byte_range(source, r.byte_start, r.byte_end)?;
        output_len = output_len - (r.byte_end - r.byte_start) + r.replacement.len();
    }
    #[cfg(test)]
    BATCH_OUTPUT_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    let mut output = String::with_capacity(output_len);
    let mut cursor = 0;
    // Reverse the stable descending sort, including ties: the last insertion
    // applied at an offset appears first in the old bottom-to-top result.
    for r in resolved.iter().rev() {
        output.push_str(&source[cursor..r.byte_start]);
        output.push_str(&r.replacement);
        cursor = r.byte_end;
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

/// Resolve a single edit object to byte offsets against the original source.
///
/// Returns one or more resolved edits on success, or `Err(Response)` on validation failure.
#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn batch_match_requires_explicit_string_replacement() {
        let source = "one\ntwo\n";
        let mut edits = vec![
            serde_json::json!({"match": "one"}),
            serde_json::json!({"oldString": "one"}),
        ];
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!(5),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            edits.push(serde_json::json!({"match": "one", "replacement": invalid}));
            edits.push(serde_json::json!({"oldString": "one", "newString": invalid}));
        }
        for edit in edits {
            assert!(
                matches!(resolve_edit(source, &edit, 0, "required-text"), Err(response) if response.data["code"] == "invalid_request"),
                "malformed edit must not become deletion: {edit}"
            );
        }
        for edit in [
            serde_json::json!({"match": "one", "replacement": ""}),
            serde_json::json!({"oldString": "one", "newString": ""}),
        ] {
            assert!(resolve_edit(source, &edit, 0, "explicit-delete").is_ok());
        }
    }

    #[test]
    fn batch_range_requires_explicit_string_content() {
        let source = "one\ntwo\n";
        let mut edits = vec![serde_json::json!({"line_start": 1, "line_end": 1})];
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!(5),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            edits.push(serde_json::json!({"line_start": 1, "line_end": 1, "content": invalid}));
        }
        for edit in edits {
            assert!(
                matches!(resolve_edit(source, &edit, 0, "required-text"), Err(response) if response.data["code"] == "invalid_request"),
                "malformed edit must not become deletion: {edit}"
            );
        }
        assert!(resolve_edit(
            source,
            &serde_json::json!({"line_start": 1, "line_end": 1, "content": ""}),
            0,
            "explicit-delete"
        )
        .is_ok());
    }
}

/// Test convenience for resolving one edit without a shared line-offset table.
#[cfg(test)]
fn resolve_edit(
    source: &str,
    edit_val: &serde_json::Value,
    index: usize,
    req_id: &str,
) -> Result<Vec<ResolvedEdit>, Response> {
    let line_offsets = edit::LineOffsets::new(source);
    resolve_edit_with_offsets(source, &line_offsets, edit_val, index, req_id, None)
}

/// Resolve one edit, reusing line starts computed once for the whole batch.
fn resolve_edit_with_offsets(
    source: &str,
    line_offsets: &edit::LineOffsets,
    edit_val: &serde_json::Value,
    index: usize,
    req_id: &str,
    path: Option<&Path>,
) -> Result<Vec<ResolvedEdit>, Response> {
    // Detect edit type: match-replace or line-range
    // Accept both "match"/"replacement" and "oldString"/"newString" (backward compat)
    let match_str = edit_val
        .get("match")
        .or_else(|| edit_val.get("oldString"))
        .and_then(|v| v.as_str());

    if let Some(match_str) = match_str {
        if match_str.is_empty() {
            return Err(Response::error(
                req_id,
                "invalid_request",
                format!("batch: edit[{}] 'match' must not be empty", index),
            ));
        }
        // String match-replace with progressive fuzzy matching (same as edit_match)
        let replacement = edit_val
            .get("replacement")
            .or_else(|| edit_val.get("newString"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Response::error(req_id, "invalid_request", format!("batch: edit[{}] requires a string 'replacement' or 'newString' (use an empty string to delete)", index)))?;
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

        // A plan covers every match one replacement applies to, so finding
        // string lines parses the file at most once for all of them.
        let effective_replacement = |plan: &crate::fuzzy_match::ReindentPlan<'_>,
                                     at: usize|
         -> Result<String, Response> {
            let mut output = String::new();
            super::edit_match::push_fuzzy_replacement(&mut output, plan, at).map_err(|error| {
                Response::error(
                    req_id,
                    error.code(),
                    format!("batch: edit[{}] {}", index, error),
                )
            })?;
            Ok(output)
        };

        if fuzzy_matches.is_empty() {
            let message = format!(
                "batch: edit[{}] match {} not found in file{}",
                index,
                crate::fuzzy_match::describe_searched_text(match_str),
                crate::fuzzy_match::render_edit_not_found_detail(source, match_str, replacement),
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
            let plan = crate::fuzzy_match::ReindentPlan::new(
                source,
                match_str,
                replacement,
                path,
                &fuzzy_matches,
            );
            return fuzzy_matches
                .iter()
                .enumerate()
                .map(|(at, m)| {
                    Ok(ResolvedEdit {
                        byte_start: m.byte_start,
                        byte_end: m.byte_start + m.byte_len,
                        replacement: effective_replacement(&plan, at)?,
                        fuzzy_match: (m.pass > 1).then(|| {
                            crate::fuzzy_match::replacement_detail(
                                source,
                                match_str,
                                std::slice::from_ref(m),
                            )
                        }),
                    })
                })
                .collect();
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
                replacement: effective_replacement(
                    &crate::fuzzy_match::ReindentPlan::new(
                        source,
                        match_str,
                        replacement,
                        path,
                        std::slice::from_ref(m),
                    ),
                    0,
                )?,
                fuzzy_match: (m.pass > 1).then(|| {
                    crate::fuzzy_match::replacement_detail(
                        source,
                        match_str,
                        std::slice::from_ref(m),
                    )
                }),
            }]);
        }

        if fuzzy_matches.len() > 1 {
            let line_index = edit::SourceLineIndex::new(source);
            let occurrences: Vec<serde_json::Value> = fuzzy_matches
                .iter()
                .enumerate()
                .map(|(occurrence_index, matched)| {
                    let line = line_index.line_at(matched.byte_start);
                    let context = line_index.context(line, 2);
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
            replacement: effective_replacement(
                &crate::fuzzy_match::ReindentPlan::new(
                    source,
                    match_str,
                    replacement,
                    path,
                    std::slice::from_ref(m),
                ),
                0,
            )?,
            fuzzy_match: (m.pass > 1).then(|| {
                crate::fuzzy_match::replacement_detail(source, match_str, std::slice::from_ref(m))
            }),
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
            .ok_or_else(|| {
                Response::error(
                    req_id,
                    "invalid_request",
                    format!(
                        "batch: edit[{}] requires string 'content' (use an empty string to delete)",
                        index
                    ),
                )
            })?;

        let total_lines = line_offsets.line_count();

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
                fuzzy_match: None,
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
            let byte_pos = line_offsets.offset(line_start);
            let mut replacement_str = content.to_string();
            if !replacement_str.ends_with('\n') {
                replacement_str.push('\n');
            }
            return Ok(vec![ResolvedEdit {
                byte_start: byte_pos,
                byte_end: byte_pos,
                replacement: replacement_str,
                fuzzy_match: None,
            }]);
        }

        // Clamp line_end to last valid line instead of hard error
        let line_end = if line_end >= total_lines {
            total_lines - 1
        } else {
            line_end
        };

        // Convert line range to byte offsets
        let byte_start = line_offsets.offset(line_start);
        // line_end is inclusive: end byte is at the end of line_end (including its newline if present)
        let byte_end = line_offsets.offset(line_end.saturating_add(1));

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
            fuzzy_match: None,
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

#[cfg(test)]
thread_local! {
    static BATCH_OUTPUT_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod a03_tests {
    use super::*;
    use crate::{config::Config, language::StubProvider};

    fn request(file: &Path, edits: Value, preview: bool) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "a03", "command": "batch", "file": file,
            "edits": edits, "preview": preview, "include_diff_content": true
        }))
        .unwrap()
    }

    /// The host `edit` tool sends a single-file `oldString` miss through this
    /// batch path. Its error should name the miss without pasting the whole
    /// match back; the nearest-miss excerpt that follows is already bounded.
    #[test]
    fn not_found_error_does_not_repeat_a_large_match() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let needle: String = (0..400)
            .map(|index| format!("    let absent_{index} = 0;\n"))
            .collect();
        let response = handle_batch(
            &request(
                &path,
                serde_json::json!([{"match": needle, "replacement": "x"}]),
                false,
            ),
            &ctx,
        );
        assert!(!response.success);
        assert_eq!(response.data["code"], "batch_edit_failed");
        let message = response.data["message"].as_str().unwrap();
        assert!(
            !message.contains(&needle),
            "{} byte match repeated in a {} byte error",
            needle.len(),
            message.len()
        );
        assert!(message.len() < 4 * 1024, "{} byte error", message.len());
        assert!(
            message.starts_with("batch: edit[0] match '    let absent_0 = 0;…' (400 lines, 9490 bytes) not found in "),
            "{message}"
        );
    }

    #[test]
    fn a03_batch_output_allocation_bound() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.txt");
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let mut observed = Vec::new();
        for count in [1, 100, 2_000] {
            let source = format!("{}{}", "TARGET\n".repeat(count), "z".repeat(1024 * 1024));
            std::fs::write(&path, &source).unwrap();
            BATCH_OUTPUT_ALLOCATIONS.with(|counter| counter.set(0));
            let start = std::time::Instant::now();
            let response = handle_batch(
                &request(
                    &path,
                    serde_json::json!([
                        {"match": "TARGET", "replacement": "é", "replace_all": true}
                    ]),
                    true,
                ),
                &ctx,
            );
            let elapsed = start.elapsed();
            assert!(response.success, "{:?}", response);
            let expected = format!("{}{}", "é\n".repeat(count), "z".repeat(1024 * 1024));
            assert_eq!(
                response.data["metadata"]["diff"],
                edit::build_unified_diff(path.to_str().unwrap(), &source, &expected)
            );
            let allocations = BATCH_OUTPUT_ALLOCATIONS.with(|counter| counter.get());
            eprintln!(
                "A03 batch matches={count}: output allocations={allocations} elapsed={elapsed:?}"
            );
            observed.push(allocations);
        }
        assert_eq!(
            observed,
            [1, 1, 1],
            "one batch output allocation per request"
        );
    }

    #[test]
    fn a03_ambiguity_one_index_pass() {
        let mut observed = Vec::new();
        for count in [2, 8, 9, 20_000] {
            let source = format!("{}end", "é TARGET long context\r\n".repeat(count));
            edit::A03_LINE_INDEX_PASSES.with(|counter| counter.set(0));
            let response = resolve_edit(
                &source,
                &serde_json::json!({"match": "TARGET", "replacement": "x"}),
                0,
                "a03",
            )
            .err()
            .unwrap();
            let occurrences = &response.data["occurrences"];
            let lines: Vec<_> = source.lines().collect();
            let expected: Vec<_> = (0..count).map(|line| serde_json::json!({
                "occurrence": line + 1, "line": line + 1,
                "context": lines[line.saturating_sub(2)..(line+3).min(lines.len())].join("\n")
            })).collect();
            assert_eq!(occurrences, &serde_json::json!(expected));
            let passes = edit::A03_LINE_INDEX_PASSES.with(|counter| counter.get());
            eprintln!("A03 ambiguity matches={count}: index passes={passes}");
            observed.push(passes);
        }
        assert_eq!(
            observed,
            [1, 1, 1, 1],
            "one source index for all ambiguity entries"
        );
    }

    #[test]
    fn a03_batch_builder_edge_goldens() {
        for (source, descending, expected) in [
            ("abcdef", vec![(4, 6, "世界"), (1, 3, "")], "ad世界"),
            ("aé中z", vec![(3, 6, "🙂🙂"), (0, 1, "long")], "longé🙂🙂z"),
            (
                "abc",
                vec![(1, 1, "FIRST"), (1, 1, "SECOND")],
                "aSECONDFIRSTbc",
            ),
            ("abc", vec![(1, 2, "R"), (1, 1, "I")], "aIRc"),
            ("", vec![(0, 0, "A"), (0, 0, "B")], "BA"),
        ] {
            let resolved: Vec<_> = descending
                .into_iter()
                .map(|(byte_start, byte_end, replacement)| ResolvedEdit {
                    byte_start,
                    byte_end,
                    replacement: replacement.into(),
                    fuzzy_match: None,
                })
                .collect();
            assert_eq!(build_batch_output(source, &resolved).unwrap(), expected);
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fuzzy.txt");
        std::fs::write(&path, "  TARGET  \r\nsecond\n").unwrap();
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let response = handle_batch(
            &request(
                &path,
                serde_json::json!([{"match": "TARGET", "replacement": "grow\nmore"}]),
                true,
            ),
            &ctx,
        );
        assert!(response.success, "{:?}", response);
        // The exact substring pass preserves whitespace and the original CRLF.
        assert_eq!(response.data["diff"]["after"], "  grow\nmore  \r\nsecond\n");
        let source = "  TARGET  \r\n  NEXT  \r\nend\n";
        std::fs::write(&path, source).unwrap();
        let response = handle_batch(
            &request(
                &path,
                serde_json::json!([{"match": "TARGET\nNEXT", "replacement": "é"}]),
                true,
            ),
            &ctx,
        );
        assert!(response.success, "{:?}", response);
        let mut frozen = resolve_edit(
            source,
            &serde_json::json!({"match": "TARGET\nNEXT", "replacement": "é"}),
            0,
            "golden",
        )
        .unwrap_or_else(|_| panic!("fuzzy fixture must resolve"));
        frozen.sort_by(|a, b| b.byte_start.cmp(&a.byte_start));
        let mut legacy = source.to_owned();
        for r in frozen {
            legacy = edit::replace_byte_range(&legacy, r.byte_start, r.byte_end, &r.replacement)
                .unwrap();
        }
        assert_eq!(legacy, "  é  \r\nend\n");
        assert_eq!(response.data["diff"]["after"], legacy);
    }

    #[test]
    fn a03_single_match_ambiguity_payload_golden() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("long.txt");
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        let source = format!(
            "α\r\n{}\r\nTARGET TARGET\nlast",
            "TARGET".to_owned() + &"中".repeat(10_000)
        );
        std::fs::write(&path, &source).unwrap();
        let req: RawRequest = serde_json::from_value(serde_json::json!({"id": "ambiguity", "command": "edit_match", "file": path, "match": "TARGET", "replacement": "x"})).unwrap();
        edit::A03_LINE_INDEX_PASSES.with(|count| count.set(0));
        let response = crate::commands::edit_match::handle_edit_match(&req, &ctx);
        assert!(!response.success);
        let expected = vec![
            serde_json::json!({"occurrence": 1, "line": 2, "context": source.lines().collect::<Vec<_>>().join("\n")}),
            serde_json::json!({"occurrence": 2, "line": 3, "context": source.lines().collect::<Vec<_>>().join("\n")}),
            serde_json::json!({"occurrence": 3, "line": 3, "context": source.lines().collect::<Vec<_>>().join("\n")}),
        ];
        assert_eq!(response.data["occurrences"], serde_json::json!(expected));
        assert_eq!(edit::A03_LINE_INDEX_PASSES.with(|count| count.get()), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a03_write_handler_new_existing_skip_and_preview_counts() {
        let temp = tempfile::tempdir().unwrap();
        for extension in ["ts", "txt"] {
            for existed in [false, true] {
                for preview in [false, true] {
                    for include_diff in [false, true] {
                        let path = temp
                            .path()
                            .join(format!("{existed}-{preview}-{include_diff}.{extension}"));
                        if existed {
                            std::fs::write(&path, "const value = 1;\n").unwrap();
                        }
                        let mut config = Config::default();
                        config.backup.enabled = Some(false);
                        let ctx = AppContext::new(Box::new(StubProvider), config);
                        let req: RawRequest = serde_json::from_value(serde_json::json!({"id": "counts", "command": "write", "file": path, "content": "const value = 2;\n", "preview": preview, "include_diff": include_diff})).unwrap();
                        edit::A03_HANDLER_READS.with(|count| count.set(0));
                        edit::A03_SYNTAX_PARSES.with(|count| count.set(0));
                        crate::backup::reset_capture_read_count(&path);
                        let response = crate::commands::write::handle_write(&req, &ctx);
                        assert!(response.success, "{:?}", response);
                        assert_eq!(response.data["created"], !existed);
                        assert_eq!(
                            response.data["metadata"]["diff"],
                            edit::build_unified_diff(
                                path.to_str().unwrap(),
                                if existed { "const value = 1;\n" } else { "" },
                                "const value = 2;\n"
                            )
                        );
                        let reads = edit::A03_HANDLER_READS.with(|count| count.get())
                            + crate::backup::capture_read_count(&path);
                        assert_eq!(reads, usize::from(existed) + usize::from(!preview), "write existed={existed} preview={preview} diff={include_diff} lang={extension}");
                        assert_eq!(
                            edit::A03_SYNTAX_PARSES.with(|count| count.get()),
                            if preview || extension == "txt" {
                                0
                            } else {
                                1 + usize::from(existed)
                            }
                        );
                        assert_eq!(path.exists(), existed || !preview);
                        if preview && existed {
                            assert_eq!(
                                std::fs::read_to_string(&path).unwrap(),
                                "const value = 1;\n"
                            );
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a03_live_fingerprint_and_undo_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("live.txt");
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        std::fs::write(&path, "é before\r\n").unwrap();
        crate::backup::A03_FINGERPRINT_HASHES.with(|count| count.set(0));
        crate::backup::A03_LIVE_FINGERPRINT_READS.with(|count| count.set(0));
        crate::backup::reset_capture_read_count(&path);
        edit::A03_HANDLER_READS.with(|count| count.set(0));
        let req = request(
            &path,
            serde_json::json!([{"match": "before", "replacement": "after"}]),
            false,
        );
        let response = handle_batch(&req, &ctx);
        assert!(response.success, "{:?}", response);
        assert_eq!(response.data["diff"]["after"], "é after\r\n");
        // Finalization must fingerprint the live path, not the cached reply text.
        std::fs::write(&path, "external post-state\n").unwrap();
        let mut store = ctx.backup().lock();
        store.record_post_mutation_states();
        let hashes = crate::backup::A03_FINGERPRINT_HASHES.with(|count| count.get());
        let live_reads = crate::backup::A03_LIVE_FINGERPRINT_READS.with(|count| count.get());
        let reads = edit::A03_HANDLER_READS.with(|count| count.get())
            + crate::backup::capture_read_count(&path)
            + live_reads;
        eprintln!("A03 handler + finalizer reads={reads}, hashes={hashes}, live fingerprint reads={live_reads}");
        assert_eq!(live_reads, 1);
        assert_eq!(reads, 3);
        assert_eq!(hashes, 2);
        let history = store.history(req.session(), &path);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].content_bytes().unwrap().as_ref(),
            "é before\r\n".as_bytes()
        );
        assert_eq!(
            history[0].post_state,
            Some(crate::backup::PathFingerprint::Content(
                blake3::hash(b"external post-state\n").to_hex().to_string()
            ))
        );
        store.restore_last_operation(req.session()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), "é before\r\n".as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn a03_handler_buffer_read_bound() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.ts");
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        for preview in [true, false] {
            std::fs::write(&path, "const value = 1;\n").unwrap();
            crate::backup::reset_capture_read_count(&path);
            edit::A03_HANDLER_READS.with(|counter| counter.set(0));
            edit::A03_SYNTAX_PARSES.with(|counter| counter.set(0));
            let response = handle_batch(
                &request(
                    &path,
                    serde_json::json!([
                        {"match": "1", "replacement": "2"}
                    ]),
                    preview,
                ),
                &ctx,
            );
            assert!(response.success, "{:?}", response);
            let reads = edit::A03_HANDLER_READS.with(|counter| counter.get())
                + crate::backup::capture_read_count(&path);
            let parses = edit::A03_SYNTAX_PARSES.with(|counter| counter.get());
            eprintln!("A03 batch preview={preview}: handler reads={reads}, syntax parses={parses}");
            assert_eq!(parses, if preview { 0 } else { 2 });
            assert_eq!(
                reads,
                if preview { 1 } else { 2 },
                "handler-wide content reads"
            );
        }
    }
}

#[cfg(test)]
mod a03_measurements {
    use super::*;
    use crate::{config::Config, language::StubProvider};

    #[test]
    #[ignore = "release-only component and handler timing probe"]
    fn a03_release_measurements() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.txt");
        let ctx = AppContext::new(Box::new(StubProvider), Config::default());
        for bytes in [10 * 1024, 1024 * 1024, 10 * 1024 * 1024] {
            let source = "x".repeat(bytes) + "TARGET\n";
            std::fs::write(&path, &source).unwrap();
            let req: RawRequest = serde_json::from_value(serde_json::json!({"id": "measure", "command": "batch", "file": path, "edits": [{"match": "TARGET", "replacement": "DONE"}], "include_diff_content": true})).unwrap();
            let start = std::time::Instant::now();
            let response = handle_batch(&req, &ctx);
            assert!(response.success, "{:?}", response);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "x".repeat(bytes) + "DONE\n"
            );
            eprintln!("A03 handler bytes={bytes} elapsed={:?}", start.elapsed());
        }
        for count in [1, 100, 2_000] {
            let source = "x".repeat(1024 * 1024);
            let edits: Vec<_> = (0..count)
                .map(|i| ResolvedEdit {
                    byte_start: i * 2,
                    byte_end: i * 2 + 1,
                    replacement: "y".into(),
                    fuzzy_match: None,
                })
                .rev()
                .collect();
            let start = std::time::Instant::now();
            let content = build_batch_output(&source, &edits).unwrap();
            assert_eq!(&content[..count * 2], "yx".repeat(count));
            eprintln!(
                "A03 batch builder matches={count} elapsed={:?}",
                start.elapsed()
            );
        }
        for reversed in [false, true] {
            let before: String = (0..20_000)
                .map(|i| format!("before-{i:05}-{}\n", "x".repeat(49)))
                .collect();
            let after: String = if reversed {
                (0..20_000)
                    .rev()
                    .map(|i| format!("before-{i:05}-{}\n", "x".repeat(49)))
                    .collect()
            } else {
                (0..20_000)
                    .map(|i| format!("after--{i:05}-{}\n", "y".repeat(49)))
                    .collect()
            };
            let baseline = similar::TextDiff::from_lines(&before, &after);
            let expected_patch = format!("Index: large.ts\n===================================================================\n{}", baseline.unified_diff().header("large.ts", "large.ts"));
            let expected_count = if reversed { 19_999 } else { 20_000 };
            let start = std::time::Instant::now();
            let mut actual = serde_json::json!({});
            edit::attach_preview_diff(
                &mut actual,
                &serde_json::json!({}),
                "large.ts",
                &before,
                &after,
            );
            let elapsed = start.elapsed();
            assert_eq!(
                actual,
                serde_json::json!({"metadata": {"diff": expected_patch}, "preview": true, "preview_diff": expected_patch, "diff": {"additions": expected_count, "deletions": expected_count}})
            );
            eprintln!("A03 diff reversed={reversed} elapsed={elapsed:?}");
        }
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn batch_reuses_one_line_offset_scan() {
        let source = "\u{feff}first\r\n\u{e9}\r\nthree\rfour\r\ntail";
        edit::LINE_OFFSET_BYTES_SCANNED.with(|count| count.set(0));
        let offsets = edit::LineOffsets::new(source);
        let edit = serde_json::json!({"line_start": 2, "line_end": 2, "content": "new"});
        for _ in 0..100 {
            let resolved =
                resolve_edit_with_offsets(source, &offsets, &edit, 0, "test", None).unwrap();
            assert_eq!((resolved[0].byte_start, resolved[0].byte_end), (10, 14));
            let content =
                edit::replace_byte_range(source, 10, 14, &resolved[0].replacement).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
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
            assert_eq!(
                std::fs::read(path).unwrap(),
                "\u{feff}first\r\nnew\nthree\rfour\r\ntail".as_bytes()
            );
        }
        assert_eq!(
            edit::LINE_OFFSET_BYTES_SCANNED.with(|count| count.get()),
            source.len()
        );
    }

    /// A batch replace-all item shares one parse for string lines across
    /// all of its 1,000 loose matches.
    #[test]
    fn batch_replace_all_parses_for_string_lines_at_most_once() {
        use crate::fuzzy_match::STRING_LINE_PARSES;
        let source = format!("def f():\n{}", "    v = 'a'\n".repeat(1_000));
        let offsets = edit::LineOffsets::new(&source);
        let edit = serde_json::json!({
            "match": "v = 'a' ",
            "replacement": "v = 'b'\nw = 'c'",
            "replaceAll": true,
        });
        STRING_LINE_PARSES.with(|count| count.set(0));
        let resolved =
            resolve_edit_with_offsets(&source, &offsets, &edit, 0, "t", Some(Path::new("t.py")))
                .unwrap_or_else(|_| panic!("batch replace-all should resolve"));
        assert_eq!(resolved.len(), 1_000);
        assert!(resolved
            .iter()
            .all(|edit| edit.replacement == "v = 'b'\n    w = 'c'"));
        assert_eq!(STRING_LINE_PARSES.with(|count| count.get()), 1);
    }
}
