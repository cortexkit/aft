---
id: T02
parent: S05
milestone: M001
provides:
  - edit_match command with string matching, occurrence selection, and structured disambiguation
  - batch command with atomic multi-edit, bottom-to-top sort, single backup, and validation-phase rollback
key_files:
  - src/commands/edit_match.rs
  - src/commands/batch.rs
  - tests/integration/edit_test.rs
key_decisions:
  - batch validation fails fast on any bad edit before taking backup — no backup taken on validation failure, keeping undo history clean
  - edit_match disambiguation returns occurrences with ±2 lines of context for agent decision-making
  - batch resolve_edit returns Response directly as Err for clean early-return in validation phase
patterns_established:
  - batch validation-then-apply: resolve all edits to byte offsets against original, sort descending, apply sequentially
  - line-range to byte-offset conversion for batch line-range edits (line_byte_offset / line_end_byte_offset)
observability_surfaces:
  - "[aft] edit_match: {pattern} in {path}" on stderr for each edit_match mutation
  - "[aft] batch: {n} edits in {path}" on stderr for each batch mutation
  - edit_history shows single backup entry per batch (not per-edit)
  - ambiguous_match response includes occurrences array with index, line, context
duration: 15min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: edit_match command and atomic batch command

**Implemented edit_match (string matching with disambiguation) and batch (atomic multi-edit with rollback), completing the four-command editing surface.**

## What Happened

Created `src/commands/edit_match.rs`: finds all occurrences of a match string in file content. Zero matches → `match_not_found` error. Single match (or `occurrence` specified) → auto-backup, replace, write, validate syntax, return `{ file, replacements: 1, syntax_valid, backup_id }`. Multiple matches without `occurrence` → return `ambiguous_match` with occurrences array containing `{ index, line, context }` where context is ±2 lines. Rejects empty match string with `invalid_request`. Out-of-range occurrence returns `invalid_request`.

Created `src/commands/batch.rs`: accepts `file` and `edits` array. Each edit is either `{ match, replacement }` (string match-replace) or `{ line_start, line_end, content }` (line range, 0-indexed inclusive). Two-phase execution: (1) validate all edits against original content — match edits must have exactly 1 occurrence (no disambiguation in batch), line-range edits must be in bounds. If any edit fails validation: return error immediately, no file modification, no backup. (2) On valid: auto-backup once, sort edits by byte_start descending (bottom-to-top), apply all sequentially, write result, validate syntax. Returns `{ file, edits_applied, syntax_valid, backup_id }`.

Wired both commands into `main.rs` dispatch and `commands/mod.rs`.

Wrote 8 integration tests covering the full specification: edit_match single occurrence, multiple occurrences disambiguation, occurrence selector, no match error, batch multiple edits, batch rollback on failure, batch line-range edit, batch with undo round-trip.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test` — 98 unit + 35 integration = 133 total, all pass ✅
- `cargo test --test integration edit` — 17 tests pass (T01's 9 + T02's 8) ✅
- Batch rollback test confirms file content unchanged after failed batch ✅
- Batch undo test confirms undo restores original after successful batch ✅
- Observability: `[aft] edit_match: {pattern} in {path}` and `[aft] batch: {n} edits in {path}` confirmed in handlers ✅

### Slice-level verification status (T02 of 2 — final):

- ✅ `cargo build` — 0 errors, 0 warnings
- ✅ `cargo test` — all existing + new tests pass (no regressions)
- ✅ `write` creates new file, returns `syntax_valid: true`
- ✅ `write` auto-backups existing file (undo restores original)
- ✅ `edit_symbol` replaces function, returns `syntax_valid: true`
- ✅ `edit_symbol` returns `ambiguous_symbol` candidates
- ✅ `edit_symbol` delete removes symbol
- ✅ `edit_match` replaces matched string, returns replacement count
- ✅ `edit_match` returns `ambiguous_match` candidates with context
- ✅ `batch` applies multiple edits atomically, returns `syntax_valid`
- ✅ `batch` rolls back on failure (file unchanged)
- ✅ Syntax validation catches intentional syntax error (`syntax_valid: false`)

## Diagnostics

- `edit_match` and `batch` emit stderr signals on every mutation
- `edit_history` command shows single backup entry per batch operation
- `undo` restores pre-mutation state for batch (single undo undoes entire batch)
- Ambiguous match resolution returns structured JSON with occurrences array (index, line, context)
- Batch failure returns which edit index failed in the error message

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/commands/edit_match.rs` — new: edit_match command handler with string matching, disambiguation, occurrence selection
- `src/commands/batch.rs` — new: batch command handler with atomic multi-edit, validation-then-apply, rollback on failure
- `src/commands/mod.rs` — added batch, edit_match module declarations
- `src/main.rs` — added edit_match, batch dispatch arms
- `tests/integration/edit_test.rs` — added 8 integration tests for edit_match and batch commands
