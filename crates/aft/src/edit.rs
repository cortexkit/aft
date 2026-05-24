//! Shared edit engine: byte-offset conversion, content replacement,
//! syntax validation, and auto-backup orchestration.
//!
//! Used by `write`, `edit_symbol`, `edit_match`, and `batch` commands.

#![cfg_attr(test, allow(clippy::items_after_test_module))]

use std::path::Path;

use crate::config::Config;
use crate::context::AppContext;
use crate::error::AftError;
use crate::format;
use crate::parser::{detect_language, grammar_for, FileParser};

/// Convert 0-indexed line/col to a byte offset within `source`.
///
/// Tree-sitter columns are byte-indexed within the line, so `col` is a byte
/// offset from the start of the line (not a character offset).
///
/// Scans raw bytes so both LF and CRLF line endings are counted correctly.
/// Returns `source.len()` if line is beyond the end of the file.
pub fn line_col_to_byte(source: &str, line: u32, col: u32) -> usize {
    let bytes = source.as_bytes();
    let target_line = line as usize;
    let mut current_line = 0usize;
    let mut line_start = 0usize;

    loop {
        let mut line_end = line_start;
        while line_end < bytes.len() && bytes[line_end] != b'\n' && bytes[line_end] != b'\r' {
            line_end += 1;
        }

        if current_line == target_line {
            return line_start + (col as usize).min(line_end.saturating_sub(line_start));
        }

        if line_end >= bytes.len() {
            return source.len();
        }

        line_start = if bytes[line_end] == b'\r'
            && line_end + 1 < bytes.len()
            && bytes[line_end + 1] == b'\n'
        {
            line_end + 2
        } else {
            line_end + 1
        };
        current_line += 1;
    }
}

/// Replace bytes in `[start..end)` with `replacement`.
///
/// Returns an error if the range is invalid or does not align to UTF-8 char boundaries.
pub fn replace_byte_range(
    source: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> Result<String, AftError> {
    if start > end {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): start must be <= end",
                start, end
            ),
        });
    }
    if end > source.len() {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): end exceeds source length {}",
                start,
                end,
                source.len()
            ),
        });
    }
    if !source.is_char_boundary(start) {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): start is not a char boundary",
                start, end
            ),
        });
    }
    if !source.is_char_boundary(end) {
        return Err(AftError::InvalidRequest {
            message: format!(
                "invalid byte range [{}..{}): end is not a char boundary",
                start, end
            ),
        });
    }

    let mut result = String::with_capacity(
        source.len().saturating_sub(end.saturating_sub(start)) + replacement.len(),
    );
    result.push_str(&source[..start]);
    result.push_str(replacement);
    result.push_str(&source[end..]);
    Ok(result)
}

/// Validate syntax of a file using a fresh FileParser (D023).
///
/// Returns `Ok(Some(true))` if syntax is valid, `Ok(Some(false))` if there are
/// parse errors, and `Ok(None)` if the language is unsupported.
pub fn validate_syntax(path: &Path) -> Result<Option<bool>, AftError> {
    let mut parser = FileParser::new();
    match parser.parse(path) {
        Ok((tree, _lang)) => Ok(Some(!tree.root_node().has_error())),
        Err(AftError::InvalidRequest { .. }) => {
            // Unsupported language — not an error, just can't validate
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Validate syntax of an in-memory string without touching disk.
///
/// Uses `detect_language(path)` + `grammar_for(lang)` + `parser.parse()`
/// to validate syntax of a proposed content string. Returns `None` for
/// unsupported languages, `Some(true)` for valid, `Some(false)` for invalid.
pub fn validate_syntax_str(content: &str, path: &Path) -> Option<bool> {
    let lang = detect_language(path)?;
    let grammar = grammar_for(lang);
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return None;
    }
    let tree = parser.parse(content.as_bytes(), None)?;
    Some(!tree.root_node().has_error())
}

/// Check if the caller requested diff info in the response.
pub fn wants_diff(params: &serde_json::Value) -> bool {
    params
        .get("include_diff")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Compute diff info between before/after content for UI metadata.
/// Returns a JSON value with before, after, additions, deletions.
/// For files >512KB, omits full content and returns only counts.
pub fn compute_diff_info(before: &str, after: &str) -> serde_json::Value {
    use similar::ChangeTag;

    let diff = similar::TextDiff::from_lines(before, after);
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }

    // For large files, skip sending full content to avoid bloating JSON
    let size_limit = 512 * 1024; // 512KB
    if before.len() > size_limit || after.len() > size_limit {
        serde_json::json!({
            "additions": additions,
            "deletions": deletions,
            "truncated": true,
        })
    } else {
        serde_json::json!({
            "before": before,
            "after": after,
            "additions": additions,
            "deletions": deletions,
        })
    }
}
/// Snapshot the file into the backup store before mutation, scoped to a session.
///
/// Returns `Ok(Some(backup_id))` if the file existed and was backed up,
/// `Ok(None)` if the file doesn't exist (new file creation).
///
/// The `session` argument is the request-level session namespace (see
/// [`crate::protocol::RawRequest::session`]). Snapshots created by one session
/// are not visible from another, which is what keeps undo state isolated in
/// a shared-bridge setup (issue #14).
///
/// Drops the RefCell borrow before returning (D029).
pub fn auto_backup(
    ctx: &AppContext,
    session: &str,
    path: &Path,
    description: &str,
    op_id: Option<&str>,
) -> Result<Option<String>, AftError> {
    if !path.exists() {
        return Ok(None);
    }
    let backup_id = {
        let mut store = ctx.backup().borrow_mut();
        store.snapshot_with_op(session, path, description, op_id)?
    }; // borrow dropped here
    Ok(Some(backup_id))
}

/// Result of the write → format → validate pipeline.
///
/// Returned by `write_format_validate` to give callers a single struct
/// with all post-write signals for the response JSON.
pub struct WriteResult {
    /// Whether tree-sitter syntax validation passed. `None` if unsupported language.
    pub syntax_valid: Option<bool>,
    /// Whether the file was auto-formatted.
    pub formatted: bool,
    /// Why formatting was skipped, if it was. Values: "unsupported_language",
    /// "no_formatter_configured", "formatter_not_installed", "formatter_excluded_path",
    /// "timeout", "error".
    pub format_skipped_reason: Option<String>,
    /// Whether full validation was requested (controls whether validation_errors is included in response).
    pub validate_requested: bool,
    /// Structured type-checker errors (only populated when validate:"full" is requested).
    pub validation_errors: Vec<format::ValidationError>,
    /// Why validation was skipped, if it was. Values: "unsupported_language",
    /// "no_checker_configured", "checker_not_installed", "timeout", "error".
    pub validate_skipped_reason: Option<String>,
    /// True when the write+format+validate pipeline detected post-write
    /// invalid syntax against a previously-valid file and restored the
    /// pre-write content. The on-disk file is the original; `syntax_valid`
    /// reports the would-have-been-written status (Some(false)).
    pub rolled_back: bool,
    /// Per-edit LSP diagnostics outcome (v0.17.3). Carries the verified-fresh
    /// diagnostics PLUS per-server status (pending/exited) so the response
    /// can report `complete: bool` honestly.
    ///
    /// `None` means the caller didn't request diagnostics OR the request
    /// was a fire-and-forget notify (no wait). `Some(outcome)` always
    /// reports diagnostics from servers that proved freshness against the
    /// post-edit document version.
    pub lsp_outcome: Option<crate::lsp::manager::PostEditWaitOutcome>,
}

impl WriteResult {
    /// Append LSP diagnostics + per-server status to a response JSON
    /// object.
    ///
    /// v0.17.3 honest-reporting contract: when diagnostics were requested
    /// (`lsp_outcome.is_some()`), this ALWAYS emits `lsp_diagnostics: [...]`
    /// (even if empty) plus `lsp_complete: bool`, `lsp_pending_servers`,
    /// and `lsp_exited_servers`. Empty `lsp_diagnostics` no longer means
    /// "the field disappeared" — it means "we waited and got an explicit
    /// fresh-but-clean result, OR every expected server is in the pending/
    /// exited list (check `lsp_complete`)."
    ///
    /// When diagnostics were NOT requested (`lsp_outcome.is_none()`),
    /// nothing is added — keeps the no-LSP edit path's response shape
    /// unchanged.
    pub fn append_lsp_diagnostics_to(&self, result: &mut serde_json::Value) {
        result["rolled_back"] = serde_json::json!(self.rolled_back);

        let Some(outcome) = self.lsp_outcome.as_ref() else {
            return;
        };

        result["lsp_diagnostics"] = serde_json::json!(outcome
            .diagnostics
            .iter()
            .map(|d| {
                serde_json::json!({
                    "file": d.file.display().to_string(),
                    "line": d.line,
                    "column": d.column,
                    "end_line": d.end_line,
                    "end_column": d.end_column,
                    "severity": d.severity.as_str(),
                    "message": d.message,
                    "code": d.code,
                    "source": d.source,
                })
            })
            .collect::<Vec<_>>());

        result["lsp_complete"] = serde_json::Value::Bool(outcome.complete());

        if !outcome.pending_servers.is_empty() {
            result["lsp_pending_servers"] = serde_json::json!(outcome
                .pending_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
        if !outcome.exited_servers.is_empty() {
            result["lsp_exited_servers"] = serde_json::json!(outcome
                .exited_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
    }
}

/// Write content to disk, auto-format, then validate syntax.
///
/// This is the shared tail for all mutation commands. The pipeline order is:
/// 1. `fs::write` — persist content
/// 2. `auto_format` — run the project formatter (reads the written file, writes back)
/// 3. `validate_syntax` — parse the (potentially formatted) file
/// 4. `validate_full` — run type checker if requested by params or config
///
/// The `params` argument carries the original request parameters. When it
/// contains `"validate": "full"`, or config sets `validate_on_edit: "full"`,
/// the project's type checker is invoked after syntax validation and the
/// results are included in `WriteResult`.
pub fn write_format_validate(
    path: &Path,
    content: &str,
    config: &Config,
    params: &serde_json::Value,
) -> Result<WriteResult, AftError> {
    let pre_write_content = if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    };
    // Existing clean files are protected from invalid mutations. New files have
    // no safe prior content to restore, so their pre-write validity remains None
    // and invalid syntax is reported without rollback.
    let was_syntax_valid = if pre_write_content.is_some() {
        match validate_syntax(path) {
            Ok(valid) => valid,
            Err(_) => None,
        }
    } else {
        None
    };

    // Step 1: Write
    std::fs::write(path, content).map_err(|e| AftError::InvalidRequest {
        message: format!("failed to write file: {}", e),
    })?;

    // Step 2: Format (before validate so we validate the formatted content)
    let (formatted, format_skipped_reason) = format::auto_format(path, config);

    // Step 3: Validate syntax
    let syntax_valid = match validate_syntax(path) {
        Ok(sv) => sv,
        Err(_) => None,
    };
    let rolled_back = if was_syntax_valid == Some(true) && syntax_valid == Some(false) {
        if let Some(original) = pre_write_content.as_ref() {
            std::fs::write(path, original).map_err(|e| AftError::InvalidRequest {
                message: format!("failed to roll back invalid edit: {}", e),
            })?;
            true
        } else {
            false
        }
    } else {
        false
    };

    // Step 4: Full validation (type checker) — only when requested
    let param_validate = params.get("validate").and_then(|v| v.as_str());
    let config_validate = config.validate_on_edit.as_deref();
    // Explicit param overrides config. Valid values: "syntax" | "full" | "off".
    let validate_mode = param_validate.or(config_validate).unwrap_or("off");
    let validate_requested = validate_mode == "full";
    let (validation_errors, validate_skipped_reason) = if validate_requested {
        format::validate_full(path, config)
    } else {
        (Vec::new(), None)
    };

    Ok(WriteResult {
        syntax_valid,
        formatted,
        format_skipped_reason,
        validate_requested,
        validation_errors,
        validate_skipped_reason,
        rolled_back,
        lsp_outcome: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- line_col_to_byte ---

    #[test]
    fn line_col_to_byte_empty_string() {
        assert_eq!(line_col_to_byte("", 0, 0), 0);
    }

    #[test]
    fn line_col_to_byte_single_line() {
        let source = "hello";
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 3), 3);
        assert_eq!(line_col_to_byte(source, 0, 5), 5); // end of line
    }

    #[test]
    fn line_col_to_byte_multi_line() {
        let source = "abc\ndef\nghi\n";
        // line 0: "abc" at bytes 0..3, newline at 3
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 2), 2);
        // line 1: "def" at bytes 4..7, newline at 7
        assert_eq!(line_col_to_byte(source, 1, 0), 4);
        assert_eq!(line_col_to_byte(source, 1, 3), 7);
        // line 2: "ghi" at bytes 8..11, newline at 11
        assert_eq!(line_col_to_byte(source, 2, 0), 8);
        assert_eq!(line_col_to_byte(source, 2, 2), 10);
    }

    #[test]
    fn line_col_to_byte_last_line_no_trailing_newline() {
        let source = "abc\ndef";
        // line 1: "def" at bytes 4..7, no trailing newline
        assert_eq!(line_col_to_byte(source, 1, 0), 4);
        assert_eq!(line_col_to_byte(source, 1, 3), 7); // end
    }

    #[test]
    fn line_col_to_byte_multi_byte_utf8() {
        // "é" is 2 bytes in UTF-8
        let source = "café\nbar";
        // line 0: "café" is 5 bytes (c=1, a=1, f=1, é=2)
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 5), 5); // end of "café"
                                                       // line 1: "bar" starts at byte 6
        assert_eq!(line_col_to_byte(source, 1, 0), 6);
        assert_eq!(line_col_to_byte(source, 1, 2), 8);
    }

    #[test]
    fn line_col_to_byte_beyond_end() {
        let source = "abc";
        // Line beyond file returns source.len()
        assert_eq!(line_col_to_byte(source, 5, 0), source.len());
    }

    #[test]
    fn line_col_to_byte_col_clamped_to_line_length() {
        let source = "ab\ncd";
        // col=10 on a 2-char line should clamp to 2
        assert_eq!(line_col_to_byte(source, 0, 10), 2);
    }

    #[test]
    fn line_col_to_byte_crlf() {
        let source = "abc\r\ndef\r\nghi\r\n";
        assert_eq!(line_col_to_byte(source, 0, 0), 0);
        assert_eq!(line_col_to_byte(source, 0, 10), 3);
        assert_eq!(line_col_to_byte(source, 1, 0), 5);
        assert_eq!(line_col_to_byte(source, 1, 3), 8);
        assert_eq!(line_col_to_byte(source, 2, 0), 10);
    }

    // --- replace_byte_range ---

    #[test]
    fn replace_byte_range_basic() {
        let source = "hello world";
        let result = replace_byte_range(source, 6, 11, "rust").unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn replace_byte_range_delete() {
        let source = "hello world";
        let result = replace_byte_range(source, 5, 11, "").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn replace_byte_range_insert_at_same_position() {
        let source = "helloworld";
        let result = replace_byte_range(source, 5, 5, " ").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn replace_byte_range_replace_entire_string() {
        let source = "old content";
        let result = replace_byte_range(source, 0, source.len(), "new content").unwrap();
        assert_eq!(result, "new content");
    }
}
