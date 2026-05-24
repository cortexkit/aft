# S03: LSP-Enhanced Symbol Resolution — UAT

**Milestone:** M004
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: Both binary and plugin components are fully tested through automated integration tests (463 Rust + 55 bun). No runtime UI, no user-facing surfaces beyond JSON protocol. Mock clients verify plugin LSP query logic. Real binary protocol tests verify disambiguation end-to-end.

## Preconditions

- `cargo build` succeeds (binary available at `target/debug/aft`)
- `bun install` in `opencode-plugin-aft/` completes
- No LSP server required (tests use mock data / inline JSON)

## Smoke Test

Run `cargo test lsp_hints && cd opencode-plugin-aft && bun test` — all tests pass. This confirms the binary parses lsp_hints and the plugin queries LSP and populates hints.

## Test Cases

### 1. Ambiguous symbol resolved with LSP hints

1. Create a test file with two functions named `validate` (different scopes or same file, different lines)
2. Send `edit_symbol` command with `symbol: "validate"` and NO `lsp_hints`
3. **Expected:** Response has `code: "ambiguous_symbol"` with `candidates` array listing both
4. Re-send `edit_symbol` with `symbol: "validate"` and `lsp_hints: { symbols: [{ name: "validate", file: "<matching file path>", line: <line of first validate>, kind: "function" }] }`
5. **Expected:** Response is `ok: true` with the first `validate` selected — no ambiguity error

### 2. Zoom with LSP hints disambiguation

1. Using same ambiguous fixture, send `zoom` command with `symbol: "validate"` and no hints
2. **Expected:** Response has `code: "ambiguous_symbol"` with candidates
3. Send `zoom` with `symbol: "validate"` and `lsp_hints` matching the second `validate`
4. **Expected:** Response returns the second `validate`'s body with context lines

### 3. Malformed lsp_hints graceful fallback

1. Send `edit_symbol` with `lsp_hints: "not a valid object"`
2. **Expected:** Response falls back to tree-sitter behavior (returns candidates for ambiguous symbol). Binary stderr contains `[aft] lsp_hints: ignoring malformed data:`
3. Send `edit_symbol` with `lsp_hints: { symbols: [{ wrong_field: true }] }`
4. **Expected:** Same fallback — malformed inner structure is also handled gracefully

### 4. Absent lsp_hints — no regression

1. Send any existing `edit_symbol`, `zoom`, `move_symbol`, or `inline_symbol` command WITHOUT `lsp_hints` field
2. **Expected:** Behavior identical to pre-S03 — no errors, no missing fields, same disambiguation flow

### 5. Plugin LSP query — connected server

1. Create mock client with `lsp.status()` returning `{ data: [{ name: "typescript" }] }` and `find.symbols()` returning symbols with `file://` URI prefixed locations and numeric SymbolKind values
2. Call `queryLspHints(mockClient, "myFunction")`
3. **Expected:** Returns `{ symbols: [{ name, file (no file:// prefix), line, kind (mapped string) }] }`

### 6. Plugin LSP query — no connected server

1. Create mock client with `lsp.status()` returning `{ data: [] }` (empty server list)
2. Call `queryLspHints(mockClient, "myFunction")`
3. **Expected:** Returns `undefined` — no LSP query attempted

### 7. Plugin LSP query — API error

1. Create mock client where `find.symbols()` throws an error
2. Call `queryLspHints(mockClient, "myFunction")`
3. **Expected:** Returns `undefined`, console.warn logged with error message

### 8. Plugin tool round-trip with ToolContext

1. Run existing `bun test` suite (55 tests covering all 8 tool factories)
2. **Expected:** All tests pass — ToolContext refactor doesn't break any existing tool behavior
3. Verify each symbol-resolving tool (edit_symbol, zoom, move_symbol, inline_symbol, extract_function) accepts and passes lsp_hints through bridge

## Edge Cases

### Stale LSP hints (file changed since LSP query)

1. Send `edit_symbol` with `lsp_hints` where the line number doesn't match any candidate's range
2. **Expected:** Disambiguation falls back to tree-sitter behavior — returns all candidates as if no hints were provided

### LSP hint matches multiple candidates

1. Send `edit_symbol` with `lsp_hints` where the hint could potentially align with more than one candidate (e.g., two symbols with same name on adjacent lines, hint line within both ranges)
2. **Expected:** If exactly one candidate matches, it's selected. If multiple match, fallback to returning all candidates.

### file:// URI prefix handling

1. Plugin receives symbol with location `file:///Users/project/src/utils.ts`
2. **Expected:** `queryLspHints` strips prefix to `/Users/project/src/utils.ts` in wire format. Binary's `paths_match()` handles both formats via suffix comparison.

### Unknown SymbolKind number

1. Plugin receives symbol with `kind: 99` (not in LSP_SYMBOL_KIND_MAP)
2. **Expected:** `kind` field omitted from that symbol hint (not null, not error — just absent)

## Failure Signals

- `cargo test lsp_hints` fails — binary-side parsing or disambiguation broken
- `bun test` in `opencode-plugin-aft/` fails — plugin LSP query logic or ToolContext refactor broken
- `cargo test edit_symbol` or `cargo test zoom` fails — integration wiring broken
- Stderr shows `[aft] lsp_hints: ignoring malformed data` for valid hint data — parsing regression
- Plugin throws on queryLspHints instead of returning undefined — error handling broken

## Requirements Proved By This UAT

- R031 — LSP-aware architecture: lsp_hints parsed and consumed in 4 handlers, plugin populates for 5 commands, fallback to tree-sitter when absent
- R033 — LSP integration via plugin mediation: plugin queries lsp.status() → find.symbols() → maps to wire format → binary disambiguates

## Not Proven By This UAT

- Real LSP server integration — all tests use mock clients and inline JSON, not a running language server
- Production OpenCode SDK behavior — actual lsp.status() and find.symbols() response shapes assumed from SDK type signatures
- Performance under high-volume LSP queries — no load testing

## Notes for Tester

- The `lsp_hints` field is optional everywhere — its absence must never cause an error. The entire feature is additive.
- Binary-side tests use `tests/fixtures/ambiguous.ts` which has two `validate` functions at different lines — this is the primary disambiguation fixture.
- Plugin tests mock the SDK client — no external dependencies needed beyond `bun`.
- All 463 Rust tests and 55 bun tests should pass clean. Any failure indicates a regression.
