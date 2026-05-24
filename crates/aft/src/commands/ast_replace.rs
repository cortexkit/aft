//! Handler for the `ast_replace` command: AST-aware pattern replacement using ast-grep.
//!
//! Walks the project directory (or specified paths/globs) and replaces all nodes
//! matching the given pattern with the rewrite template.

use std::path::{Path, PathBuf};

use ast_grep_core::matcher::Pattern as AstPattern;
use ast_grep_core::tree_sitter::LanguageExt;
use rayon::prelude::*;

use crate::ast_grep_hints::detect_pattern_hint;
use crate::ast_grep_lang::AstGrepLang;
use crate::commands::ast_scope::collect_ast_files;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Per-file compute result from the parallel phase. Holds everything needed
/// for the serial apply phase (dry_run diff or backup+write).
struct FileChange {
    file_path: PathBuf,
    original: String,
    new_content: String,
    replacement_count: usize,
}

/// Result of an ast_replace dry-run diff computation.
struct DryRunResult {
    /// Unified diff between original and proposed content.
    diff: String,
    /// Whether the proposed content has valid syntax. `None` for unsupported languages.
    syntax_valid: Option<bool>,
}

/// Compute a unified diff between original and proposed content, plus syntax validation.
///
/// This stays private to ast_replace because it is now the only command whose
/// plugin API exposes dry-run mutation previews.
fn dry_run_diff(original: &str, proposed: &str, path: &Path) -> DryRunResult {
    let display_path = path.display().to_string();
    let text_diff = similar::TextDiff::from_lines(original, proposed);
    let diff = text_diff
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("a/{}", display_path),
            &format!("b/{}", display_path),
        )
        .to_string();
    let syntax_valid = crate::edit::validate_syntax_str(proposed, path);
    DryRunResult { diff, syntax_valid }
}

/// Handle an `ast_replace` request.
///
/// Params:
///   - `pattern` (string, required) — ast-grep pattern, e.g. `console.log($MSG)`
///   - `rewrite` (string, required) — replacement template, e.g. `logger.info($MSG)`
///   - `lang` (string, required) — language: typescript, tsx, javascript, python, rust, go, c, cpp, zig, csharp
///   - `paths` (array of strings, optional) — restrict to these paths
///   - `globs` (array of strings, optional) — include/exclude glob patterns
///   - `dry_run` (bool, optional, default true) — preview without writing
///
/// Returns (dry_run=true):
///   `{ ok: true, files: [{ file, diff, replacements }], total_replacements: N, total_files: N, files_with_matches: N, files_searched: N }`
///
/// Returns (dry_run=false):
///   `{ ok: true, files: [{ file, replacements, backup_id? }], total_replacements: N, total_files: N, files_with_matches: N, files_searched: N }`
pub fn handle_ast_replace(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let pattern = match req.params.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "ast_replace: missing required param 'pattern'",
            );
        }
    };

    let rewrite = match req.params.get("rewrite").and_then(|v| v.as_str()) {
        Some(r) => r.to_string(),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "ast_replace: missing required param 'rewrite'",
            );
        }
    };

    // ast-grep treats `$$$` in the REWRITE template as a named-only meta-variable
    // (`$$$BODY`, `$$$ARGS`, etc). Anonymous `$$$` returns `None` from
    // `split_first_meta_var` and is emitted LITERALLY into the output —
    // silently destroying captured content. Reject that shape up front
    // with actionable guidance instead of producing literal `$$$` strings
    // in the agent's source files.
    if has_anonymous_variadic(&rewrite) {
        return Response::error(
            &req.id,
            "invalid_rewrite",
            "ast_replace: anonymous `$$$` in rewrite is not supported by ast-grep \
             (it would be emitted as the literal string `$$$` instead of expanding \
             the captured nodes). Use a NAMED variadic in BOTH pattern and rewrite, \
             e.g. pattern: `test($NAME, () => { $$$BODY })`, rewrite: \
             `test($NAME, async () => { $$$BODY })`. Single-node `$VAR` and named \
             variadic `$$$VAR` work as expected.",
        );
    }

    let lang_str = match req.params.get("lang").and_then(|v| v.as_str()) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "ast_replace: missing required param 'lang'",
            );
        }
    };

    let lang = match AstGrepLang::from_str(lang_str) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "ast_replace: unsupported language '{}'. Supported: typescript, tsx, javascript, python, rust, go, c, cpp, zig, csharp, solidity, vue",
                    lang_str
                ),
            );
        }
    };

    let paths: Vec<String> = req
        .params
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let globs: Vec<String> = req
        .params
        .get("globs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let dry_run = req
        .params
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    // Pre-compile the pattern matcher ONCE so each parallel file worker reuses
    // the same parsed pattern instead of re-parsing it per file. This is the
    // big perf win — ast-grep's `find_all(&str)` / `replace_all(&str, ...)`
    // re-parse the pattern via tree-sitter on every call.
    let compiled_pattern = match AstPattern::try_new(&pattern, lang.clone()) {
        Ok(p) => p,
        Err(e) => {
            // Attach a hint when the pattern looks like a regex or a
            // language-specific shape mistake. See ast_grep_hints.
            let mut message = format!(
                "ast_replace: invalid pattern '{}': {}. Patterns must be complete AST nodes.",
                pattern, e
            );
            if let Some(hint) = detect_pattern_hint(&pattern, &lang) {
                message.push_str("\n\n");
                message.push_str(&hint);
            }
            return Response::error(&req.id, "invalid_pattern", message);
        }
    };

    let scope = match collect_ast_files(
        &req.id,
        "ast_replace",
        ctx,
        &project_root,
        &lang,
        &paths,
        &globs,
    ) {
        Ok(scope) => scope,
        Err(resp) => return resp,
    };

    // Phase 1 — parallel compute. Each worker reads, parses, computes edits,
    // and produces the new content. No mutation of shared state, no ctx access.
    let computed: Vec<FileChange> = scope
        .files
        .par_iter()
        .filter_map(|file_path| {
            let original = std::fs::read_to_string(file_path.as_path()).ok()?;

            let root = lang.ast_grep(&original);
            // Use replace_all to get ALL edits — root.replace() only replaces the FIRST match.
            // Pass the precompiled `&Pattern` rather than `&str` so we don't reparse per file.
            let mut edits = root.root().replace_all(&compiled_pattern, rewrite.as_str());
            if edits.is_empty() {
                return None;
            }

            let replacement_count = edits.len();
            // Apply edits in reverse byte-offset order to preserve positions.
            edits.sort_by(|a, b| b.position.cmp(&a.position));
            let mut new_bytes = original.as_bytes().to_vec();
            for edit in &edits {
                let start = edit.position;
                let end = start + edit.deleted_length;
                if start <= new_bytes.len() && end <= new_bytes.len() {
                    new_bytes.splice(start..end, edit.inserted_text.iter().copied());
                }
            }
            let new_content = String::from_utf8(new_bytes).unwrap_or_else(|_| original.clone());

            Some(FileChange {
                file_path: file_path.clone(),
                original,
                new_content,
                replacement_count,
            })
        })
        .collect();

    let files_searched = scope.files.len();
    let files_with_matches = computed.len();
    let mut total_replacements = 0usize;
    let mut total_files = 0usize;
    let mut file_results: Vec<serde_json::Value> = Vec::new();

    // Phase 2 — serial apply. Backup + write must touch shared state (BackupStore
    // is `RefCell`-wrapped on AppContext) so this stays on the main thread.
    let mut changes_to_apply: Vec<(FileChange, PathBuf, String)> = Vec::new();
    for change in computed {
        total_replacements += change.replacement_count;
        total_files += 1;

        if dry_run {
            let diff_result = dry_run_diff(
                &change.original,
                &change.new_content,
                change.file_path.as_path(),
            );
            file_results.push(serde_json::json!({
                "file": change.file_path.display().to_string(),
                "diff": diff_result.diff,
                "syntax_valid": diff_result.syntax_valid,
                "replacements": change.replacement_count,
            }));
        } else {
            let validated_path =
                match validate_matched_file_path(ctx, &req.id, change.file_path.as_path()) {
                    Ok(path) => path,
                    Err(resp) => return resp,
                };

            let backup_id = match ctx.backup().borrow_mut().snapshot_with_op(
                req.session(),
                validated_path.as_path(),
                "ast_replace",
                Some(&op_id),
            ) {
                Ok(id) => id,
                Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
            };

            changes_to_apply.push((change, validated_path, backup_id));
        }
    }

    if !dry_run {
        for (change, validated_path, _) in &changes_to_apply {
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(validated_path.as_path())
            {
                ctx.backup()
                    .borrow_mut()
                    .discard_operation_entries(req.session(), &op_id);
                return Response::error_with_data(
                    &req.id,
                    "io_error",
                    format!(
                        "ast_replace: failed to open '{}' for writing: {}; rolled_back: true",
                        change.file_path.display(),
                        e
                    ),
                    serde_json::json!({
                        "rolled_back": true,
                        "failed_file": change.file_path.display().to_string(),
                    }),
                );
            }
        }

        let mut written_changes: Vec<(PathBuf, String)> = Vec::new();
        for (change, validated_path, backup_id) in changes_to_apply {
            match std::fs::write(validated_path.as_path(), &change.new_content) {
                Ok(()) => {
                    written_changes.push((validated_path.clone(), change.original.clone()));
                    let mut entry = serde_json::json!({
                        "file": change.file_path.display().to_string(),
                        "replacements": change.replacement_count,
                    });
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert(
                            "backup_id".to_string(),
                            serde_json::Value::String(backup_id),
                        );
                    }
                    file_results.push(entry);
                }
                Err(e) => {
                    let rollback_error = rollback_written_changes(
                        &written_changes,
                        Some((validated_path.as_path(), change.original.as_str())),
                    );
                    let rollback_ok = rollback_error.is_none();
                    if rollback_ok {
                        ctx.backup()
                            .borrow_mut()
                            .discard_operation_entries(req.session(), &op_id);
                    }
                    return Response::error_with_data(
                        &req.id,
                        "io_error",
                        format!(
                            "ast_replace: failed to write '{}': {}; rolled_back: {}",
                            change.file_path.display(),
                            e,
                            rollback_ok
                        ),
                        serde_json::json!({
                            "rolled_back": rollback_ok,
                            "rollback_error": rollback_error,
                            "failed_file": change.file_path.display().to_string(),
                        }),
                    );
                }
            }
        }
    }

    let mut payload = serde_json::json!({
        "files": file_results,
        "total_replacements": total_replacements,
        "total_files": total_files,
        "files_with_matches": files_with_matches,
        "files_searched": files_searched,
        "no_files_matched_scope": scope.no_files_matched_scope,
        "scope_warnings": scope.scope_warnings,
        "dry_run": dry_run,
    });

    // Same hint surface as ast_search: if zero replacements happened across a
    // valid scope, attach a hint when the pattern looks like a common mistake.
    // Especially important for replace, where "0 replacements" looks like a
    // clean no-op but may actually be silent corruption (today's `|` bug).
    if total_replacements == 0 && !scope.no_files_matched_scope {
        if let Some(hint) = detect_pattern_hint(&pattern, &lang) {
            payload["hint"] = serde_json::Value::String(hint);
        }
    }

    Response::success(&req.id, payload)
}

fn rollback_written_changes(
    written_changes: &[(PathBuf, String)],
    attempted: Option<(&Path, &str)>,
) -> Option<String> {
    if let Some((path, original)) = attempted {
        if let Err(e) = std::fs::write(path, original) {
            return Some(format!("{}: {}", path.display(), e));
        }
    }
    for (path, original) in written_changes.iter().rev() {
        if let Err(e) = std::fs::write(path, original) {
            return Some(format!("{}: {}", path.display(), e));
        }
    }
    None
}

fn validate_matched_file_path(
    ctx: &AppContext,
    req_id: &str,
    file_path: &Path,
) -> Result<PathBuf, Response> {
    ctx.validate_path(req_id, file_path)
}

/// Detect anonymous `$$$` (three meta-chars NOT followed by a name char) in a
/// rewrite template.
///
/// ast-grep's `split_first_meta_var` parses a meta-var as `$$$NAME` (with NAME
/// matching `is_valid_meta_var_char`). When `$$$` is followed by something that
/// isn't a valid name char (whitespace, punctuation, EOF), the parser returns
/// `None` and ast-grep emits the literal `$$$` string in the output.
///
/// This helper mirrors that scan: walk the template, find runs of three or
/// more `$`, peek the char after the third `$`, and call it anonymous when
/// that char isn't a valid meta-var name character.
///
/// Examples:
///   has_anonymous_variadic("logger.info($MSG)")               → false (single)
///   has_anonymous_variadic("test($N, async () => { $$$ })")   → true  (anonymous)
///   has_anonymous_variadic("test($N, async () => { $$$BODY })") → false (named)
///   has_anonymous_variadic("$$$$")                            → true  (4 $ then nothing)
///   has_anonymous_variadic("price = $$$.99")                  → true  (`.` not name char)
fn has_anonymous_variadic(rewrite: &str) -> bool {
    // Inline ast-grep's `is_valid_meta_var_char` rule (private to that crate
    // as of v0.41.1): a meta-var name char is uppercase A-Z, underscore, or
    // an ASCII digit. Lowercase letters and non-ASCII chars are NOT valid
    // name chars — `$$$body`, `$$$π`, etc. are also anonymous as far as
    // ast-grep is concerned.
    fn is_valid_meta_var_char(c: char) -> bool {
        matches!(c, 'A'..='Z' | '_' | '0'..='9')
    }

    let bytes = rewrite.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'$' && bytes[i + 2] == b'$' {
            // Walk past the run of `$` characters so we land on the first
            // non-`$` byte. Patterns like `$$$$` are still anonymous because
            // there is no NAME after the meta-char run.
            let mut j = i + 3;
            while j < bytes.len() && bytes[j] == b'$' {
                j += 1;
            }
            // Peek the first char after the meta-char run.
            let after = rewrite[j..].chars().next();
            let is_named = match after {
                Some(c) => is_valid_meta_var_char(c),
                None => false, // run of `$` at EOF — definitely anonymous
            };
            if !is_named {
                return true;
            }
            // Skip past the matched named variadic to keep scanning.
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::has_anonymous_variadic;

    #[test]
    fn detects_anonymous_variadic_in_block() {
        assert!(has_anonymous_variadic("test($N, async () => { $$$ })"));
    }

    #[test]
    fn detects_anonymous_variadic_at_end() {
        assert!(has_anonymous_variadic("trailing $$$"));
        assert!(has_anonymous_variadic("trailing $$$ "));
    }

    #[test]
    fn detects_anonymous_when_followed_by_punctuation() {
        // ast-grep stops the name at any non-identifier char, so the `.`
        // makes this anonymous and ast-grep emits literal `$$$` here.
        assert!(has_anonymous_variadic("price = $$$.99"));
    }

    #[test]
    fn detects_anonymous_when_run_extends_past_three_dollars() {
        // Four `$` then no name → still anonymous.
        assert!(has_anonymous_variadic("emit $$$$ here"));
    }

    #[test]
    fn allows_named_variadic() {
        assert!(!has_anonymous_variadic("test($N, async () => { $$$BODY })"));
        assert!(!has_anonymous_variadic("$$$_args"));
        assert!(!has_anonymous_variadic("import { $$$IMPORTS } from 'x'"));
    }

    #[test]
    fn allows_single_dollar_meta_var() {
        // `$VAR` is not a variadic at all and must not be flagged.
        assert!(!has_anonymous_variadic("logger.info($MSG)"));
        assert!(!has_anonymous_variadic("$NAME = $VALUE"));
    }

    #[test]
    fn allows_double_dollar_literal() {
        // `$$` is not a meta-var pattern that triggers this scan.
        assert!(!has_anonymous_variadic("price = $$.99"));
    }

    #[test]
    fn allows_empty_and_no_dollars() {
        assert!(!has_anonymous_variadic(""));
        assert!(!has_anonymous_variadic("plain text"));
    }
}
