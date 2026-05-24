---
id: S03
parent: M004
milestone: M004
provides:
  - LspHints struct with defensive parsing and apply_lsp_disambiguation in binary
  - LSP disambiguation wired into edit_symbol, zoom, move_symbol, inline_symbol handlers
  - ToolContext type bundling bridge + SDK client for all plugin tool factories
  - queryLspHints function querying OpenCode LSP for workspace symbols
  - lsp_hints population in bridge params for edit_symbol, zoom, move_symbol, inline_symbol, extract_function
requires:
  - slice: S01
    provides: move_symbol command handler with disambiguation path
  - slice: S02
    provides: extract_function and inline_symbol command handlers with disambiguation paths
affects: []
key_files:
  - src/lsp_hints.rs
  - src/commands/edit_symbol.rs
  - src/commands/zoom.rs
  - src/commands/move_symbol.rs
  - src/commands/inline_symbol.rs
  - opencode-plugin-aft/src/types.ts
  - opencode-plugin-aft/src/lsp.ts
  - opencode-plugin-aft/src/index.ts
  - tests/integration/lsp_hints_test.rs
key_decisions:
  - D116: LSP hints consumed at handler level between scope filter and ambiguous_symbol response, not in LanguageProvider trait
  - D117: ToolContext bundles bridge + client for all plugin factories — single extensible context object
  - D118: LSP disambiguation matches on name + file path suffix + line range for positional specificity
patterns_established:
  - Binary lsp_hints pattern: parse_lsp_hints(req) → if Some, apply_lsp_disambiguation(matches, &hints). Insert between scope filter and ambiguity check.
  - Plugin LSP hint injection: queryLspHints(ctx.client, symbolName) → if result, add lsp_hints to params before bridge.send
  - Plugin tool factory signature: all 8 factories take (ctx: ToolContext) — ctx.bridge for binary operations, ctx.client for SDK operations
observability_surfaces:
  - stderr "[aft] lsp_hints: parsed N symbol hints" when valid hints present
  - stderr "[aft] lsp_hints: ignoring malformed data: {error}" on parse failure
  - Plugin console.warn on queryLspHints API errors
drill_down_paths:
  - tasks/T01-SUMMARY.md
  - tasks/T02-SUMMARY.md
duration: 40m
verification_result: passed
completed_at: 2026-03-14
---

# S03: LSP-Enhanced Symbol Resolution

**Binary consumes lsp_hints from request JSON for symbol disambiguation; plugin queries OpenCode SDK for workspace symbols and populates hints for all symbol-resolving commands. 463 Rust tests + 55 plugin tests pass.**

## What Happened

**T01 (Binary-side):** Created `src/lsp_hints.rs` with `LspHints` / `LspSymbolHint` structs, `parse_lsp_hints()` for defensive deserialization from `req.lsp_hints`, and `apply_lsp_disambiguation()` that filters tree-sitter matches by file path suffix + line range alignment with LSP hints. Returns the single matching candidate when exactly one aligns; returns all original matches unchanged otherwise (graceful fallback). Path helpers handle `file://` URI stripping and suffix-based comparison for absolute vs relative paths. Wired into all 4 disambiguation paths: edit_symbol (after scope filter), zoom (after resolve_symbol), move_symbol (after scope filter), inline_symbol (before function/method filter). 13 unit tests cover parsing, disambiguation logic, and path matching. 4 integration tests prove protocol-level behavior: hints resolve ambiguous symbol, absence returns candidates, malformed hints trigger fallback with stderr warning, zoom works with hints.

**T02 (Plugin-side):** Created `ToolContext` type bundling `BinaryBridge` + SDK `client`. Refactored all 8 tool factory functions from `(bridge: BinaryBridge)` to `(ctx: ToolContext)`. Created `lsp.ts` with `queryLspHints()` that checks `lsp.status()` for connected servers, calls `find.symbols()`, maps LSP `SymbolKind` numbers to AFT kind strings via `LSP_SYMBOL_KIND_MAP`, strips `file://` URI prefixes. Returns `undefined` on any failure (no server, API error, empty results). Injected LSP hint queries into 5 tool execute functions: edit_symbol, zoom, move_symbol, inline_symbol, extract_function — each queries before bridge.send and adds `lsp_hints` to params if non-undefined. 13 new mock client tests cover connected/disconnected/error paths, URI handling, and kind mapping.

## Verification

- `cargo test lsp_hints` — 14 unit tests pass (13 in lsp_hints + 1 protocol)
- `cargo test lsp_hints_test` — 4 integration tests pass
- `cargo test edit_symbol` — 9 passed (includes lsp_hints disambiguation cases)
- `cargo test zoom` — 5 passed (includes lsp_hints disambiguation case)
- `cargo test` — full suite: 293 unit + 170 integration = 463 total, 0 failures
- `bun test` in `opencode-plugin-aft/` — 55 pass (42 baseline + 13 new LSP tests), 0 failures
- `npx tsc --noEmit` — clean
- Observability: stderr logging confirmed for valid/malformed hints

## Requirements Advanced

- R031 — LSP-aware architecture fully validated: lsp_hints parsed and consumed in 4 handlers, plugin populates hints for 5 commands
- R033 — LSP integration via plugin mediation fully validated: plugin queries lsp.status() → find.symbols() → maps to wire format → binary disambiguates

## Requirements Validated

- R031 — Provider interface + lsp_hints consumption proven end-to-end through integration tests with mock LSP data and plugin round-trips
- R033 — Plugin→binary LSP data flow proven: 13 mock client tests + 4 binary protocol tests + 55 total plugin tests

## New Requirements Surfaced

None.

## Requirements Invalidated or Re-scoped

None.

## Deviations

None.

## Known Limitations

- LSP integration scoped to workspace symbol verification only (D104) — no go-to-definition, find-references, or type information from the SDK
- Kind matching between LSP SymbolKind and AFT SymbolKind is best-effort mapping — unknown kinds are omitted rather than forcing a match
- Plugin tests use mock clients, not real OpenCode LSP servers — production behavior depends on SDK's actual lsp.status()/find.symbols() response shapes

## Follow-ups

None — this is the terminal slice for M004.

## Files Created/Modified

- `src/lsp_hints.rs` — new: LspHints/LspSymbolHint structs, parse/disambiguate functions, path helpers, 13 unit tests
- `src/lib.rs` — modified: added `pub mod lsp_hints`
- `src/commands/edit_symbol.rs` — modified: lsp_hints disambiguation block
- `src/commands/zoom.rs` — modified: lsp_hints disambiguation block
- `src/commands/move_symbol.rs` — modified: lsp_hints disambiguation block
- `src/commands/inline_symbol.rs` — modified: lsp_hints disambiguation block
- `tests/integration/lsp_hints_test.rs` — new: 4 integration tests
- `tests/integration/main.rs` — modified: added `mod lsp_hints_test`
- `opencode-plugin-aft/src/types.ts` — new: ToolContext type
- `opencode-plugin-aft/src/lsp.ts` — new: queryLspHints, LSP_SYMBOL_KIND_MAP
- `opencode-plugin-aft/src/index.ts` — modified: ToolContext construction, updated factory calls
- `opencode-plugin-aft/src/tools/editing.ts` — modified: ToolContext param, lsp_hints for edit_symbol
- `opencode-plugin-aft/src/tools/reading.ts` — modified: ToolContext param, lsp_hints for zoom
- `opencode-plugin-aft/src/tools/refactoring.ts` — modified: ToolContext param, lsp_hints for move_symbol, extract_function, inline_symbol
- `opencode-plugin-aft/src/tools/navigation.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/tools/safety.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/tools/imports.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/tools/structure.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/tools/transaction.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/__tests__/lsp.test.ts` — new: 13 mock client tests
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — modified: adapted to ToolContext
- `opencode-plugin-aft/src/__tests__/structure.test.ts` — modified: adapted to ToolContext

## Forward Intelligence

### What the next slice should know
- M004 is complete — no next slice in this milestone
- 463 Rust tests + 55 plugin tests is the new baseline
- The `ToolContext` pattern is established for all plugin tool factories — any new tool should follow it

### What's fragile
- LSP kind mapping (`LSP_SYMBOL_KIND_MAP`) covers common kinds (function, class, method, module, etc.) but unknown kinds are silently omitted — new LSP symbol kinds would need explicit mapping
- Path suffix matching in `apply_lsp_disambiguation` handles absolute vs relative path differences but could false-match on suffixes if two files share the same filename in different directories

### Authoritative diagnostics
- `cargo test lsp_hints` — fastest check for binary-side LSP hint parsing and disambiguation
- `bun test` in `opencode-plugin-aft/` — covers plugin LSP query logic and all tool round-trips
- Grep stderr for `[aft] lsp_hints:` to observe hint parsing outcomes at runtime

### What assumptions changed
- None — SDK surface matched research expectations (find.symbols + lsp.status only)
