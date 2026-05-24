---
id: T02
parent: S03
milestone: M004
provides:
  - ToolContext type bundling bridge + SDK client for all tool factories
  - queryLspHints function that queries OpenCode LSP for workspace symbols
  - lsp_hints population in bridge requests for edit_symbol, zoom, move_symbol, inline_symbol, extract_function
key_files:
  - opencode-plugin-aft/src/types.ts
  - opencode-plugin-aft/src/lsp.ts
  - opencode-plugin-aft/src/index.ts
key_decisions:
  - Derived client type from PluginInput["client"] rather than adding @opencode-ai/sdk as a direct dependency
  - Mock client in integration tests returns empty LSP server list (no connected servers) so queryLspHints is a no-op during binary round-trips
  - extract_function queries LSP for the extraction `name` parameter (the new function name) to help disambiguate collisions
patterns_established:
  - Tool factory signature: all 8 factories take (ctx: ToolContext) — pass ctx.bridge for bridge operations, ctx.client for SDK operations
  - LSP hint injection pattern: `const hints = await queryLspHints(ctx.client, symbolName as string); if (hints) params.lsp_hints = hints;` — inserted before bridge.send in symbol-resolving commands
  - Test mock pattern: createMockClient() returns { lsp: { status: async () => ({ data: [] }) }, find: { symbols: async () => ({ data: [] }) } }
observability_surfaces:
  - console.warn in queryLspHints on API error (visible in OpenCode plugin stderr)
  - queryLspHints failures return undefined silently — binary uses tree-sitter fallback
duration: 15min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Plugin-side LSP query and lsp_hints population

**Threaded OpenCode SDK client through tool factories, implemented LSP symbol querying, and populated lsp_hints in bridge requests for all 5 symbol-resolving commands.**

## What Happened

Created `ToolContext` type in `types.ts` bundling `BinaryBridge` + `PluginInput["client"]`. All 8 tool factory functions updated from `(bridge: BinaryBridge)` to `(ctx: ToolContext)` with internal references changed to `ctx.bridge.send(...)`.

Created `lsp.ts` with `queryLspHints(client, symbolName, directory?)` that:
1. Checks `client.lsp.status()` for any connected server
2. Calls `client.find.symbols()` with the symbol name query
3. Maps results to `{ symbols: [{ name, file, line, kind? }] }` wire format
4. Strips `file://` URI prefix, maps LSP SymbolKind numbers via `LSP_SYMBOL_KIND_MAP`
5. Returns `undefined` on any failure (no connected server, API error, empty results)

Injected LSP hint queries into 5 tool execute functions: `edit_symbol`, `zoom`, `move_symbol`, `inline_symbol`, `extract_function`. Each queries LSP before the bridge.send call and adds `lsp_hints` to params if results are non-undefined.

Updated `index.ts` to construct `ToolContext` from bridge + `input.client` and pass to all factories.

Created 13 mock client tests in `lsp.test.ts` covering connected/disconnected/error paths, URI prefix stripping, SymbolKind mapping, unknown kind omission, and directory parameter passthrough.

Updated `tools.test.ts` and `structure.test.ts` to use `createToolContext(bridge)` wrapper with mock client for existing integration tests.

## Verification

- `bun test` in `opencode-plugin-aft/`: **55 pass, 0 fail** (42 original + 13 new LSP tests)
- `npx tsc --noEmit`: clean, no TypeScript errors
- `cargo test lsp_hints`: 4 passed (binary-side tests from T01)
- `cargo test edit_symbol`: 9 passed (integration tests including lsp_hints disambiguation)
- `cargo test zoom`: 5 passed (integration tests including lsp_hints disambiguation)

### Slice verification status (T02 of 2):
- [x] `cargo test lsp_hints` — 4 passed
- [x] `cargo test edit_symbol` — 9 passed
- [x] `cargo test zoom` — 5 passed
- [x] `bun test` in `opencode-plugin-aft/` — 55 passed (includes LSP query, hint building, fallback, and round-trip tests)
- [x] Existing `cargo test` and `bun test` baselines pass without regression

## Diagnostics

- Plugin stderr shows `[aft-plugin] LSP query failed for "symbolName": error message` on API errors
- When no LSP server is connected, queryLspHints returns undefined silently — no stderr output
- Binary-side: grep stderr for `[aft] lsp_hints:` to see parsing outcomes when hints are present

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `opencode-plugin-aft/src/types.ts` — new: ToolContext interface definition
- `opencode-plugin-aft/src/lsp.ts` — new: queryLspHints function, LSP_SYMBOL_KIND_MAP, LspHints type
- `opencode-plugin-aft/src/index.ts` — modified: ToolContext construction, updated all factory calls
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
