//! Handler for the `ast_replace` command: AST-aware pattern replacement using ast-grep.
//!
//! Walks the project directory (or specified paths/globs) and replaces all nodes
//! matching the given pattern with the rewrite template.

use std::path::{Path, PathBuf};

use ast_grep_core::tree_sitter::LanguageExt;
use rayon::prelude::*;

use crate::ast_grep_hints::detect_pattern_hint;
use crate::ast_grep_lang::AstGrepLang;
use crate::commands::ast_scope::collect_ast_files;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Reply to a dry run whose rewrite would leave at least one file with invalid
/// syntax; `{files}` is replaced by the comma-separated file paths.
const DRY_RUN_INVALID_REWRITE_MESSAGE: &str = "ast_replace: dry run: rewritten code failed syntax validation in {files}; apply would refuse this rewrite (no files were written)";

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
///   - `lang` (string, required) — selects the source language that ast-grep should parse; accepted values are typescript, tsx, javascript, python, rust, go, c, cpp, cuda, metal, zig, csharp, solidity, vue, pascal, r, groovy, objc
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

    // A named meta-var in the REWRITE that the PATTERN never captured (e.g. a
    // typo: pattern `console.log($MSG)`, rewrite `logger.info($MGS)`) is NOT an
    // error to ast-grep — it emits the literal text `$MGS` into the output,
    // silently corrupting the file with success:true. Reject up front: every
    // rewrite meta-var must be bound by the pattern.
    let pattern_vars = extract_meta_var_names(&pattern);
    let rewrite_vars = extract_meta_var_names(&rewrite);
    let unbound: Vec<String> = rewrite_vars
        .difference(&pattern_vars)
        .cloned()
        .collect::<Vec<_>>();
    if !unbound.is_empty() {
        let mut unbound = unbound;
        unbound.sort();
        return Response::error(
            &req.id,
            "invalid_rewrite",
            format!(
                "ast_replace: the rewrite references meta-variable(s) the pattern never \
                 captures: {}. ast-grep would emit them as literal text (e.g. `$MGS`) instead \
                 of expanding a capture, corrupting the output. Check for a typo — every `$VAR` \
                 in the rewrite must also appear in the pattern. Pattern captures: {}.",
                unbound
                    .iter()
                    .map(|v| format!("${v}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if pattern_vars.is_empty() {
                    "(none)".to_string()
                } else {
                    let mut pv: Vec<String> =
                        pattern_vars.iter().map(|v| format!("${v}")).collect();
                    pv.sort();
                    pv.join(", ")
                }
            ),
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
                    "ast_replace: unsupported language '{}'. Supported: typescript, tsx, javascript, python, rust, go, c, cpp, cuda, metal, zig, csharp, solidity, vue, pascal, r, groovy, objc",
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
    let compiled_pattern = match lang.compile_pattern(&pattern) {
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
        None,
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
    let outcomes: Vec<Result<Option<FileChange>, (PathBuf, String)>> = scope
        .files
        .par_iter()
        .map(|file_path| {
            let original = std::fs::read_to_string(file_path.as_path())
                .map_err(|err| (file_path.clone(), err.to_string()))?;

            let root = lang.ast_grep(&original);
            // Use replace_all to get ALL edits — root.replace() only replaces the FIRST match.
            // Pass the precompiled `&Pattern` rather than `&str` so we don't reparse per file.
            let mut edits = root.root().replace_all(&compiled_pattern, rewrite.as_str());
            if edits.is_empty() {
                return Ok(None);
            }

            let replacement_count = edits.len();
            // Apply edits in reverse byte-offset order to preserve positions.
            edits.sort_by(|a, b| b.position.cmp(&a.position));
            let spans: Vec<(usize, usize, &[u8])> = edits
                .iter()
                .map(|edit| {
                    (
                        edit.position,
                        edit.deleted_length,
                        edit.inserted_text.as_slice(),
                    )
                })
                .collect();
            let new_bytes = apply_descending_edits(original.as_bytes(), &spans);
            let new_content = String::from_utf8(new_bytes).unwrap_or_else(|_| original.clone());

            Ok(Some(FileChange {
                file_path: file_path.clone(),
                original,
                new_content,
                replacement_count,
            }))
        })
        .collect();

    let mut computed = Vec::new();
    let mut read_errors = Vec::new();
    let mut files_searched = 0usize;
    for outcome in outcomes {
        match outcome {
            Ok(change) => {
                files_searched += 1;
                if let Some(change) = change {
                    computed.push(change);
                }
            }
            Err(error) => read_errors.push(error),
        }
    }

    if !read_errors.is_empty() {
        read_errors.sort_by(|a, b| a.0.cmp(&b.0));
        let failed_files: Vec<serde_json::Value> = read_errors
            .iter()
            .map(|(path, err)| {
                serde_json::json!({
                    "file": path.display().to_string(),
                    "reason": format!("read_error: {err}"),
                })
            })
            .collect();
        return Response::error_with_data(
            &req.id,
            "io_error",
            format!(
                "ast_replace: failed to read {} file(s); no files were written",
                failed_files.len()
            ),
            serde_json::json!({
                "failed_files": failed_files,
                "files_searched": files_searched,
                "rolled_back": true,
            }),
        );
    }

    let files_with_matches = computed.len();
    let mut total_replacements = 0usize;
    let mut total_files = 0usize;
    let mut file_results: Vec<serde_json::Value> = Vec::new();
    let mut invalid_rewrites: Vec<String> = Vec::new();

    // Phase 2 — serial apply. Backup + write must touch shared state (BackupStore
    // is `RefCell`-wrapped on AppContext) so this stays on the main thread.
    let mut changes_to_apply: Vec<(FileChange, PathBuf, Option<String>)> = Vec::new();
    for change in computed {
        total_replacements += change.replacement_count;
        total_files += 1;

        if dry_run {
            let diff_result = dry_run_diff(
                &change.original,
                &change.new_content,
                change.file_path.as_path(),
            );
            // Apply refuses a rewrite that fails syntax validation, so the
            // preview must refuse it too rather than promise the change.
            if diff_result.syntax_valid == Some(false) {
                invalid_rewrites.push(change.file_path.display().to_string());
                continue;
            }
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

            if crate::edit::validate_syntax_str(&change.new_content, validated_path.as_path())
                == Some(false)
            {
                invalid_rewrites.push(change.file_path.display().to_string());
                continue;
            }

            let backup_id = match ctx.backup().lock().snapshot_with_op(
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

    if !invalid_rewrites.is_empty() {
        invalid_rewrites.sort();
        if dry_run {
            return Response::error_with_data(
                &req.id,
                "invalid_rewrite",
                DRY_RUN_INVALID_REWRITE_MESSAGE.replace("{files}", &invalid_rewrites.join(", ")),
                serde_json::json!({
                    "invalid_files": invalid_rewrites,
                    "dry_run": true,
                }),
            );
        }
        ctx.backup()
            .lock()
            .discard_operation_entries(req.session(), &op_id);
        return Response::error_with_data(
            &req.id,
            "invalid_rewrite",
            "ast_replace: rewritten code failed syntax validation; no files were written",
            serde_json::json!({
                "invalid_files": invalid_rewrites,
                "rolled_back": true,
            }),
        );
    }

    if !dry_run {
        let _mutation = crate::runtime_drain::record_file_mutation(
            ctx,
            changes_to_apply.iter().map(|(_, path, _)| path.as_path()),
        );
        for (change, validated_path, _) in &changes_to_apply {
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(validated_path.as_path())
            {
                ctx.backup()
                    .lock()
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
                    ctx.lsp_notify_file_changed(validated_path.as_path(), &change.new_content);
                    written_changes.push((validated_path.clone(), change.original.clone()));
                    let mut entry = serde_json::json!({
                        "file": change.file_path.display().to_string(),
                        "replacements": change.replacement_count,
                    });
                    if let (Some(obj), Some(backup_id)) = (entry.as_object_mut(), backup_id) {
                        obj.insert(
                            "backup_id".to_string(),
                            serde_json::Value::String(backup_id),
                        );
                    }
                    crate::edit::attach_backup_skipped_reason(
                        &mut entry,
                        ctx,
                        req.session(),
                        &op_id,
                        Some(validated_path.as_path()),
                    );
                    file_results.push(entry);
                }
                Err(e) => {
                    let rollback_error = rollback_written_changes(
                        &written_changes,
                        Some((validated_path.as_path(), change.original.as_str())),
                        ctx,
                    );
                    let rollback_ok = rollback_error.is_none();
                    if rollback_ok {
                        ctx.backup()
                            .lock()
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
    crate::edit::attach_backup_skipped_reason(&mut payload, ctx, req.session(), &op_id, None);

    Response::success(&req.id, payload)
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    #[test]
    fn ast_rollback_attempts_every_file_after_restore_error() {
        let dir = tempfile::tempdir().unwrap();
        let restored = dir.path().join("restored.txt");
        std::fs::write(&restored, "after").unwrap();
        let failed = dir.path().join("missing/failed.txt");
        let attempted = dir.path().join("missing/attempted.txt");
        let changes = vec![
            (restored.clone(), "before".to_string()),
            (failed.clone(), "before".to_string()),
        ];
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let error = rollback_written_changes(&changes, Some((&attempted, "before")), &ctx).unwrap();
        assert_eq!(std::fs::read_to_string(restored).unwrap(), "before");
        assert!(error.contains(failed.to_str().unwrap()));
        assert!(error.contains(attempted.to_str().unwrap()));
    }
}

#[cfg(test)]
#[path = "../../tests/helpers/fake_lsp.rs"]
mod fake_lsp;

#[cfg(test)]
mod lsp_tests {
    use super::*;
    use crate::lsp::client::LspEvent;
    use crate::lsp::position::uri_for_path;
    use crate::lsp::registry::ServerKind;

    fn changed_contents(ctx: &AppContext, stage: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            for event in ctx.lsp().drain_events() {
                if let LspEvent::Notification {
                    method,
                    params: Some(params),
                    ..
                } = event
                {
                    if method == "custom/documentChangedContents" {
                        return params;
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing documentChangedContents notification at {stage}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn open_document() -> (tempfile::TempDir, PathBuf, AppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"notifications\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let file = dir.path().join("src/main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let file = std::fs::canonicalize(file).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(dir.path().to_path_buf()),
                format_on_edit: true,
                ..Default::default()
            },
        );
        {
            let mut manager = ctx.lsp();
            manager.override_binary(ServerKind::Rust, fake_lsp::fake_server_binary());
            manager.set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
            manager.set_extra_env("AFT_FAKE_LSP_ECHO_CHANGES", "1");
            assert_eq!(
                crate::lsp::manager::settled_lsp(&mut manager, |startup_manager| startup_manager
                    .ensure_file_open(&file, &ctx.config()))
                .unwrap()
                .server_keys
                .len(),
                1
            );
        }
        (dir, file, ctx)
    }

    #[test]
    fn ast_replace_notifies_final_unformatted_contents() {
        let (_dir, file, ctx) = open_document();
        let expected = "fn main(){let changed=1;}\n";
        let req = serde_json::from_value(serde_json::json!({"id": "ast-notify", "command": "ast_replace", "pattern": "fn main() {}", "rewrite": "fn main(){let changed=1;}", "lang": "rust", "paths": [file], "dry_run": false})).unwrap();
        let response = handle_ast_replace(&req, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);
        let changed = changed_contents(&ctx, "post-write");
        assert_eq!(
            changed["textDocument"]["uri"],
            uri_for_path(&file).unwrap().to_string()
        );
        assert_eq!(changed["textDocument"]["version"], 1);
        assert_eq!(changed["contentChanges"][0]["text"], expected);
    }

    #[test]
    fn ast_rollback_notifies_restored_contents() {
        let (_dir, file, ctx) = open_document();
        let after = "fn main(){let changed=1;}\n";
        std::fs::write(&file, after).unwrap();
        ctx.lsp_notify_file_changed(&file, after);
        assert_eq!(
            changed_contents(&ctx, "arrange")["contentChanges"][0]["text"],
            after
        );
        assert!(
            rollback_written_changes(&[(file.clone(), "fn main() {}\n".into())], None, &ctx)
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}\n");
        let restored = changed_contents(&ctx, "post-restore");
        assert_eq!(
            restored["textDocument"]["uri"],
            uri_for_path(&file).unwrap().to_string()
        );
        assert_eq!(restored["textDocument"]["version"], 2);
        assert_eq!(restored["contentChanges"][0]["text"], "fn main() {}\n");
    }
}

fn rollback_written_changes(
    written_changes: &[(PathBuf, String)],
    attempted: Option<(&Path, &str)>,
    ctx: &AppContext,
) -> Option<String> {
    let mut failures = Vec::new();
    if let Some((path, original)) = attempted {
        if let Err(e) = std::fs::write(path, original) {
            failures.push(format!("{}: {}", path.display(), e));
        } else {
            ctx.lsp_notify_file_changed(path, original);
        }
    }
    for (path, original) in written_changes.iter().rev() {
        if let Err(e) = std::fs::write(path, original) {
            failures.push(format!("{}: {}", path.display(), e));
        } else {
            ctx.lsp_notify_file_changed(path, original);
        }
    }
    (!failures.is_empty()).then(|| failures.join("; "))
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

/// Extract the set of meta-variable NAMES referenced in an ast-grep pattern or
/// rewrite template (e.g. `console.log($MSG, $$$ARGS)` -> {"MSG", "ARGS"}).
///
/// Matches ast-grep's binding rule: a meta-var is `$` (or a `$$$` variadic run)
/// followed by a name that STARTS with an uppercase letter or `_` and continues
/// with uppercase/`_`/digits. Lowercase-led (`$msg`) and digit-led (`$1`) runs
/// are NOT meta-vars to ast-grep, so they are ignored here (conservative — we
/// never want to false-reject a legitimate rewrite).
fn extract_meta_var_names(s: &str) -> std::collections::HashSet<String> {
    fn is_name_char(c: char) -> bool {
        matches!(c, 'A'..='Z' | '_' | '0'..='9')
    }
    let mut names = std::collections::HashSet::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            // Skip a run of `$` (covers `$NAME` and `$$$NAME`).
            let mut j = i + 1;
            while j < chars.len() && chars[j] == '$' {
                j += 1;
            }
            // The name must START with an uppercase letter or underscore.
            if j < chars.len() && matches!(chars[j], 'A'..='Z' | '_') {
                let start = j;
                while j < chars.len() && is_name_char(chars[j]) {
                    j += 1;
                }
                names.insert(chars[start..j].iter().collect());
                i = j;
                continue;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{extract_meta_var_names, has_anonymous_variadic};

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

    #[test]
    fn extracts_meta_var_names() {
        let names = extract_meta_var_names("console.log($MSG, $$$ARGS)");
        assert!(names.contains("MSG"));
        assert!(names.contains("ARGS"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn extract_ignores_non_metavar_dollars() {
        // lowercase-led, digit-led, and bare `$` are not ast-grep meta-vars.
        let names = extract_meta_var_names("price = $$.99 + $msg + $1 + $");
        assert!(names.is_empty(), "got: {names:?}");
    }

    #[test]
    fn extract_handles_underscore_led_names() {
        let names = extract_meta_var_names("$_PRIVATE = $VALUE");
        assert!(names.contains("_PRIVATE"));
        assert!(names.contains("VALUE"));
    }

    // The subset relationship the handler enforces: a rewrite var absent from
    // the pattern is the typo/corruption case.
    #[test]
    fn rewrite_var_not_in_pattern_is_detectable() {
        let pattern = extract_meta_var_names("console.log($MSG)");
        let rewrite = extract_meta_var_names("logger.info($MGS)"); // typo
        let unbound: Vec<_> = rewrite.difference(&pattern).collect();
        assert_eq!(unbound, vec![&"MGS".to_string()]);
    }

    #[test]
    fn rewrite_subset_of_pattern_is_clean() {
        let pattern = extract_meta_var_names("test($NAME, () => { $$$BODY })");
        let rewrite = extract_meta_var_names("test($NAME, async () => { $$$BODY })");
        assert!(rewrite.difference(&pattern).next().is_none());
    }
}

/// Applies `(position, deleted_length, inserted)` edits sorted by descending
/// position, with the result of splicing them into `original` one by one in
/// that order.
///
/// Splicing one by one shifts the whole tail of the file per edit, so a file
/// with many matches cost edits x file size in copying. When the edits are in
/// bounds and do not overlap (the normal case) the result is built in one
/// forward pass instead; walking the list backwards gives ascending positions,
/// and edits at the same position come out in the same order the splices
/// would leave them. Anything else keeps the splice loop so its exact result
/// is preserved.
fn apply_descending_edits(original: &[u8], edits: &[(usize, usize, &[u8])]) -> Vec<u8> {
    let mut cursor = 0usize;
    let forward_safe = edits.iter().rev().all(|&(start, deleted, _)| {
        let Some(end) = start.checked_add(deleted) else {
            return false;
        };
        let ok = start >= cursor && end <= original.len();
        cursor = end;
        ok
    });
    if forward_safe {
        let inserted: usize = edits.iter().map(|(_, _, text)| text.len()).sum();
        let mut out = Vec::with_capacity(original.len() + inserted);
        let mut cursor = 0usize;
        for &(start, deleted, text) in edits.iter().rev() {
            out.extend_from_slice(&original[cursor..start]);
            out.extend_from_slice(text);
            cursor = start + deleted;
        }
        out.extend_from_slice(&original[cursor..]);
        #[cfg(test)]
        BYTES_MOVED.with(|moved| moved.set(moved.get() + out.len()));
        return out;
    }
    let mut new_bytes = original.to_vec();
    for &(start, deleted, text) in edits {
        let end = start + deleted;
        if start <= new_bytes.len() && end <= new_bytes.len() {
            #[cfg(test)]
            BYTES_MOVED.with(|moved| moved.set(moved.get() + (new_bytes.len() - end) + text.len()));
            new_bytes.splice(start..end, text.iter().copied());
        }
    }
    new_bytes
}

#[cfg(test)]
thread_local! {
    /// Bytes `apply_descending_edits` wrote or shifted on this thread.
    static BYTES_MOVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod apply_edits_tests {
    use super::*;

    /// The reference: splice each edit in the given (descending) order.
    fn splice_reference(original: &[u8], edits: &[(usize, usize, &[u8])]) -> Vec<u8> {
        let mut new_bytes = original.to_vec();
        for &(start, deleted, text) in edits {
            let end = start + deleted;
            if start <= new_bytes.len() && end <= new_bytes.len() {
                new_bytes.splice(start..end, text.iter().copied());
            }
        }
        new_bytes
    }

    #[test]
    fn forward_build_matches_splicing_including_ties_and_overlaps() {
        let original = b"abcdefghijklmnopqrstuvwxyz";
        let cases: Vec<Vec<(usize, usize, &[u8])>> = vec![
            vec![],
            vec![(20, 2, b"XX"), (10, 0, b"I"), (0, 3, b"")],
            // Two insertions at one position, and a replacement ending there.
            vec![(5, 0, b"second"), (5, 0, b"first"), (2, 3, b"R")],
            // Edit reaching the end of the input.
            vec![(24, 2, b"END"), (0, 0, b"START")],
            // Overlapping edits fall back to splicing.
            vec![(8, 6, b"o2"), (5, 6, b"o1")],
            // Out-of-bounds edits are skipped like before.
            vec![(40, 1, b"zz"), (3, 1, b"d")],
        ];
        for edits in cases {
            assert_eq!(
                apply_descending_edits(original, &edits),
                splice_reference(original, &edits),
                "{edits:?}"
            );
        }
    }

    #[test]
    fn many_edits_cost_one_pass_over_the_file() {
        let original = "x = foo(1);\n".repeat(2000).into_bytes();
        let mut edits: Vec<(usize, usize, &[u8])> = (0..2000)
            .map(|line| (line * 12 + 4, 3, b"bar".as_slice()))
            .collect();
        edits.reverse();
        let before = BYTES_MOVED.with(|moved| moved.get());
        let result = apply_descending_edits(&original, &edits);
        let moved = BYTES_MOVED.with(|moved| moved.get()) - before;
        assert_eq!(result, "x = bar(1);\n".repeat(2000).into_bytes());
        assert!(
            moved <= 2 * original.len(),
            "{moved} bytes moved to rewrite a {} byte file",
            original.len()
        );
    }
}
