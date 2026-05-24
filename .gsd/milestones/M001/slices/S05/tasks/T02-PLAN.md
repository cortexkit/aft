---
estimated_steps: 5
estimated_files: 5
---

# T02: edit_match command and atomic batch command

**Slice:** S05 — Three-Layer Editing Engine
**Milestone:** M001

## Description

Implement the remaining two mutation commands: `edit_match` (content-based string matching with disambiguation) and `batch` (atomic multi-edit with rollback). Both build on T01's edit engine for auto-backup and syntax validation. `edit_match` finds lines by string content and replaces them; `batch` accepts an array of edits, sorts bottom-to-top, and applies atomically with a single pre-backup and rollback on failure.

## Steps

1. **Create `src/commands/edit_match.rs`** — edit_match command handler:
   - Extract `file` (required), `match` (required, non-empty string), `replacement` (required string), optional `occurrence` (0-indexed integer to select a specific match).
   - Read file content. Find all occurrences of `match` string in the file content.
   - **Zero matches**: return `symbol_not_found`-style error (match not found in file).
   - **Single match** (or `occurrence` specified): auto-backup, replace the occurrence, write file, validate syntax. Return `{ file, replacements: 1, syntax_valid, backup_id }`.
   - **Multiple matches without `occurrence`**: return `ambiguous_match` response with all occurrences listed: `{ code: "ambiguous_match", occurrences: [{ index, line, context }] }` where context is the matched line ±2 lines. Don't modify the file.
   - When `occurrence` is specified but out of range, return `invalid_request` error.
   - Use `edit::auto_backup` and `edit::validate_syntax` from T01.

2. **Create `src/commands/batch.rs`** — batch command handler:
   - Extract `file` (required), `edits` (required array). Each edit is either `{ match, replacement }` (string match-replace) or `{ line_start, line_end, content }` (line range replacement, 0-indexed).
   - Validate all edits against the original content before applying any:
     - For match edits: find the match string, ensure exactly 1 occurrence (ambiguous = error for batch, no interactive disambiguation).
     - For line-range edits: validate line numbers are within bounds.
   - If validation passes: auto-backup once (single snapshot of original file).
   - Sort edits by position descending (bottom-to-top) to prevent line drift.
   - Apply all edits sequentially to the content string.
   - Write the result, validate syntax.
   - If any edit fails validation: return error, no file modification, no backup taken.
   - Return `{ file, edits_applied: n, syntax_valid, backup_id }`.

3. **Wire into dispatch** — add `edit_match` and `batch` arms in `main.rs` dispatch. Add module declarations in `src/commands/mod.rs`.

4. **Add integration tests** to `tests/integration/edit_test.rs`:
   - `edit_match_single_occurrence` — unique string replaced, content verified.
   - `edit_match_multiple_occurrences_returns_candidates` — ambiguous match returns occurrences with context.
   - `edit_match_with_occurrence_selector` — select specific occurrence by index.
   - `edit_match_no_match` — match string not found, error returned.
   - `batch_multiple_edits` — two match-replace edits applied atomically, content verified.
   - `batch_rollback_on_failure` — second edit's match not found, file unchanged from original.
   - `batch_line_range_edit` — replace a line range, content verified.
   - `batch_with_undo` — batch then undo restores original.

5. **Full verification** — `cargo build` (0 warnings), `cargo test` (all pass including T01 tests).

## Must-Haves

- [ ] `edit_match` finds string occurrences, replaces single/selected match, auto-backups, validates syntax
- [ ] `edit_match` returns structured disambiguation with line numbers and context for multiple occurrences
- [ ] `edit_match` rejects empty match string with `invalid_request`
- [ ] `batch` applies multiple edits atomically (all or nothing)
- [ ] `batch` sorts edits bottom-to-top before applying to prevent line drift
- [ ] `batch` rolls back (no file modification) when any edit fails validation
- [ ] `batch` takes a single auto-backup before applying edits
- [ ] Integration tests prove edit_match and batch work through the binary protocol

## Verification

- `cargo build` — 0 errors, 0 warnings
- `cargo test` — all tests pass (T01 + T02 + all prior slices)
- `cargo test --test integration edit` — all edit integration tests pass
- Specifically: batch rollback test confirms file content unchanged after failed batch

## Observability Impact

- Signals added: `[aft] edit_match: {pattern} in {path}` and `[aft] batch: {n} edits in {path}` on stderr
- How a future agent inspects this: `edit_history` shows single backup entry per batch (not per-edit)
- Failure state exposed: `ambiguous_match` with `occurrences` array; batch failure returns which edit index failed

## Inputs

- `src/edit.rs` — `auto_backup()`, `validate_syntax()`, `replace_byte_range()` from T01
- `src/error.rs` — `AmbiguousMatch` variant added in T01
- `src/commands/write.rs`, `src/commands/edit_symbol.rs` — patterns established in T01
- `tests/integration/edit_test.rs` — test file started in T01, add more tests
- T01 task summary — any deviations or pattern changes from T01 execution

## Expected Output

- `src/commands/edit_match.rs` — edit_match command handler with disambiguation
- `src/commands/batch.rs` — batch command handler with atomic apply and rollback
- `src/commands/mod.rs` — edit_match, batch module declarations added
- `src/main.rs` — edit_match, batch dispatch arms added
- `tests/integration/edit_test.rs` — 8+ additional integration tests for edit_match and batch
