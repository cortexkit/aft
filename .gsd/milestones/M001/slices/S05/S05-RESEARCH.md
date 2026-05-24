# S05: Three-Layer Editing Engine — Research

**Date:** 2026-03-14

## Summary

S05 builds four mutation commands (`edit_symbol`, `edit_match`, `write`, `batch`) on top of the tree-sitter symbol engine (S02) and backup system (S04). The codebase is well-prepared: `LanguageProvider.resolve_symbol()` already returns `Vec<SymbolMatch>` for disambiguation, `BackupStore.snapshot()` handles pre-mutation backup, and tree-sitter 0.24's `Node::has_error()` provides ~1ms syntax validation via re-parse.

The primary risk is range arithmetic — converting tree-sitter's 0-indexed line/column positions to byte offsets for content replacement. Tree-sitter columns are byte-indexed (not character-indexed), which simplifies UTF-8 handling but requires careful line-boundary accounting. The batch command's bottom-to-top sort prevents line drift but needs careful atomicity: all edits must be validated against the *original* content positions before any are applied.

The slice is medium-complexity, roughly 4 new command handlers + 1 shared edit engine module + dispatcher wiring + tests. Existing patterns (D021 command module pattern, D025/D026 AppContext threading, D023 standalone FileParser for AST access) cover all architecture questions. No new dependencies needed.

## Recommendation

Build a shared `src/edit.rs` module containing the core edit engine (line-to-byte conversion, content replacement, syntax validation, auto-backup orchestration). Each command handler (`edit_symbol`, `edit_match`, `write`, `batch`) is a thin command module in `src/commands/` that parses request params and delegates to edit.rs primitives. Follow D023's pattern: edit handlers create their own `FileParser` for post-edit syntax validation rather than extending the `LanguageProvider` trait.

Task breakdown:
1. **T01**: `src/edit.rs` (core engine) + `write` command + `edit_symbol` command — these share the most infrastructure (auto-backup, syntax validation, file write). Test with integration tests.
2. **T02**: `edit_match` command + `batch` command — match-based editing and atomic multi-edit. Batch builds on the engine from T01.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Symbol resolution | `LanguageProvider.resolve_symbol()` | Returns Vec<SymbolMatch> with scope chains — perfect for disambiguation |
| File backup before mutation | `BackupStore.snapshot(path, description)` | Already wired through AppContext via RefCell |
| Syntax validation | `tree_sitter::Node::has_error()` | Built into tree-sitter 0.24, ~1ms re-parse |
| Command dispatch pattern | `src/commands/` module pattern (D021) | All existing handlers follow `(&RawRequest, &AppContext) -> Response` |
| State threading | `AppContext` with RefCell stores (D025/D029) | Provider + BackupStore accessible from any handler |

## Existing Code and Patterns

- `src/parser.rs` — `FileParser::parse(path) -> (&Tree, LangId)` for parsing, `extract_symbols(path) -> Vec<Symbol>` for symbol extraction. Cache invalidates on mtime change. `node_range()` converts tree-sitter nodes to `Range { start_line, start_col, end_line, end_col }` (all 0-indexed). `detect_language(path)` returns `LangId` for supported extensions.
- `src/parser.rs::TreeSitterProvider` — implements `LanguageProvider` via `RefCell<FileParser>`. `resolve_symbol()` returns all name matches, never errors on multiple matches (caller decides).
- `src/symbols.rs` — `Range` uses 0-indexed line/col (tree-sitter convention, `start_position().row`). S02 summary incorrectly states "1-based" — actual code is 0-based. The edit engine must use 0-based indexing.
- `src/backup.rs` — `BackupStore::snapshot(&mut self, path, description) -> Result<String, AftError>` reads file from disk and stores content. `restore_latest()` pops and writes back.
- `src/commands/zoom.rs` — handles ambiguous symbols by returning `ambiguous_symbol` error with candidates list. Creates own `FileParser` for AST access (D023). Good pattern to follow for edit_symbol disambiguation.
- `src/commands/undo.rs` — clean handler pattern: extract params from `req.params`, borrow store, return Response. Follow this.
- `src/error.rs` — `AftError::AmbiguousSymbol` variant exists with `name: String, candidates: Vec<String>`. For richer disambiguation in edit_symbol, we can return a structured JSON response directly (candidates with qualified names, line numbers, kinds) rather than using this error variant.
- `src/context.rs` — `ctx.backup().borrow_mut().snapshot(path, desc)` for auto-backup. Drop the borrow before doing subsequent operations to avoid RefCell panics.
- `tests/integration/helpers.rs` — `AftProcess::spawn()` and `send()` pattern for integration tests. `fixture_path()` for test files.

## Constraints

- **0-indexed ranges**: Symbol.range uses 0-indexed line and column numbers (tree-sitter convention). The edit engine must convert these to byte offsets for string slicing. Tree-sitter columns are byte-indexed within the line, simplifying UTF-8 handling.
- **RefCell borrow discipline**: Handlers receive `&AppContext` (immutable). Mutable borrows via `borrow_mut()` must not overlap. Sequence: resolve symbol (borrows provider) → drop that borrow → snapshot backup (borrows backup store) → drop → write file → validate. Never hold two `borrow_mut()` simultaneously.
- **Single-threaded**: Binary processes one request at a time (stdin read loop). No concurrency concerns for RefCell, but no parallelism either.
- **FileParser cache keyed by path + mtime**: After writing an edited file, mtime changes, so subsequent `parse()` calls will re-parse. This is correct for validation but means we can't use incremental parsing (tree-sitter's edit API) — we do a full re-parse. Acceptable since files are typically small and tree-sitter parses are fast (~1ms).
- **Handler signature**: `(&RawRequest, &AppContext) -> Response` per D026. New handlers must follow this.
- **All content through JSON**: File content, replacement text — all passed as JSON string values. No shell arguments.
- **Auto-backup before EVERY mutation**: S04's BackupStore is ready. S05 must call `snapshot()` before `write/edit_symbol/edit_match/batch`. This completes R007.

## Common Pitfalls

- **Off-by-one in line-to-byte conversion** — The most fragile part. A line offset calculation that's wrong by 1 byte will corrupt files silently. Must have unit tests for: empty file, single-line file, file ending with newline, file ending without newline, multi-byte UTF-8 characters, Windows line endings (though primary targets are Unix).
- **Batch edit ordering** — Edits must be sorted bottom-to-top (descending start position) before applying. If sorted wrong, each edit shifts subsequent positions. Must sort by byte offset, not line number, since two edits could be on the same line.
- **RefCell double-borrow panic** — If a handler borrows BackupStore and then tries to borrow it again (e.g., nested function call), it panics at runtime. Keep borrows short-scoped: `{ let mut backup = ctx.backup().borrow_mut(); backup.snapshot(...); }` — drop before next use.
- **Ambiguity in edit_match with empty string** — `match: ""` would match everywhere. Validate that match string is non-empty.
- **Newline handling at edit boundaries** — When replacing a symbol, the replacement content should maintain the file's newline convention. When inserting before/after, need to add appropriate newlines.
- **Stale cache after rapid edits** — Filesystem mtime has 1-second granularity on some systems. Two edits within 1 second could leave a stale cache entry. For syntax validation, create a fresh FileParser (D023 pattern) rather than using the cached one in TreeSitterProvider.

## Open Risks

- **Edit boundaries for decorated/attributed symbols** — Python decorated functions include the decorator in the symbol's range (per S02). When `edit_symbol` replaces such a function with `operation: "replace"`, should the decorator be included in the replacement? Current behavior: the decorator IS part of the range (tree-sitter includes it). The agent probably wants to replace just the function body. May need an option or convention — for now, replace the entire range including decorators, document it clearly.
- **Indentation preservation** — `edit_symbol` with `insert_before`/`insert_after` doesn't know the correct indentation level. The content must come pre-indented from the agent. This matches the "agent generates code" model but could lead to indentation mismatches. Auto-format (R016, M002) will fix this later.
- **Batch rollback on partial failure** — If edit 3 of 5 fails (e.g., match not found), we need to restore from the single backup taken before any edits were applied. The backup must capture the file state before batch begins, not between individual edits.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | plurigrid/asi@tree-sitter | available (7 installs) — low value, not recommended |
| Rust | github/awesome-copilot@rust-mcp-server-generator | available (7K installs) — MCP focused, not relevant |

No skills worth installing — the codebase patterns are well-established and tree-sitter usage is straightforward.

## Key Technical Details

### Line/Col to Byte Offset Algorithm

```
fn line_col_to_byte(source: &str, line: u32, col: u32) -> usize {
    let mut byte_offset = 0;
    for (i, src_line) in source.split('\n').enumerate() {
        if i == line as usize {
            return byte_offset + col as usize;
        }
        byte_offset += src_line.len() + 1; // +1 for \n
    }
    byte_offset // EOF fallback
}
```

This works because tree-sitter columns are byte-indexed. Edge case: last line with no trailing newline — handled by the EOF fallback.

### Syntax Validation Pattern

```
fn validate_syntax(path: &Path) -> Result<bool, AftError> {
    let mut parser = FileParser::new();
    let (tree, _lang) = parser.parse(path)?;
    Ok(!tree.root_node().has_error())
}
```

Fresh FileParser avoids cache staleness issues. ~1ms overhead per validation.

### Command JSON Contracts

**edit_symbol**: `{ command: "edit_symbol", file, symbol, operation: "replace"|"delete"|"insert_before"|"insert_after", content?, scope? }` → `{ file, symbol, range, new_range?, syntax_valid, backup_id }` or `{ code: "ambiguous_symbol", candidates: [{name, qualified, line, kind}] }`

**edit_match**: `{ command: "edit_match", file, match, replacement, occurrence? }` or `{ command: "edit_match", file, from, to, replacement }` → `{ file, replacements, syntax_valid, backup_id }` or `{ code: "ambiguous_match", occurrences: [{index, line, context}] }`

**write**: `{ command: "write", file, content, create_dirs? }` → `{ file, created, syntax_valid?, backup_id? }`

**batch**: `{ command: "batch", file, edits: [{match, replacement} | {line_start, line_end, content}] }` → `{ file, edits_applied, syntax_valid, backup_id }`

### Error Variant Needed

Add `AmbiguousMatch` to `AftError` enum for edit_match disambiguation:
```rust
AmbiguousMatch {
    pattern: String,
    count: usize,
}
```

## Sources

- tree-sitter 0.24 Rust API: `Node::has_error()` confirms syntax error detection (source: cargo registry `tree-sitter-0.24.7/binding_rust/lib.rs`)
- S02 Forward Intelligence: TreeSitterProvider, FileParser, symbol ranges, extract_symbols API
- S04 Forward Intelligence: AppContext threading, BackupStore.snapshot() API, RefCell borrow patterns
- D023: Zoom handler creates own FileParser rather than extending LanguageProvider — edit handlers should follow same pattern for syntax validation
- D025/D026: AppContext + handler signature conventions
