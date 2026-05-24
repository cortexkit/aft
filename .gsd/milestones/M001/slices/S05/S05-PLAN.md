# S05: Three-Layer Editing Engine

**Goal:** All four mutation commands (`edit_symbol`, `edit_match`, `write`, `batch`) work with auto-backup before every mutation, tree-sitter syntax validation after every mutation, and symbol disambiguation for ambiguous targets.
**Demo:** Edit a real TypeScript file using each of the three edit modes (symbol, match, write), confirm syntax validation catches an intentional error, and run a batch of edits that applies atomically.

## Must-Haves

- `write` command creates/overwrites files with content from JSON, auto-backups existing files, returns `syntax_valid` for supported languages
- `edit_symbol` resolves symbols by name via tree-sitter, applies replace/delete/insert_before/insert_after operations, auto-backups, validates syntax, returns candidates on ambiguity
- `edit_match` finds content by string match, replaces single or all occurrences, returns candidates with context on ambiguity
- `batch` applies multiple edits to one file atomically (all succeed or rollback), sorted bottom-to-top to prevent line drift
- Every mutation auto-snapshots via `BackupStore.snapshot()` before modifying the file (completes R007)
- Every mutation response includes `syntax_valid` boolean from tree-sitter re-parse (R010)
- Ambiguous symbol resolution returns structured candidates with qualified names, line numbers, kinds (R011)
- `AmbiguousMatch` error variant added to `AftError` for edit_match disambiguation

## Proof Level

- This slice proves: contract + operational (commands work through JSON protocol with real file I/O, backup, and syntax validation)
- Real runtime required: yes (binary processes commands, reads/writes real files)
- Human/UAT required: no

## Verification

- `cargo build` — 0 errors, 0 warnings
- `cargo test` — all existing tests pass (no regressions), plus new unit tests in `src/edit.rs` and integration tests in `tests/integration/edit_test.rs`
- `tests/integration/edit_test.rs` — integration tests proving:
  - `write` creates a new file with correct content, returns `syntax_valid: true`
  - `write` auto-backups an existing file before overwriting (undo restores original)
  - `edit_symbol` replaces a function body, returns new range and `syntax_valid: true`
  - `edit_symbol` returns `ambiguous_symbol` candidates when multiple symbols match
  - `edit_symbol` delete operation removes a symbol
  - `edit_match` replaces a matched string, returns replacement count
  - `edit_match` returns `ambiguous_match` candidates when multiple occurrences exist
  - `batch` applies multiple edits atomically, returns `syntax_valid`
  - `batch` rolls back all edits when one fails (e.g., match not found)
  - Syntax validation catches intentional syntax error (returns `syntax_valid: false`)

## Observability / Diagnostics

- Runtime signals: `[aft] edit_symbol`, `[aft] edit_match`, `[aft] write`, `[aft] batch` on stderr for each mutation command
- Inspection surfaces: `edit_history` command (from S04) shows auto-backup entries created by mutation commands; `undo` restores pre-mutation state
- Failure visibility: structured JSON error codes (`ambiguous_symbol`, `ambiguous_match`, `symbol_not_found`, `file_not_found`, `invalid_request`) with contextual candidates in response body

## Integration Closure

- Upstream surfaces consumed: `src/parser.rs` (FileParser for syntax validation), `src/language.rs` (LanguageProvider for symbol resolution), `src/backup.rs` (BackupStore for auto-snapshot), `src/context.rs` (AppContext for state threading)
- New wiring introduced: 4 dispatch arms in `main.rs`, 4 command modules in `src/commands/`, shared `src/edit.rs` engine module
- What remains before the milestone is truly usable end-to-end: S06 (OpenCode plugin bridge), S07 (binary distribution)

## Tasks

- [x] **T01: Core edit engine, write command, and edit_symbol command** `est:1.5h`
  - Why: Builds the shared edit infrastructure (line-to-byte conversion, syntax validation, auto-backup orchestration) and the two primary mutation commands. `write` is the simplest full-file mutation; `edit_symbol` is the core semantic editing primitive. Both exercise all shared infrastructure.
  - Files: `src/edit.rs`, `src/commands/write.rs`, `src/commands/edit_symbol.rs`, `src/commands/mod.rs`, `src/error.rs`, `src/lib.rs`, `src/main.rs`, `tests/integration/edit_test.rs`, `tests/integration/main.rs`
  - Do: Create `src/edit.rs` with line_col_to_byte offset conversion, content replacement by byte range, syntax validation via fresh FileParser (D023 pattern), auto-backup orchestration. Implement `write` command handler (full file write, create dirs if needed, auto-backup existing files, syntax validation for supported languages). Implement `edit_symbol` command handler (resolve symbol, apply replace/delete/insert_before/insert_after, auto-backup, syntax validation, disambiguation with structured candidates). Wire both into dispatch. Add `AmbiguousMatch` error variant to `AftError`. Write integration tests covering write (new file, overwrite with backup), edit_symbol (replace, delete, disambiguation), and syntax validation (valid + intentionally broken edits).
  - Verify: `cargo test` — all tests pass, 0 warnings. Integration tests prove write creates files, edit_symbol replaces symbols, auto-backup enables undo, syntax_valid detects errors.
  - Done when: `write` and `edit_symbol` work end-to-end through the binary protocol with auto-backup and syntax validation.

- [x] **T02: edit_match command and atomic batch command** `est:1h`
  - Why: Completes the editing surface. `edit_match` provides content-based editing for when the agent knows the code shape. `batch` provides atomic multi-edit for preventing line drift. Both build on T01's edit engine.
  - Files: `src/commands/edit_match.rs`, `src/commands/batch.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/integration/edit_test.rs`
  - Do: Implement `edit_match` command handler (find string matches, single/all replacement, occurrence selection, ambiguity with context lines). Implement `batch` command handler (accept array of edits, sort bottom-to-top by position, apply atomically with single pre-backup, rollback on any failure). Wire both into dispatch. Add integration tests covering edit_match (single match, multiple occurrences with disambiguation, occurrence selection), batch (multiple edits applied, rollback on failure), and batch + undo round-trip.
  - Verify: `cargo test` — all tests pass including T01's tests (no regressions). Integration tests prove edit_match replaces content, batch applies atomically, batch rollback works.
  - Done when: All four mutation commands work through the binary protocol. Full slice verification passes.

## Files Likely Touched

- `src/edit.rs` (new — shared edit engine)
- `src/commands/write.rs` (new — write command)
- `src/commands/edit_symbol.rs` (new — edit_symbol command)
- `src/commands/edit_match.rs` (new — edit_match command)
- `src/commands/batch.rs` (new — batch command)
- `src/commands/mod.rs` (add 4 module declarations)
- `src/error.rs` (add AmbiguousMatch variant)
- `src/lib.rs` (add edit module)
- `src/main.rs` (add 4 dispatch arms)
- `tests/integration/edit_test.rs` (new — integration tests)
- `tests/integration/main.rs` (add edit_test module)
