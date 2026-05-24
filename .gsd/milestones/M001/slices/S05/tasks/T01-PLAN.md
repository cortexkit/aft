---
estimated_steps: 8
estimated_files: 9
---

# T01: Core edit engine, write command, and edit_symbol command

**Slice:** S05 — Three-Layer Editing Engine
**Milestone:** M001

## Description

Build the shared edit engine (`src/edit.rs`) with line/col-to-byte-offset conversion, content replacement by byte range, tree-sitter syntax validation via fresh FileParser (D023), and auto-backup orchestration via BackupStore. Then implement `write` (full file write with auto-backup) and `edit_symbol` (symbol-level editing with resolve → backup → edit → validate cycle). Wire both into main.rs dispatch. Add `AmbiguousMatch` error variant to AftError for T02. Write integration tests proving the commands work end-to-end.

## Steps

1. **Add `AmbiguousMatch` error variant** to `src/error.rs` — add variant with `pattern: String, count: usize`, Display impl, error code `"ambiguous_match"`. Update the match arm in `code()`, `fmt()`, and the lib.rs test.

2. **Create `src/edit.rs`** — the shared edit engine module:
   - `line_col_to_byte(source: &str, line: u32, col: u32) -> usize` — converts 0-indexed line/col to byte offset. Tree-sitter columns are byte-indexed, so col can be used directly within a line. Handle edge cases: empty file, last line without trailing newline.
   - `replace_byte_range(source: &str, start: usize, end: usize, replacement: &str) -> String` — replaces bytes in range with new content.
   - `validate_syntax(path: &Path) -> Result<bool, AftError>` — creates a fresh FileParser (D023), parses the file, returns `!root_node.has_error()`. Returns `Ok(None)` equivalent for unsupported languages (caller decides how to report).
   - `auto_backup(ctx: &AppContext, path: &Path, description: &str) -> Result<Option<String>, AftError>` — if file exists, snapshots via `ctx.backup().borrow_mut().snapshot()` and returns the backup_id. If file doesn't exist, returns None. Must drop the borrow before returning.
   - Unit tests for line_col_to_byte: empty string, single line, multi-line, last line no trailing newline, multi-byte UTF-8.
   - Unit tests for replace_byte_range: basic replacement, empty replacement (delete), insert at same position.

3. **Create `src/commands/write.rs`** — write command handler:
   - Extract `file` (required) and `content` (required) params, optional `create_dirs` (bool, default false).
   - If file exists, auto-backup via `edit::auto_backup()`.
   - If `create_dirs` is true and parent doesn't exist, create parent directories.
   - Write content to file via `std::fs::write`.
   - Attempt syntax validation — return `syntax_valid` if language is supported, omit if not.
   - Return `{ file, created: bool, syntax_valid?, backup_id? }`.

4. **Create `src/commands/edit_symbol.rs`** — edit_symbol command handler:
   - Extract `file`, `symbol`, `operation` (replace/delete/insert_before/insert_after), optional `content`, optional `scope` params.
   - Resolve symbol via `ctx.provider().resolve_symbol(path, symbol_name)`.
   - **Disambiguation**: if multiple matches, filter by `scope` param if provided. If still multiple, return structured response with candidates: `{ code: "ambiguous_symbol", candidates: [{ name, qualified, line, kind }] }`. Follow zoom.rs pattern but richer.
   - **Single match**: read file content, convert symbol range to byte offsets via `edit::line_col_to_byte`, apply operation:
     - `replace`: replace byte range with `content`
     - `delete`: replace byte range with empty string
     - `insert_before`: insert `content` + newline before the symbol's start byte
     - `insert_after`: insert newline + `content` after the symbol's end byte
   - Auto-backup before writing.
   - Write modified content to file.
   - Validate syntax via `edit::validate_syntax`.
   - Return `{ file, symbol, operation, range, new_range?, syntax_valid, backup_id }`.

5. **Wire into dispatch** — add `write` and `edit_symbol` arms in `main.rs` dispatch function. Add module declarations in `src/commands/mod.rs` and `pub mod edit` in `src/lib.rs`.

6. **Write integration tests** in `tests/integration/edit_test.rs`:
   - `write_creates_new_file` — write to temp file, verify content.
   - `write_backups_existing_file` — write over existing, then undo, verify original restored.
   - `write_syntax_valid` — write valid TS, check `syntax_valid: true`.
   - `write_syntax_invalid` — write broken TS, check `syntax_valid: false`.
   - `edit_symbol_replace` — replace a function in sample.ts, verify new content and `syntax_valid: true`.
   - `edit_symbol_delete` — delete a function, verify it's gone.
   - `edit_symbol_ambiguous` — target a name that exists twice (need fixture), verify candidates returned.
   - `edit_symbol_not_found` — target nonexistent symbol, verify error.
   - Add `mod edit_test;` to `tests/integration/main.rs`.

7. **Build and test** — `cargo build` (0 warnings), `cargo test` (all pass).

## Must-Haves

- [ ] `edit.rs` line_col_to_byte handles 0-indexed line/col correctly with unit tests
- [ ] `edit.rs` validate_syntax uses fresh FileParser (not cached provider) per D023
- [ ] `edit.rs` auto_backup drops RefCell borrow before returning per D029 constraints
- [ ] `write` command creates files, auto-backups existing files, returns syntax_valid
- [ ] `edit_symbol` resolves symbols, applies all 4 operations, auto-backups, validates syntax
- [ ] `edit_symbol` returns structured disambiguation candidates when multiple symbols match
- [ ] `AmbiguousMatch` error variant added (ready for T02)
- [ ] Integration tests prove write and edit_symbol work through the binary protocol

## Verification

- `cargo build` — 0 errors, 0 warnings
- `cargo test` — all existing + new tests pass
- `cargo test --test integration edit` — edit integration tests pass in isolation
- `cargo test --lib edit` — edit engine unit tests pass

## Observability Impact

- Signals added: `[aft] write: {path}` and `[aft] edit_symbol: {symbol} in {path}` on stderr for each mutation
- How a future agent inspects this: `edit_history` command shows auto-backup entries created by write/edit_symbol
- Failure state exposed: `ambiguous_symbol` response includes candidates with qualified names, line numbers, and kinds

## Inputs

- `src/parser.rs` — `FileParser::new()` + `.parse(path)` for syntax validation; `tree.root_node().has_error()` for error detection
- `src/language.rs` — `LanguageProvider::resolve_symbol()` returns `Vec<SymbolMatch>` for disambiguation
- `src/backup.rs` — `BackupStore::snapshot()` for auto-backup before mutations
- `src/context.rs` — `AppContext` with `provider()`, `backup()` accessors
- `src/symbols.rs` — `Range` struct (0-indexed line/col), `Symbol`, `SymbolKind`
- `src/commands/zoom.rs` — disambiguation pattern to follow (resolve → check count → return candidates)
- `src/commands/undo.rs` — handler pattern to follow (extract params → borrow store → return response)
- S02 summary: Symbol ranges use 0-indexed line/col (tree-sitter convention)
- S04 summary: RefCell borrow discipline — drop borrows before next operation

## Expected Output

- `src/edit.rs` — shared edit engine with line_col_to_byte, replace_byte_range, validate_syntax, auto_backup + unit tests
- `src/commands/write.rs` — write command handler
- `src/commands/edit_symbol.rs` — edit_symbol command handler with disambiguation
- `src/error.rs` — AmbiguousMatch variant added
- `src/commands/mod.rs` — write, edit_symbol module declarations added
- `src/lib.rs` — `pub mod edit` added
- `src/main.rs` — write, edit_symbol dispatch arms added
- `tests/integration/edit_test.rs` — 8+ integration tests for write and edit_symbol
- `tests/integration/main.rs` — edit_test module declaration added
