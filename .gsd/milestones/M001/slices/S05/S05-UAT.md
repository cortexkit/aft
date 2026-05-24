# S05: Three-Layer Editing Engine — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: live-runtime
- Why this mode is sufficient: All four mutation commands require real file I/O, real tree-sitter parsing, and real backup store interactions. The binary must be running to exercise these paths.

## Preconditions

- `cargo build` succeeds with 0 errors, 0 warnings
- A temp directory is available for test file creation
- The `aft` binary is available at `target/debug/aft`
- The `tests/fixtures/ambiguous.ts` fixture exists (two `process` functions in different scopes)

## Smoke Test

1. Start the `aft` binary: `echo '{"id":"1","command":"ping"}' | target/debug/aft`
2. **Expected:** Response with `{"id":"1","ok":true}`

## Test Cases

### 1. Write creates a new file

1. Start the binary as a persistent process
2. Send: `{"id":"1","command":"write","file":"/tmp/aft-uat/new.ts","content":"export function hello(): string { return 'world'; }\n"}`
3. **Expected:** Response with `ok: true`, `created: true`, `syntax_valid: true`
4. Verify `/tmp/aft-uat/new.ts` contains the exact content sent

### 2. Write auto-backups existing file and undo restores it

1. Write a file with known content via the `write` command
2. Overwrite it with different content via another `write` command
3. **Expected:** Second write returns `backup_id` (non-null), `created: false`
4. Send `{"id":"3","command":"undo","file":"/tmp/aft-uat/new.ts"}`
5. **Expected:** File content matches the original (first write), not the overwrite

### 3. Edit_symbol replaces a function body

1. Write a TypeScript file with: `function greet(name: string): string { return 'hi ' + name; }\n`
2. Send: `{"id":"2","command":"edit_symbol","file":"...","symbol":"greet","operation":"replace","content":"function greet(name: string): string { return 'hello ' + name; }\n"}`
3. **Expected:** Response with `ok: true`, `symbol: "greet"`, `operation: "replace"`, `syntax_valid: true`, `backup_id` present
4. Read the file — the function body should contain `'hello '` instead of `'hi '`

### 4. Edit_symbol returns disambiguation candidates

1. Use the `tests/fixtures/ambiguous.ts` file (has top-level `process` and `DataHandler.process`)
2. Send: `{"id":"2","command":"edit_symbol","file":"tests/fixtures/ambiguous.ts","symbol":"process","operation":"replace","content":"..."}`
3. **Expected:** Response with `ok: true`, `code: "ambiguous_symbol"`, `candidates` array containing at least 2 entries, each with `name`, `qualified`, `line`, `kind`

### 5. Edit_symbol delete removes a symbol

1. Write a TypeScript file with two functions: `function keep() {}` and `function remove() {}`
2. Send edit_symbol with `symbol: "remove"`, `operation: "delete"`
3. **Expected:** Response with `ok: true`, `operation: "delete"`, `syntax_valid: true`
4. Read the file — `remove` function is gone, `keep` function remains

### 6. Edit_match replaces a single occurrence

1. Write a file containing `const x = "hello";`
2. Send: `{"id":"2","command":"edit_match","file":"...","match":"hello","replacement":"world"}`
3. **Expected:** Response with `ok: true`, `replacements: 1`, `syntax_valid: true`, `backup_id` present
4. Read the file — `"world"` replaces `"hello"`

### 7. Edit_match returns disambiguation for multiple occurrences

1. Write a file containing `hello` on three separate lines
2. Send edit_match with `match: "hello"` and no `occurrence`
3. **Expected:** Response with `ok: true`, `code: "ambiguous_match"`, `occurrences` array with 3 entries, each containing `index`, `line`, `context` (±2 lines around match)

### 8. Edit_match with occurrence selector

1. Using the same file from test 7, send edit_match with `match: "hello"`, `replacement: "world"`, `occurrence: 1`
2. **Expected:** Response with `ok: true`, `replacements: 1`
3. Read the file — only the second occurrence (index 1) is replaced

### 9. Batch applies multiple edits atomically

1. Write a TypeScript file with multiple distinct strings
2. Send batch with 2+ match-replace edits targeting different strings
3. **Expected:** Response with `ok: true`, `edits_applied` matching edit count, `syntax_valid: true`, single `backup_id`
4. Read file — all replacements applied

### 10. Batch rolls back on failure

1. Write a TypeScript file with known content
2. Send batch where the first edit is valid but the second targets a non-existent match
3. **Expected:** Response with `ok: false`, error indicating which edit failed
4. Read file — content is unchanged (no partial application, no backup created)

### 11. Syntax validation catches intentional error

1. Write a valid TypeScript file
2. Overwrite it with syntactically invalid TypeScript (e.g., `function broken( { }`)
3. **Expected:** Response with `syntax_valid: false`

## Edge Cases

### Write to nested directory that doesn't exist

1. Send write to `/tmp/aft-uat/deep/nested/dir/file.ts`
2. **Expected:** Directories created automatically, file written, response with `created: true`

### Edit_match with empty match string

1. Send edit_match with `match: ""`
2. **Expected:** Error response with `code: "invalid_request"`

### Edit_symbol for non-existent symbol

1. Send edit_symbol targeting a symbol name that doesn't exist in the file
2. **Expected:** Error response with `code: "symbol_not_found"`

### Edit_match with no matches found

1. Send edit_match with a match string that doesn't appear in the file
2. **Expected:** Error response with `code: "match_not_found"`

### Batch with line-range edit

1. Write a file with 5 lines
2. Send batch with a line-range edit: `{"line_start": 1, "line_end": 2, "content": "replaced\n"}`
3. **Expected:** Lines 1-2 (0-indexed) replaced with new content, response `ok: true`

### Write to unsupported language file

1. Send write with a `.txt` file
2. **Expected:** Response with `syntax_valid: null` (not false — indicates language not supported for validation)

## Failure Signals

- Any `cargo test` failure in `tests/integration/edit_test.rs`
- `syntax_valid` returning `false` for known-valid code
- `backup_id` missing from mutation responses on existing files
- `undo` failing to restore pre-mutation content
- Batch partially applying edits (file modified but error returned)
- Disambiguation responses missing candidates or context

## Requirements Proved By This UAT

- R004 — edit_symbol resolves symbols, applies 4 operations, validates syntax, disambiguates
- R005 — edit_match finds content by string, handles single/multiple/selected occurrences
- R006 — write creates/overwrites via JSON, batch applies atomically with rollback
- R007 — every mutation auto-backups, undo restores pre-mutation state
- R010 — every response includes syntax_valid from tree-sitter re-parse
- R011 — ambiguous symbols return structured candidates with qualified names, lines, kinds

## Not Proven By This UAT

- R010 full validation mode (type-checker invocation) — deferred to R017/M002
- Cross-file batch editing — batch is single-file only by design
- Performance under load (hundreds of rapid edits) — not a UAT concern for this slice
- OpenCode plugin integration — deferred to S06

## Notes for Tester

- All tests should be run against a fresh binary instance (start the binary, run tests, shut down)
- The `ambiguous.ts` fixture at `tests/fixtures/ambiguous.ts` is required for disambiguation tests
- `syntax_valid` is `null` (not `false`) for unsupported languages — this is intentional, not a bug
- Batch rollback means the file is never modified — verify by reading the file, not by checking undo history (no backup is created on validation failure)
- Symbol ranges exclude the `export` keyword — when providing replacement content for exported functions, don't include `export` in the replacement
