---
estimated_steps: 5
estimated_files: 5
---

# T02: Add transaction command with multi-file atomicity and rollback

**Slice:** S04 — Dry-run & Transactions
**Milestone:** M002

## Description

Create the `transaction` command handler that applies edits to multiple files atomically. Follows the phase model: parse all operations → snapshot all files → apply each (write or edit_match) → check syntax_valid → rollback all on any failure. Handles both existing files (restore from backup) and new files (delete on rollback). Also supports `dry_run: true` for previewing multi-file changes.

## Steps

1. Create `src/commands/transaction.rs` with `handle_transaction(req, ctx) -> Response`. Parse `operations` array from params — each operation has `file`, `command` ("write" or "edit_match"), and command-specific params (`content` for write; `match` + `replacement` for edit_match). Validate all operations upfront before any mutations.
2. Implement the phase model:
   - **Snapshot phase**: For each unique file that exists on disk, call `ctx.backup().borrow_mut().snapshot(path, "transaction")`. Track which files were snapshotted vs which are new (don't exist yet). Drop borrow between iterations (D029).
   - **Apply phase**: For each operation, read current source (or empty string for new files), compute new content (full write or find/replace for edit_match), call `write_format_validate()`. Collect per-file `WriteResult`. If `write_format_validate` returns an error, trigger rollback immediately.
   - **Validate phase**: Check `syntax_valid` on all results. If any file has `syntax_valid == Some(false)`, trigger rollback.
   - **Rollback**: For snapshotted files, call `backup.borrow_mut().restore_latest(path)` in reverse order. For new files, delete them via `fs::remove_file`. Return error response with `code: "transaction_failed"`, `failed_operation` index, `rolled_back` file list, and failure message.
   - **Success**: Return `{ ok: true, files_modified, results: [{ file, syntax_valid, formatted, format_skipped_reason }] }`.
3. Add dry-run support: when `dry_run: true`, compute all new content and generate per-file diffs using `dry_run_diff()` without any disk writes or backups. Return `{ ok: true, dry_run: true, diffs: [{ file, diff, syntax_valid }] }`.
4. Wire into dispatch: add `pub mod transaction;` in `src/commands/mod.rs` and `"transaction" => aft::commands::transaction::handle_transaction(&req, ctx)` in `src/main.rs`.
5. Write `tests/integration/transaction_test.rs` with integration tests:
   - `transaction_success_three_files` — write 3 files atomically, all succeed, verify all 3 modified on disk
   - `transaction_rollback_syntax_error` — 3-file transaction, third file has syntax error, verify all 3 rolled back to original content (milestone acceptance scenario)
   - `transaction_rollback_new_file` — transaction creates a new file then a later operation fails, verify new file is deleted on rollback
   - `transaction_edit_match_operation` — transaction with edit_match operations
   - `transaction_dry_run` — transaction with dry_run returns per-file diffs, no files modified
   - `transaction_empty_operations` — empty operations array returns error
   - Register `transaction_test` module in `tests/integration/main.rs`

## Must-Haves

- [ ] Transaction applies all operations or rolls back all on failure
- [ ] Syntax error in any file triggers full rollback
- [ ] Rollback restores snapshotted files in reverse order
- [ ] New files (created during transaction) are deleted on rollback
- [ ] Transaction supports both `write` and `edit_match` per-file operations
- [ ] Dry-run transaction returns per-file diffs without touching disk
- [ ] Error response includes `failed_operation` index and `rolled_back` file list
- [ ] RefCell borrows are scoped tightly (no re-entrant panics)

## Verification

- `cargo test` — all existing tests pass + new transaction_test tests pass
- `cargo build` — 0 warnings
- The 3-file rollback acceptance scenario from milestone success criteria passes

## Observability Impact

- Signals added/changed: transaction error response includes `failed_operation` index, `rolled_back` array, and descriptive `message`
- How a future agent inspects this: error response fields are structured and parseable
- Failure state exposed: which operation index failed, which files were rolled back, whether files were new (deleted) or existing (restored)

## Inputs

- `src/edit.rs` — `write_format_validate()`, `auto_backup()`, `dry_run_diff()`, `is_dry_run()` (from T01)
- `src/backup.rs` — `BackupStore::snapshot()`, `restore_latest()`
- Existing handler patterns in `src/commands/batch.rs` — closest pattern to transaction

## Expected Output

- `src/commands/transaction.rs` — new command handler
- `src/commands/mod.rs` — `pub mod transaction;` added
- `src/main.rs` — `"transaction"` dispatch arm added
- `tests/integration/transaction_test.rs` — 6 integration tests
- `tests/integration/main.rs` — `transaction_test` module registered
