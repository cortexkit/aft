# S03: LSP-Enhanced Symbol Resolution

**Goal:** When `lsp_hints` JSON is present in a command request, the binary uses it to disambiguate symbols that tree-sitter alone returns as ambiguous. When absent or invalid, behavior is unchanged.
**Demo:** Integration test sends `edit_symbol` with `lsp_hints` containing a file+line for one of two ambiguous symbols — the correct one is selected without returning candidates. Same test without `lsp_hints` returns the usual ambiguous_symbol response.

## Must-Haves

- `LspHints` struct defined in the binary with `symbols: Vec<LspSymbolHint>` where each hint has `name`, `file`, `line`, `kind`
- `lsp_hints` deserialized and consumed in all 4 disambiguation paths: `edit_symbol`, `zoom`, `move_symbol`, `inline_symbol`
- When hints match a single tree-sitter candidate (by file+line), that candidate is auto-selected
- When hints are absent, behavior is identical to current (no regression)
- When hints are malformed or stale (no matching candidate), graceful fallback to tree-sitter behavior
- Plugin tool factories receive `client` alongside `bridge`, with type-safe `ToolContext` wrapper
- Plugin queries `client.lsp.status()` before `client.find.symbols()` — skips LSP query when no server is connected
- Plugin populates `lsp_hints` in bridge params for `edit_symbol`, `zoom`, `move_symbol`, `inline_symbol`, `extract_function`
- LSP SymbolKind numbers mapped to AFT SymbolKind strings (12→function, 5→class, 6→method, etc.)
- `cargo test` passes (baseline 446 + new tests)
- `bun test` passes (baseline 42 + new tests)

## Proof Level

- This slice proves: integration (binary consumes lsp_hints through protocol; plugin queries SDK and populates hints)
- Real runtime required: no (binary tests use mock JSON; plugin tests use mock client)
- Human/UAT required: no

## Verification

- `cargo test lsp_hints` — unit tests for `LspHints` parsing, kind mapping, and disambiguation logic
- `cargo test edit_symbol` — integration tests including lsp_hints disambiguation, fallback, and malformed hints
- `cargo test zoom` — integration test with lsp_hints disambiguation
- `bun test` in `opencode-plugin-aft/` — tests proving LSP query, hint building, fallback when LSP unavailable, and round-trip through bridge
- Existing `cargo test` and `bun test` baselines pass without regression

## Observability / Diagnostics

- Runtime signals: stderr log `[aft] lsp_hints: parsed N symbol hints` when hints are present; `[aft] lsp_hints: ignoring malformed data` on parse failure
- Inspection surfaces: response JSON unchanged — no new fields. Disambiguation resolves silently when hints match.
- Failure visibility: malformed hints logged to stderr with the parse error detail; command proceeds without hints (no user-visible error)
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: `req.lsp_hints: Option<serde_json::Value>` in `RawRequest` (M001/D003), `resolve_symbol()` in all 4 command handlers, OpenCode SDK `client.find.symbols()` and `client.lsp.status()`
- New wiring introduced: `LspHints` struct + `apply_lsp_disambiguation()` in binary; `ToolContext` type + `queryLspHints()` helper in plugin; `lsp_hints` field populated in bridge params
- What remains before the milestone is truly usable end-to-end: nothing — this is the terminal slice (M004 complete after S03)

## Tasks

- [x] **T01: Binary-side lsp_hints consumption and disambiguation** `est:45m`
  - Why: The binary already receives `lsp_hints` as `Option<serde_json::Value>` but never parses or uses it. This task makes 4 command handlers consume it for disambiguation.
  - Files: `src/lsp_hints.rs`, `src/lib.rs`, `src/commands/edit_symbol.rs`, `src/commands/zoom.rs`, `src/commands/move_symbol.rs`, `src/commands/inline_symbol.rs`, `tests/integration/lsp_hints_test.rs`, `tests/integration/main.rs`
  - Do: Define `LspHints` struct with `symbols: Vec<LspSymbolHint>` (`name`, `file`, `line`, `kind: Option<String>`). Add `parse_lsp_hints(req) -> Option<LspHints>` that defensively deserializes from `req.lsp_hints`. Add `apply_lsp_disambiguation(matches, hints) -> Vec<SymbolMatch>` that filters tree-sitter matches by file+line alignment with LSP hints. Wire into the disambiguation path of all 4 handlers — between scope filter and ambiguous_symbol response. Add unit tests for parsing (valid, malformed, missing) and disambiguation logic (single match, no match → fallback, stale hint). Add integration tests sending `lsp_hints` JSON through the binary protocol: (a) ambiguous symbol resolved with hints, (b) same symbol without hints returns candidates, (c) malformed hints → fallback, (d) zoom with hints.
  - Verify: `cargo test lsp_hints` passes; `cargo test edit_symbol` passes; `cargo test zoom` passes; full `cargo test` baseline holds
  - Done when: `edit_symbol` for an ambiguous symbol with matching `lsp_hints` returns the single correct result instead of candidates; without hints, same call returns candidates as before

- [x] **T02: Plugin-side LSP query and lsp_hints population** `est:45m`
  - Why: The plugin must query OpenCode's LSP infrastructure and pass symbol data to the binary. Without this, `lsp_hints` is always empty in production.
  - Files: `opencode-plugin-aft/src/types.ts`, `opencode-plugin-aft/src/lsp.ts`, `opencode-plugin-aft/src/index.ts`, `opencode-plugin-aft/src/tools/editing.ts`, `opencode-plugin-aft/src/tools/reading.ts`, `opencode-plugin-aft/src/tools/refactoring.ts`, `opencode-plugin-aft/src/tools/navigation.ts`, `opencode-plugin-aft/src/__tests__/tools.test.ts`, `opencode-plugin-aft/src/__tests__/lsp.test.ts`
  - Do: Create `ToolContext` type bundling `bridge` + `client`. Change tool factory signatures from `(bridge: BinaryBridge)` to `(ctx: ToolContext)`. Update `index.ts` to construct context from `input.client` + `bridge`. Create `lsp.ts` module with `queryLspHints(client, symbolName, directory?)` that checks `lsp.status()` → if connected, calls `find.symbols({ query: { query: symbolName } })` → maps SDK `Symbol[]` to the `lsp_hints` wire format (strip `file://` prefix, map SymbolKind numbers). In tool execute functions for edit_symbol, zoom, move_symbol, inline_symbol, extract_function: extract symbol name from params, call `queryLspHints()`, merge result into bridge params as `lsp_hints`. Add `lsp.test.ts` with mock client testing: connected server → hints returned, no server → empty hints, API error → empty hints. Update existing bun test to work with new `ToolContext` signature.
  - Verify: `bun test` in `opencode-plugin-aft/` passes with all existing + new tests; no regressions
  - Done when: Plugin tools query LSP and populate `lsp_hints` for symbol-resolution commands; mock-client tests prove all three paths (connected, disconnected, error); existing bun tests pass with the refactored factory signatures

## Files Likely Touched

- `src/lsp_hints.rs` — new: LspHints struct, parsing, disambiguation logic, unit tests
- `src/lib.rs` — modified: `pub mod lsp_hints`
- `src/commands/edit_symbol.rs` — modified: wire lsp_hints into disambiguation
- `src/commands/zoom.rs` — modified: wire lsp_hints into disambiguation
- `src/commands/move_symbol.rs` — modified: wire lsp_hints into disambiguation
- `src/commands/inline_symbol.rs` — modified: wire lsp_hints into disambiguation
- `tests/integration/lsp_hints_test.rs` — new: integration tests for lsp_hints through protocol
- `tests/integration/main.rs` — modified: add `mod lsp_hints_test`
- `opencode-plugin-aft/src/types.ts` — new: ToolContext type
- `opencode-plugin-aft/src/lsp.ts` — new: queryLspHints helper, SymbolKind mapping
- `opencode-plugin-aft/src/index.ts` — modified: construct ToolContext, update factory calls
- `opencode-plugin-aft/src/tools/editing.ts` — modified: accept ToolContext, populate lsp_hints
- `opencode-plugin-aft/src/tools/reading.ts` — modified: accept ToolContext, populate lsp_hints for zoom
- `opencode-plugin-aft/src/tools/refactoring.ts` — modified: accept ToolContext, populate lsp_hints
- `opencode-plugin-aft/src/tools/navigation.ts` — modified: accept ToolContext (signature only, no LSP query needed for configure/call_tree)
- `opencode-plugin-aft/src/__tests__/lsp.test.ts` — new: mock client tests for LSP query logic
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — modified: adapt to ToolContext
