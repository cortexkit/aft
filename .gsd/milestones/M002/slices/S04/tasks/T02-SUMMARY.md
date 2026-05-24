---
id: T02
parent: S04
milestone: M002
provides:
  - transaction command handler with multi-file atomic edits and rollback
  - Response::error_with_data for structured error responses with extra fields
key_files:
  - src/commands/transaction.rs
  - src/protocol.rs
  - tests/integration/transaction_test.rs
key_decisions:
  - "Separated dry-run content computation from apply-phase computation: dry-run reads files once and passes original to compute_new_content_dry, avoiding re-reads during snapshot-less path"
  - "Used HashSet to deduplicate file snapshots when multiple operations target the same file"
  - "Added Response::error_with_data to protocol.rs rather than inlining JSON construction — keeps transaction error responses clean and makes the pattern available to future commands"
patterns_established:
  - "Phase model pattern: parse → snapshot → apply → validate → rollback/success with clear separation between phases"
  - "Rollback tracks two categories: snapshotted (existing files restored via backup) and new (files deleted via fs::remove_file)"
  - "Transaction error shape: { ok: false, code: 'transaction_failed', message, failed_operation: index, rolled_back: [{ file, action }] }"
observability_surfaces:
  - "Transaction error response includes failed_operation index and rolled_back array with per-file action (restored/deleted)"
  - "stderr logging: [aft] transaction success/failure messages with file counts"
duration: 20m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Add transaction command with multi-file atomicity and rollback

**Created `transaction` command handler that applies edits to multiple files atomically with syntax-checked rollback, plus dry-run preview support.**

## What Happened

Built `src/commands/transaction.rs` implementing the full phase model:

1. **Parse phase**: Validates all operations upfront — each needs `file`, `command` ("write" or "edit_match"), and command-specific params. Empty operations array rejected immediately.
2. **Snapshot phase**: Deduplicates files via HashSet, snapshots existing files via `backup.snapshot()` with scoped RefCell borrows (D029), tracks new files separately for rollback cleanup.
3. **Apply phase**: Computes new content per operation (full write or match-replace), calls `write_format_validate()`. On any write failure, triggers rollback immediately.
4. **Validate phase**: Checks `syntax_valid` on all results. Any `Some(false)` triggers full rollback.
5. **Rollback**: Restores snapshotted files in reverse order via `backup.restore_latest()`, deletes new files via `fs::remove_file`. Error response carries structured `failed_operation` index and `rolled_back` array.

Dry-run path bypasses all disk I/O — reads originals, computes diffs via `dry_run_diff()`, returns per-file `{ file, diff, syntax_valid }`.

Added `Response::error_with_data()` to `src/protocol.rs` for merging extra structured fields into error responses.

Wired dispatch in `main.rs` and registered module in `mod.rs`.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 175 unit + 119 integration tests pass ✅
- 6 new transaction integration tests all pass:
  - `transaction_success_three_files` — 3 files atomically modified, verified on disk ✅
  - `transaction_rollback_syntax_error` — milestone acceptance scenario: 3-file transaction, third has syntax error, all 3 rolled back ✅
  - `transaction_rollback_new_file` — new file deleted on rollback, existing file restored ✅
  - `transaction_edit_match_operation` — edit_match operations work within transaction ✅
  - `transaction_dry_run` — per-file diffs returned, files unchanged on disk ✅
  - `transaction_empty_operations` — empty operations rejected with error ✅

Slice-level verification (partial — T02 is intermediate):
- `cargo test` — all pass ✅
- `cargo build` — 0 warnings ✅
- `bun test` in `opencode-plugin-aft/` — not yet (T03 adds plugin registration)
- Transaction tests prove success/rollback/new-file/dry-run paths ✅
- Milestone acceptance: 3-file rollback scenario passes ✅

## Diagnostics

- Send `transaction` command with `operations` array to apply multi-file edits atomically
- On failure, response includes `failed_operation` (0-indexed), `rolled_back` array with per-file `{ file, action }` where action is "restored" or "deleted"
- Use `dry_run: true` to preview with per-file diffs in `diffs` array
- stderr logs `[aft] transaction: N files modified successfully` or `[aft] transaction failed at operation[N]: reason — rolled back M files`

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/commands/transaction.rs` — new transaction command handler (parse, snapshot, apply, validate, rollback phases)
- `src/commands/mod.rs` — added `pub mod transaction;`
- `src/main.rs` — added `"transaction"` dispatch arm
- `src/protocol.rs` — added `Response::error_with_data()` method
- `tests/integration/transaction_test.rs` — 6 integration tests
- `tests/integration/main.rs` — registered `transaction_test` module
