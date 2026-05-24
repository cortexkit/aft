---
id: S04
parent: M002
milestone: M002
provides:
  - dry_run: true support on all 12 mutation commands returning unified diff without modifying files
  - validate_syntax_str() for in-memory syntax checking without disk I/O
  - dry_run_diff() for unified diff generation using similar crate
  - transaction command with multi-file atomic edits, syntax-checked rollback, and new-file cleanup
  - Response::error_with_data for structured error responses with extra fields
  - Plugin registration: dry_run param on all 12 mutation tools, transaction tool with Zod schema
requires:
  - slice: S03
    provides: write_format_validate() shared mutation pipeline, BackupStore, all 12 mutation handlers
affects: []
key_files:
  - src/edit.rs
  - src/commands/transaction.rs
  - src/protocol.rs
  - tests/integration/dryrun_test.rs
  - tests/integration/transaction_test.rs
  - opencode-plugin-aft/src/tools/transaction.ts
key_decisions:
  - "D071: Dry-run shows raw edit diff only — formatting requires disk write, deferred"
  - "D072: Transaction limited to write and edit_match operations"
  - "D073: Syntax error triggers transaction rollback by default"
  - "D044: similar crate for unified diff generation"
patterns_established:
  - "Dry-run early return pattern: after computing new_source, check is_dry_run, return { ok, dry_run, diff, syntax_valid } — consistent 5-line insertion across all 12 handlers"
  - "Transaction phase model: parse → snapshot → apply → validate → rollback/success"
  - "Rollback tracks two categories: snapshotted (restored via backup) and new (deleted via fs::remove_file)"
observability_surfaces:
  - "Dry-run response: { ok, dry_run: true, diff, syntax_valid } — standard unified diff, parseable by patch/git apply"
  - "Transaction error: { ok: false, code: 'transaction_failed', failed_operation, rolled_back: [{ file, action }] }"
  - "stderr logs: [aft] transaction success/failure with file counts"
drill_down_paths:
  - .gsd/milestones/M002/slices/S04/tasks/T01-SUMMARY.md
  - .gsd/milestones/M002/slices/S04/tasks/T02-SUMMARY.md
  - .gsd/milestones/M002/slices/S04/tasks/T03-SUMMARY.md
duration: 65m
verification_result: passed
completed_at: 2026-03-14
---

# S04: Dry-run & Transactions

**Every mutation command now supports `dry_run: true` returning unified diffs without disk modification, and a new `transaction` command applies multi-file edits atomically with full rollback on syntax failure — completing M002.**

## What Happened

Three tasks, each building on the previous:

**T01 — Dry-run infrastructure.** Added `similar = "2"` crate. Created four shared functions in `edit.rs`: `validate_syntax_str` (in-memory syntax checking), `DryRunResult` (diff + syntax_valid), `dry_run_diff` (unified diff with a/b prefixes, 3-line context), `is_dry_run` (param extraction). Wired the 5-line early-return pattern into all 12 mutation handlers — each checks `is_dry_run`, computes new_source, returns diff without touching disk, creating backups, or invoking formatters. For handlers where backup preceded computation, wrapped in conditional. Batch clones original source before applying edits to preserve both sides for diff. 8 integration tests: write, edit_symbol (milestone acceptance), edit_match, batch, add_import, no-backup verification, syntax validation, empty-diff no-op.

**T02 — Transaction command.** Created `src/commands/transaction.rs` with five-phase model: parse (validate all operations upfront), snapshot (deduplicate files via HashSet, track new vs existing), apply (compute content + write_format_validate per operation), validate (check syntax_valid on all results), rollback (reverse-order restore for existing files, delete for new files). Added `Response::error_with_data()` to protocol.rs for structured error responses carrying `failed_operation` index and `rolled_back` array. Dry-run transaction bypasses all disk I/O, returns per-file diffs. 6 integration tests: success (3 files), rollback on syntax error (milestone acceptance), rollback with new file, edit_match operations, dry-run, empty operations rejection.

**T03 — Plugin registration.** Added `dry_run: z.boolean().optional()` to all 12 mutation tool schemas across editing.ts (4), imports.ts (3), structure.ts (5). Created `transaction.ts` with Zod schema for operations array and tool definition. Wired into index.ts. 3 bun tests: write dry-run round-trip, transaction success, transaction rollback.

## Verification

- `cargo test` — 175 unit + 119 integration = 294 tests, 0 failures ✅
- `cargo build` — 0 warnings ✅
- `bun test` — 39 tests, 157 assertions, 0 failures ✅
- Milestone acceptance: `edit_symbol` with `dry_run: true` → diff returned, file unchanged ✅
- Milestone acceptance: 3-file transaction, third has syntax error → all 3 rolled back ✅
- Dry-run no-backup: `edit_history` confirms no backup entries after dry-run ✅
- Dry-run syntax validation: `syntax_valid: false` for broken proposed content ✅
- Transaction new-file rollback: new file deleted, existing file restored ✅

## Requirements Validated

- R018 — Dry-run mode on all mutations: all 12 mutation commands accept `dry_run: true`, return unified diff and `syntax_valid` without modifying files, creating backups, or invoking formatters. Proven by 8 integration tests + 1 bun test.
- R019 — Multi-file atomic transactions: `transaction` command applies edits atomically with rollback on syntax failure. Proven by 6 integration tests + 2 bun tests including the milestone 3-file rollback acceptance scenario.

## New Requirements Surfaced

None.

## Requirements Invalidated or Re-scoped

None.

## Deviations

- D047 (dry-run includes formatted output) deferred as D071 — formatting requires external subprocess reading the file from disk, incompatible with the no-disk-write constraint. Agents can use real edit + undo for formatted preview.

## Known Limitations

- Transaction supports `write` and `edit_match` operations only — not the full 12 mutation commands (D072). Covers the primary use cases.
- Dry-run shows raw edit diff, not post-format result (D071). Agents needing formatted preview can apply + undo.
- Transaction rollback on syntax error is mandatory — no opt-out for partial-failure tolerance (D073).

## Follow-ups

None — S04 is the final slice of M002.

## Files Created/Modified

- `Cargo.toml` — added `similar = "2"` dependency
- `src/edit.rs` — added validate_syntax_str, DryRunResult, dry_run_diff, is_dry_run
- `src/commands/write.rs` — dry-run early return
- `src/commands/edit_symbol.rs` — dry-run early return
- `src/commands/edit_match.rs` — conditional backup + dry-run early return
- `src/commands/batch.rs` — conditional backup + source clone + dry-run early return
- `src/commands/add_import.rs` — conditional backup + dry-run early return
- `src/commands/remove_import.rs` — conditional backup + dry-run early return
- `src/commands/organize_imports.rs` — conditional backup + dry-run early return
- `src/commands/add_member.rs` — conditional backup + dry-run early return
- `src/commands/add_derive.rs` — dry-run early return
- `src/commands/add_decorator.rs` — conditional backup + dry-run early return
- `src/commands/add_struct_tags.rs` — dry-run early return
- `src/commands/wrap_try_catch.rs` — dry-run early return
- `src/commands/transaction.rs` — new transaction command handler
- `src/commands/mod.rs` — added pub mod transaction
- `src/main.rs` — added transaction dispatch arm
- `src/protocol.rs` — added Response::error_with_data()
- `tests/integration/dryrun_test.rs` — 8 integration tests
- `tests/integration/transaction_test.rs` — 6 integration tests
- `tests/integration/main.rs` — registered both test modules
- `opencode-plugin-aft/src/tools/editing.ts` — dry_run param on 4 tools
- `opencode-plugin-aft/src/tools/imports.ts` — dry_run param on 3 tools
- `opencode-plugin-aft/src/tools/structure.ts` — dry_run param on 5 tools
- `opencode-plugin-aft/src/tools/transaction.ts` — new transaction tool
- `opencode-plugin-aft/src/index.ts` — wired transactionTools
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — 3 new tests

## Forward Intelligence

### What the next slice should know
- M002 is complete. Next work is M003 (Call Graph Navigation) which is independent of S04's changes.
- All 20 commands (19 domain + ping) are now registered through the binary protocol and plugin.
- The `similar` crate is available for any future diff generation needs.

### What's fragile
- Transaction's `compute_new_content_dry` duplicates some logic from write/edit_match handlers to avoid disk I/O during dry-run — changes to those handlers need to be mirrored.

### Authoritative diagnostics
- `tests/integration/dryrun_test.rs` — proves dry-run correctness across all command types
- `tests/integration/transaction_test.rs` — proves atomicity and rollback including milestone acceptance scenario
- Send any mutation command with `"dry_run": true` to verify the path without side effects

### What assumptions changed
- D047 assumed dry-run could include formatted output — actual implementation showed formatters need disk files, so deferred as D071.
