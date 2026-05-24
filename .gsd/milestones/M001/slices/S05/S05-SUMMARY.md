---
id: S05
parent: M001
milestone: M001
provides:
  - write command (full file create/overwrite with auto-backup and syntax validation)
  - edit_symbol command (symbol-level editing with 4 operations, auto-backup, syntax validation, structured disambiguation)
  - edit_match command (content-based editing with string matching, occurrence selection, disambiguation with context)
  - batch command (atomic multi-edit for single file, bottom-to-top sort, single backup, validation-phase rollback)
  - shared edit engine (src/edit.rs) with line_col_to_byte, replace_byte_range, validate_syntax, auto_backup
  - AmbiguousMatch error variant in AftError
requires:
  - slice: S02
    provides: FileParser for syntax validation, LanguageProvider for symbol resolution, Symbol types
  - slice: S04
    provides: BackupStore for auto-snapshot before every mutation, AppContext for state threading
affects:
  - S06
key_files:
  - src/edit.rs
  - src/commands/write.rs
  - src/commands/edit_symbol.rs
  - src/commands/edit_match.rs
  - src/commands/batch.rs
  - src/error.rs
  - tests/integration/edit_test.rs
  - tests/fixtures/ambiguous.ts
key_decisions:
  - Symbol ranges from tree-sitter exclude export keywords — replacement content targets function_declaration node, not export_statement wrapper
  - validate_syntax returns Option<bool> — None for unsupported languages (clean distinction from validation failure)
  - Disambiguation returns success response with code field, not error response — gives caller structured candidates
  - Batch validates all edits against original content before taking backup — no backup on validation failure keeps undo history clean
  - edit_match disambiguation returns occurrences with ±2 lines context for agent decision-making
patterns_established:
  - edit::auto_backup — borrow RefCell, snapshot, drop borrow before returning (D029 discipline)
  - edit::validate_syntax — fresh FileParser per D023 (no cached provider)
  - batch validation-then-apply — resolve all edits to byte offsets against original, sort descending, apply sequentially
  - Command handlers build JSON with serde_json::json! macro for type-safe response construction
observability_surfaces:
  - "[aft] write: {path}" on stderr for each write mutation
  - "[aft] edit_symbol: {symbol} in {path}" on stderr for each symbol edit
  - "[aft] edit_match: {pattern} in {path}" on stderr for each match edit
  - "[aft] batch: {n} edits in {path}" on stderr for each batch mutation
  - auto-backup entries visible via edit_history command
  - ambiguous_symbol and ambiguous_match responses include structured candidates
drill_down_paths:
  - .gsd/milestones/M001/slices/S05/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S05/tasks/T02-SUMMARY.md
duration: 40min
verification_result: passed
completed_at: 2026-03-14
---

# S05: Three-Layer Editing Engine

**Four mutation commands (write, edit_symbol, edit_match, batch) with auto-backup, syntax validation, and structured disambiguation — completing the editing surface for the agent toolkit.**

## What Happened

Built the shared edit engine (`src/edit.rs`) with four primitives: `line_col_to_byte` (0-indexed line/col to byte offset using tree-sitter's byte-indexed columns), `replace_byte_range` (string splicing), `validate_syntax` (fresh FileParser returning `Option<bool>` for clean unsupported-language handling), and `auto_backup` (RefCell borrow/snapshot/drop discipline per D029).

**T01** delivered `write` and `edit_symbol`. `write` handles full file create/overwrite with auto-backup of existing files, directory creation, and syntax validation. `edit_symbol` resolves symbols via `LanguageProvider::resolve_symbol`, applies 4 operations (replace/delete/insert_before/insert_after) using byte-range manipulation, auto-backups before write, validates syntax after write, and returns structured disambiguation candidates when multiple symbols match a name.

**T02** delivered `edit_match` and `batch`. `edit_match` finds string occurrences in file content — single match auto-applies, multiple matches return candidates with ±2 lines of context, and an `occurrence` parameter allows index-based selection. `batch` accepts an array of edits (string match-replace or line-range) for a single file, validates all edits against original content before taking backup, sorts by byte offset descending to prevent drift, and applies atomically — any validation failure returns an error without modifying the file.

All four commands wired into `main.rs` dispatch. `AmbiguousMatch` error variant added to `AftError`.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test` — 98 unit + 35 integration = 133 total, all pass ✅
- `write` creates new file, returns `syntax_valid: true` ✅
- `write` auto-backups existing file (undo restores original) ✅
- `edit_symbol` replaces function body, returns new range and `syntax_valid: true` ✅
- `edit_symbol` returns `ambiguous_symbol` candidates when multiple symbols match ✅
- `edit_symbol` delete removes symbol ✅
- `edit_match` replaces matched string, returns replacement count ✅
- `edit_match` returns `ambiguous_match` candidates with context lines ✅
- `batch` applies multiple edits atomically, returns `syntax_valid` ✅
- `batch` rolls back on failure (file unchanged) ✅
- Syntax validation catches intentional syntax error (`syntax_valid: false`) ✅
- Observability: all four stderr signals confirmed in handlers ✅

## Requirements Advanced

- R004 (edit_symbol) — symbol-level editing with 4 operations and structured disambiguation working through JSON protocol
- R005 (edit_match) — content-based editing with string matching, occurrence selection, and disambiguation
- R006 (write + batch) — full file write via JSON stdin, atomic multi-edit with bottom-to-top sort and rollback
- R007 (auto-backup) — every mutation auto-snapshots via BackupStore before modifying files (completes the R007 chain from S04)
- R010 (syntax validation) — every edit response includes `syntax_valid` from tree-sitter re-parse
- R011 (disambiguation) — ambiguous symbol targets return structured candidates with qualified names, line numbers, kinds

## Requirements Validated

- R004 — edit_symbol works end-to-end: resolve symbol → apply operation → auto-backup → syntax validate → return result. All 4 operations tested. Disambiguation with structured candidates proven.
- R005 — edit_match works end-to-end: find matches → handle single/multiple/selected → auto-backup → replace → syntax validate. Disambiguation with context lines proven.
- R006 — write creates/overwrites files via JSON. batch applies multiple edits atomically with rollback on failure. Both proven through integration tests.
- R010 — every mutation response includes `syntax_valid` boolean. Intentional syntax errors detected. Unsupported languages return `null` (not false).
- R011 — disambiguation returns structured candidates with name, qualified name, line, kind. Tested with ambiguous.ts fixture containing duplicate symbol names in different scopes.

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- R007 — can now be marked validated. S04 built the BackupStore infrastructure; S05 proves every mutation auto-snapshots before modifying files. Integration tests confirm undo restores pre-mutation state for write, edit_symbol, edit_match, and batch.

## Deviations

None.

## Known Limitations

- Symbol ranges from tree-sitter's `function_declaration` start at the `function` keyword, not at `export`. Agents providing replacement content for `edit_symbol replace` must provide content matching the symbol node's actual range (excluding export wrapper). This is correct tree-sitter behavior but agents need to be aware.
- `batch` does not support disambiguation — each match edit in a batch must have exactly one occurrence. Agents should resolve ambiguities with individual `edit_match` calls first.
- Syntax validation is tree-sitter only (structural syntax). Type-level validation is deferred to R017 (M002).

## Follow-ups

- none — all planned work complete, no new work discovered

## Files Created/Modified

- `src/edit.rs` — new: shared edit engine with 4 public functions + 11 unit tests
- `src/commands/write.rs` — new: write command handler
- `src/commands/edit_symbol.rs` — new: edit_symbol command handler with disambiguation
- `src/commands/edit_match.rs` — new: edit_match command handler with occurrence selection
- `src/commands/batch.rs` — new: batch command handler with atomic multi-edit and rollback
- `src/commands/mod.rs` — added 4 module declarations
- `src/error.rs` — added AmbiguousMatch variant
- `src/lib.rs` — added pub mod edit
- `src/main.rs` — added 4 dispatch arms
- `tests/integration/edit_test.rs` — new: 17 integration tests for all edit commands
- `tests/integration/main.rs` — added edit_test module
- `tests/fixtures/ambiguous.ts` — new: fixture with duplicate symbol names for disambiguation testing

## Forward Intelligence

### What the next slice should know
- All four mutation commands follow the same pattern: extract params → auto-backup → apply → validate syntax → build response. The `src/edit.rs` module documents this flow.
- The JSON protocol for all S05 commands is visible in the integration tests (`tests/integration/edit_test.rs`) — these are the authoritative examples of request/response shapes for Zod schema generation in S06.
- `AppContext` (from S04) threads all shared state. S06's tool registrations need to map OpenCode tool parameters to the JSON request format.

### What's fragile
- `edit_symbol` depends on tree-sitter symbol ranges being accurate. If tree-sitter query patterns change in the future, edit ranges will shift. The integration tests in `edit_test.rs` would catch this.
- `batch` sorts by byte offset descending — this assumes edits don't overlap. Overlapping edits would produce incorrect results silently.

### Authoritative diagnostics
- `tests/integration/edit_test.rs` — 17 tests covering all 4 commands, all edge cases, and rollback behavior. If any edit behavior breaks, these tests pinpoint it.
- stderr signals (`[aft] write:`, `[aft] edit_symbol:`, `[aft] edit_match:`, `[aft] batch:`) — confirm commands are being dispatched.
- `edit_history` command — shows all auto-backup entries created by mutations. Use this to verify backups are being created.

### What assumptions changed
- none — implementation matched the plan
