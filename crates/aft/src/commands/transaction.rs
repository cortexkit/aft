//! Handler for the `transaction` command: multi-file atomic edits with rollback.
//!
//! Applies edits to multiple files atomically. Phase model:
//! 1. Parse & validate all operations upfront
//! 2. Snapshot all existing files
//! 3. Apply each operation (write or edit_match)
//! 4. Validate syntax on all results
//! 5. Rollback all on any failure; success only if every file passes

use std::collections::HashSet;
use std::path::PathBuf;

use crate::context::AppContext;
use crate::edit;
use crate::lsp::manager::PostEditWaitOutcome;
use crate::protocol::{RawRequest, Response};

/// A parsed operation ready for execution.
struct ParsedOp {
    file: PathBuf,
    kind: OpKind,
}

enum OpKind {
    Write {
        content: String,
    },
    EditMatch {
        match_str: String,
        replacement: String,
    },
}

/// Per-file result after a successful apply.
struct FileResult {
    file: String,
    syntax_valid: Option<bool>,
    formatted: bool,
    format_skipped_reason: Option<String>,
}

struct TransactionWriteResult {
    file_result: FileResult,
    file: PathBuf,
}

struct TransactionOpError {
    code: &'static str,
    message: String,
}

/// Handle a `transaction` request.
///
/// Params:
///   - `operations` (array, required) — each element is an object with:
///       - `file` (string) — target file path
///       - `command` (string) — "write" or "edit_match"
///       - For "write": `content` (string)
///       - For "edit_match": `match` (string) + `replacement` (string)
///
/// On success: `{ ok, files_modified, results: [{ file, syntax_valid, formatted, format_skipped_reason }] }`
/// On failure: `{ error: { code: "transaction_failed", message, failed_operation, rolled_back } }`
pub fn handle_transaction(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Parse operations ---
    let operations = match req.params.get("operations").and_then(|v| v.as_array()) {
        Some(ops) => ops,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "transaction: missing required param 'operations' (expected array)",
            );
        }
    };

    if operations.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "transaction: 'operations' array must not be empty",
        );
    }

    // Validate all operations upfront before any mutations
    let mut parsed: Vec<ParsedOp> = Vec::with_capacity(operations.len());
    for (i, op) in operations.iter().enumerate() {
        match parse_operation(op, i) {
            Ok(p) => match validate_operation_path(p, &req.id, ctx) {
                Ok(validated) => parsed.push(validated),
                Err(resp) => return resp,
            },
            Err(msg) => return Response::error(&req.id, "invalid_request", msg),
        }
    }

    // --- Snapshot phase ---
    // Track which files existed (snapshotted) vs new (will be deleted on rollback).
    let mut snapshotted_files: Vec<PathBuf> = Vec::new();
    let mut new_files: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for op in &parsed {
        if seen.contains(&op.file) {
            continue;
        }
        seen.insert(op.file.clone());

        if op.file.exists() {
            // Snapshot existing file — scoped borrow (D029)
            let snapshot_result = {
                let mut store = ctx.backup().borrow_mut();
                store.snapshot_with_op(req.session(), &op.file, "transaction", Some(&op_id))
            };
            if let Err(e) = snapshot_result {
                return Response::error(&req.id, e.code(), e.to_string());
            }
            snapshotted_files.push(op.file.clone());
        } else {
            let tombstone_result = {
                let mut store = ctx.backup().borrow_mut();
                store.snapshot_op_tombstone(
                    req.session(),
                    &op_id,
                    &op.file,
                    "transaction: file created by transaction",
                )
            };
            if let Err(e) = tombstone_result {
                return Response::error(&req.id, e.code(), e.to_string());
            }
            new_files.push(op.file.clone());
        }
    }

    // --- Apply phase ---
    let multi_file_write_paths: Vec<String> = parsed
        .iter()
        .map(|op| op.file.display().to_string())
        .collect();
    let lsp_params = params_with_multi_file_write_paths(&req.params, &multi_file_write_paths);
    let mut results: Vec<TransactionWriteResult> = Vec::new();

    for (i, op) in parsed.iter().enumerate() {
        let new_content = match compute_new_content(op) {
            Ok(c) => c,
            Err(err) => {
                let failures = rollback(ctx, req.session(), &snapshotted_files, &new_files);
                if failures.is_empty() {
                    ctx.backup()
                        .borrow_mut()
                        .discard_operation_entries(req.session(), &op_id);
                }
                return transaction_error(
                    &req.id,
                    err.code,
                    i,
                    &snapshotted_files,
                    &new_files,
                    &err.message,
                    &failures,
                );
            }
        };

        match edit::write_format_validate(&op.file, &new_content, &ctx.config(), &req.params) {
            Ok(wr) => {
                // Track this file as new if it was created by this operation
                // (in case earlier ops in the same transaction created it)
                if !snapshotted_files.contains(&op.file) && !new_files.contains(&op.file) {
                    new_files.push(op.file.clone());
                }
                results.push(TransactionWriteResult {
                    file: op.file.clone(),
                    file_result: FileResult {
                        file: op.file.display().to_string(),
                        syntax_valid: wr.syntax_valid,
                        formatted: wr.formatted,
                        format_skipped_reason: wr.format_skipped_reason,
                    },
                });
            }
            Err(e) => {
                let failures = rollback(ctx, req.session(), &snapshotted_files, &new_files);
                if failures.is_empty() {
                    ctx.backup()
                        .borrow_mut()
                        .discard_operation_entries(req.session(), &op_id);
                }
                return transaction_error(
                    &req.id,
                    "transaction_failed",
                    i,
                    &snapshotted_files,
                    &new_files,
                    &format!("write failed: {}", e),
                    &failures,
                );
            }
        }
    }

    // --- Validate phase: check syntax_valid on all results ---
    for (i, result) in results.iter().enumerate() {
        if result.file_result.syntax_valid == Some(false) {
            let failures = rollback(ctx, req.session(), &snapshotted_files, &new_files);
            if failures.is_empty() {
                ctx.backup()
                    .borrow_mut()
                    .discard_operation_entries(req.session(), &op_id);
            }
            return transaction_error(
                &req.id,
                "transaction_failed",
                i,
                &snapshotted_files,
                &new_files,
                &format!("syntax error in {}", result.file_result.file),
                &failures,
            );
        }
    }

    // --- Success ---
    //
    // Send the batched `workspace/didChangeWatchedFiles` notification exactly
    // once for the transaction. `lsp_post_write` reads `multi_file_write_paths`
    // from params on EVERY call; if we passed `lsp_params` (which carries the
    // full path list) into all N per-file invocations, every server would
    // receive N identical batched notifications instead of 1. That broke the
    // `transaction_success_batches_config_file_watched_notifications` test
    // intermittently under parallel load: the test's first `drain_events`
    // returned one notification, then the trailing duplicates landed in the
    // `is_none()` window 250 ms later and failed the assertion.
    //
    // Strategy: stripped_params has `multi_file_write_paths` removed. The
    // first per-file call still gets the full lsp_params (so the batched
    // notification fires once), and every subsequent call uses
    // stripped_params (so they only fire per-document didChange — no extra
    // batched notifications).
    let mut stripped_params = lsp_params.clone();
    if let Some(obj) = stripped_params.as_object_mut() {
        obj.remove("multi_file_write_paths");
    }

    let mut lsp_outcomes: Vec<PostEditWaitOutcome> = Vec::new();
    for (idx, result) in results.iter().enumerate() {
        if let Ok(final_content) = std::fs::read_to_string(&result.file) {
            let params_for_call = if idx == 0 {
                &lsp_params
            } else {
                &stripped_params
            };
            if let Some(lsp_outcome) =
                ctx.lsp_post_write(&result.file, &final_content, params_for_call)
            {
                lsp_outcomes.push(lsp_outcome);
            }
        }
    }

    let files_modified = results.len();
    let result_json: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            let file_result = &r.file_result;
            let mut v = serde_json::json!({
                "file": file_result.file,
                "syntax_valid": file_result.syntax_valid,
                "formatted": file_result.formatted,
            });
            if let Some(ref reason) = file_result.format_skipped_reason {
                v["format_skipped_reason"] = serde_json::json!(reason);
            }
            v
        })
        .collect();
    let lsp_outcome = merge_lsp_outcomes(lsp_outcomes.iter());

    log::debug!(
        "transaction: {} files modified successfully",
        files_modified
    );

    let mut result = serde_json::json!({
        "ok": true,
        "files_modified": files_modified,
        "results": result_json,
    });

    append_lsp_diagnostics_to_transaction(&mut result, lsp_outcome.as_ref());

    Response::success(&req.id, result)
}

fn params_with_multi_file_write_paths(
    params: &serde_json::Value,
    paths: &[String],
) -> serde_json::Value {
    let mut params = params.clone();
    params["multi_file_write_paths"] = serde_json::json!(paths);
    params
}

fn merge_lsp_outcomes<'a>(
    outcomes: impl Iterator<Item = &'a PostEditWaitOutcome>,
) -> Option<PostEditWaitOutcome> {
    let mut merged: Option<PostEditWaitOutcome> = None;

    for outcome in outcomes {
        let merged = merged.get_or_insert_with(PostEditWaitOutcome::default);
        merged.diagnostics.extend(outcome.diagnostics.clone());
        merged
            .pending_servers
            .extend(outcome.pending_servers.clone());
        merged.exited_servers.extend(outcome.exited_servers.clone());
    }

    merged
}

fn append_lsp_diagnostics_to_transaction(
    result: &mut serde_json::Value,
    outcome: Option<&PostEditWaitOutcome>,
) {
    let Some(outcome) = outcome else {
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

fn validate_operation_path(
    mut op: ParsedOp,
    req_id: &str,
    ctx: &AppContext,
) -> Result<ParsedOp, Response> {
    op.file = ctx.validate_path(req_id, &op.file)?;
    Ok(op)
}

/// Parse and validate a single operation object.
fn parse_operation(op: &serde_json::Value, index: usize) -> Result<ParsedOp, String> {
    let file = op
        .get("file")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("transaction: operation[{}] missing 'file'", index))?;

    let command = op
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("transaction: operation[{}] missing 'command'", index))?;

    let kind = match command {
        "write" => {
            let content = op.get("content").and_then(|v| v.as_str()).ok_or_else(|| {
                format!(
                    "transaction: operation[{}] 'write' requires 'content'",
                    index
                )
            })?;
            OpKind::Write {
                content: content.to_string(),
            }
        }
        "edit_match" => {
            let match_str = op.get("match").and_then(|v| v.as_str()).ok_or_else(|| {
                format!(
                    "transaction: operation[{}] 'edit_match' requires 'match'",
                    index
                )
            })?;
            let replacement = op
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    "transaction: edit_match operation requires 'replacement' field".to_string()
                })?;
            OpKind::EditMatch {
                match_str: match_str.to_string(),
                replacement: replacement.to_string(),
            }
        }
        other => {
            return Err(format!(
                "transaction: operation[{}] unknown command '{}' (expected 'write' or 'edit_match')",
                index, other
            ));
        }
    };

    Ok(ParsedOp {
        file: PathBuf::from(file),
        kind,
    })
}

/// Compute the new content for a single operation.
fn compute_new_content(op: &ParsedOp) -> Result<String, TransactionOpError> {
    match &op.kind {
        OpKind::Write { content } => Ok(content.clone()),
        OpKind::EditMatch {
            match_str,
            replacement,
        } => {
            let source = if op.file.exists() {
                std::fs::read_to_string(&op.file).map_err(|e| TransactionOpError {
                    code: "transaction_failed",
                    message: format!("failed to read {}: {}", op.file.display(), e),
                })?
            } else {
                String::new()
            };

            let (byte_start, byte_len) = find_single_fuzzy_match(&source, match_str, op)?;
            edit::replace_byte_range(&source, byte_start, byte_start + byte_len, replacement)
                .map_err(|e| TransactionOpError {
                    code: e.code(),
                    message: e.to_string(),
                })
        }
    }
}

fn find_single_fuzzy_match(
    source: &str,
    match_str: &str,
    op: &ParsedOp,
) -> Result<(usize, usize), TransactionOpError> {
    let fuzzy_matches = crate::fuzzy_match::find_all_fuzzy(source, match_str);

    if fuzzy_matches.is_empty() {
        return Err(TransactionOpError {
            code: "transaction_failed",
            message: format!("match '{}' not found in {}", match_str, op.file.display()),
        });
    }
    if fuzzy_matches.len() > 1 {
        return Err(TransactionOpError {
            code: "ambiguous_match",
            message: format!(
                "match '{}' is ambiguous ({} occurrences) in {}",
                match_str,
                fuzzy_matches.len(),
                op.file.display()
            ),
        });
    }

    let fuzzy_match = &fuzzy_matches[0];
    Ok((fuzzy_match.byte_start, fuzzy_match.byte_len))
}

/// Rollback failure information
struct RollbackFailure {
    file: String,
    action: String,
    error: String,
}

/// Rollback: restore snapshotted files (reverse order), delete new files.
/// Returns a list of files that failed to rollback.
fn rollback(
    ctx: &AppContext,
    session: &str,
    snapshotted: &[PathBuf],
    new_files: &[PathBuf],
) -> Vec<RollbackFailure> {
    let mut failures = Vec::new();

    // Restore snapshotted files in reverse order
    for path in snapshotted.iter().rev() {
        let result = {
            let mut store = ctx.backup().borrow_mut();
            store.restore_latest(session, path)
        };
        if let Err(e) = result {
            crate::slog_warn!(
                "transaction rollback: failed to restore {}: {}",
                path.display(),
                e
            );
            failures.push(RollbackFailure {
                file: path.display().to_string(),
                action: "restore".to_string(),
                error: e.to_string(),
            });
        }
    }

    // Delete new files
    for path in new_files {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                crate::slog_warn!(
                    "transaction rollback: failed to delete new file {}: {}",
                    path.display(),
                    e
                );
                failures.push(RollbackFailure {
                    file: path.display().to_string(),
                    action: "delete".to_string(),
                    error: e.to_string(),
                });
            }
        }
    }

    failures
}

/// Build the structured error response for a failed transaction.
fn transaction_error(
    req_id: &str,
    code: &str,
    failed_index: usize,
    snapshotted: &[PathBuf],
    new_files: &[PathBuf],
    message: &str,
    rollback_failures: &[RollbackFailure],
) -> Response {
    let mut rolled_back: Vec<serde_json::Value> = snapshotted
        .iter()
        .map(|p| serde_json::json!({ "file": p.display().to_string(), "action": "restored" }))
        .collect();

    for p in new_files {
        rolled_back.push(serde_json::json!({
            "file": p.display().to_string(),
            "action": "deleted",
        }));
    }

    log::debug!(
        "transaction failed at operation[{}]: {} — rolled back {} files",
        failed_index,
        message,
        rolled_back.len()
    );

    let mut data = serde_json::json!({
        "failed_operation": failed_index,
        "rolled_back": rolled_back,
    });

    if !rollback_failures.is_empty() {
        data["rollback_failures"] = serde_json::json!(rollback_failures
            .iter()
            .map(|f| serde_json::json!({
                "file": f.file,
                "action": f.action,
                "error": f.error,
            }))
            .collect::<Vec<_>>());
    }

    let code = if rollback_failures.is_empty() {
        code
    } else {
        "transaction_rollback_failed"
    };
    Response::error_with_data(req_id, code, message, data)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn transaction_error_uses_rollback_failed_code() {
        let response = transaction_error(
            "txn",
            "transaction_failed",
            1,
            &[],
            &[PathBuf::from("created.ts")],
            "write failed",
            &[RollbackFailure {
                file: "created.ts".to_string(),
                action: "delete".to_string(),
                error: "permission denied".to_string(),
            }],
        );
        let json = serde_json::to_value(response).expect("serialize response");

        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "transaction_rollback_failed");
        assert_eq!(json["rollback_failures"].as_array().unwrap().len(), 1);
    }
}
