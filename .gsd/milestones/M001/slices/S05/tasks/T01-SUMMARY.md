---
id: T01
parent: S05
milestone: M001
provides:
  - shared edit engine (src/edit.rs) with line_col_to_byte, replace_byte_range, validate_syntax, auto_backup
  - write command handler (full file write with auto-backup and syntax validation)
  - edit_symbol command handler with 4 operations and structured disambiguation
  - AmbiguousMatch error variant for T02's edit_match
  - ambiguous.ts fixture for disambiguation testing
key_files:
  - src/edit.rs
  - src/commands/write.rs
  - src/commands/edit_symbol.rs
  - src/error.rs
  - tests/integration/edit_test.rs
  - tests/fixtures/ambiguous.ts
key_decisions:
  - Symbol ranges from tree-sitter don't include export keywords — replacement content must match the function_declaration node range, not the enclosing export_statement
  - validate_syntax returns Option<bool> to cleanly handle unsupported languages (None = can't validate, not an error)
  - Disambiguation returns a success response with code field (not an error response) to give the caller structured candidates
patterns_established:
  - edit::auto_backup pattern: borrow RefCell, snapshot, drop borrow before returning — D029 discipline
  - edit::validate_syntax uses fresh FileParser per D023 (no cached provider)
  - Command handlers build JSON with serde_json::json! macro for type-safe response construction
observability_surfaces:
  - "[aft] write: {path}" on stderr for each write mutation
  - "[aft] edit_symbol: {symbol} in {path}" on stderr for each symbol edit
  - auto-backup entries visible via edit_history command
  - ambiguous_symbol response includes candidates with qualified names, line numbers, kinds
duration: 25min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Core edit engine, write command, and edit_symbol command

**Built shared edit engine, write command, and edit_symbol command with auto-backup, syntax validation, and structured disambiguation.**

## What Happened

Created `src/edit.rs` as the shared edit engine with four public functions: `line_col_to_byte` (0-indexed line/col to byte offset, tree-sitter byte-indexed columns), `replace_byte_range` (string splicing by byte range), `validate_syntax` (fresh FileParser per D023, returns `Option<bool>` for unsupported language handling), and `auto_backup` (RefCell borrow discipline per D029 — borrow, snapshot, drop before return).

Implemented `write` command: extracts file + content params, auto-backups existing files, optionally creates parent dirs, writes content, validates syntax for supported languages. Returns `{ file, created, syntax_valid?, backup_id? }`.

Implemented `edit_symbol` command: resolves symbol via `LanguageProvider::resolve_symbol`, handles disambiguation (multiple matches → filter by scope param → still multiple → return structured candidates with name, qualified, line, kind), applies 4 operations (replace/delete/insert_before/insert_after) using byte-range manipulation, auto-backups before write, validates syntax after write. Returns `{ file, symbol, operation, range, new_range?, syntax_valid, backup_id }`.

Added `AmbiguousMatch { pattern, count }` error variant to `AftError` — ready for T02's `edit_match` command.

Wired both commands into `main.rs` dispatch and `commands/mod.rs`.

Created `tests/fixtures/ambiguous.ts` with a top-level `process` function and a `DataHandler.process` method for disambiguation testing.

Wrote 8 integration tests covering: write creates new file, write backups existing (undo restores), write syntax valid/invalid, edit_symbol replace, edit_symbol delete, edit_symbol ambiguous (candidates returned), edit_symbol not found.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test` — 98 unit + 27 integration = 125 total, all pass ✅
- `cargo test --test integration edit` — 9 tests pass (8 edit + 1 edit_history) ✅
- `cargo test --lib edit` — 11 unit tests pass ✅
- Observability: `[aft] write: {path}` and `[aft] edit_symbol: {symbol} in {path}` confirmed on stderr ✅

### Slice-level verification status (T01 of 2):

- ✅ `cargo build` — 0 errors, 0 warnings
- ✅ `cargo test` — all existing + new tests pass
- ✅ `write` creates new file, returns `syntax_valid: true`
- ✅ `write` auto-backups existing file (undo restores original)
- ✅ `edit_symbol` replaces function, returns `syntax_valid: true`
- ✅ `edit_symbol` returns `ambiguous_symbol` candidates
- ✅ `edit_symbol` delete removes symbol
- ✅ Syntax validation catches intentional syntax error (`syntax_valid: false`)
- ⬜ `edit_match` replaces matched string (T02)
- ⬜ `edit_match` returns `ambiguous_match` candidates (T02)
- ⬜ `batch` applies multiple edits atomically (T02)
- ⬜ `batch` rolls back on failure (T02)

## Diagnostics

- `write` and `edit_symbol` emit stderr signals on every mutation
- `edit_history` command shows backup entries created by mutations
- `undo` command restores pre-mutation state for any backed-up file
- Ambiguous symbol resolution returns structured JSON with candidates array

## Deviations

None.

## Known Issues

- Symbol ranges from tree-sitter's `function_declaration` start at the `function` keyword, not at `export`. Callers of `edit_symbol replace` must provide replacement content that matches the symbol node's actual range (excluding export wrapper). This is correct behavior — it's how tree-sitter scopes the grammar node — but agents need to be aware of it when constructing replacement content.

## Files Created/Modified

- `src/edit.rs` — new: shared edit engine with line_col_to_byte, replace_byte_range, validate_syntax, auto_backup + 11 unit tests
- `src/commands/write.rs` — new: write command handler
- `src/commands/edit_symbol.rs` — new: edit_symbol command handler with disambiguation
- `src/error.rs` — added AmbiguousMatch variant
- `src/commands/mod.rs` — added write, edit_symbol module declarations
- `src/lib.rs` — added pub mod edit, AmbiguousMatch test
- `src/main.rs` — added write, edit_symbol dispatch arms
- `tests/integration/edit_test.rs` — new: 8 integration tests for write and edit_symbol
- `tests/integration/main.rs` — added edit_test module declaration
- `tests/fixtures/ambiguous.ts` — new: fixture with duplicate symbol names for disambiguation testing
