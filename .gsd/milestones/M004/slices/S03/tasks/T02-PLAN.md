---
estimated_steps: 7
estimated_files: 10
---

# T02: Plugin-side LSP query and lsp_hints population

**Slice:** S03 — LSP-Enhanced Symbol Resolution
**Milestone:** M004

## Description

Thread the OpenCode SDK client through to plugin tool factories, implement LSP querying logic that checks server availability and fetches workspace symbols, and populate the `lsp_hints` field in bridge requests for all commands that perform symbol resolution. Add dedicated tests for the LSP query module with a mock client, and update existing tests to work with the refactored factory signatures.

## Steps

1. Create `opencode-plugin-aft/src/types.ts` with `ToolContext` type: `{ bridge: BinaryBridge, client: OpencodeClient }` (import `OpencodeClient` type from SDK). This bundles the two dependencies tool factories need.
2. Create `opencode-plugin-aft/src/lsp.ts` with:
   - `LSP_SYMBOL_KIND_MAP`: maps LSP SymbolKind numbers to AFT kind strings (12→"function", 5→"class", 6→"method", 23→"struct", 11→"interface", 10→"enum")
   - `queryLspHints(client, symbolName, directory?)`: checks `client.lsp.status()` → if any server has `status: "connected"`, calls `client.find.symbols({ query: { query: symbolName, directory } })` → maps result to `{ symbols: [{ name, file, line, kind }] }` wire format (strip `file://` prefix from URIs, map kind numbers). Returns `undefined` if no LSP server connected, API errors, or empty results. Entire function wrapped in try/catch — any failure returns `undefined`.
3. Update all tool factory signatures from `(bridge: BinaryBridge)` to `(ctx: ToolContext)`: `editingTools`, `readingTools`, `refactoringTools`, `navigationTools`, `safetyTools`, `importTools`, `structureTools`, `transactionTools`. Inside each factory, replace `bridge.send(...)` references with `ctx.bridge.send(...)`.
4. Update `opencode-plugin-aft/src/index.ts`: construct `ToolContext` from `{ bridge, client: input.client }`, pass to all tool factories.
5. In tool execute functions for `aft_edit_symbol`, `aft_zoom`, `aft_move_symbol`, `aft_inline_symbol`, `aft_extract_function`: before calling `ctx.bridge.send()`, extract the symbol name from params, call `queryLspHints(ctx.client, symbolName)`, and if result is non-undefined, add `lsp_hints` to the params object.
6. Create `opencode-plugin-aft/src/__tests__/lsp.test.ts` with mock client tests:
   - Connected server + symbols found → returns formatted hints
   - No connected server → returns undefined
   - API throws error → returns undefined
   - File URI prefix stripping works correctly
   - SymbolKind mapping covers known kinds, unknown kinds omitted
7. Update `opencode-plugin-aft/src/__tests__/tools.test.ts` to work with new `ToolContext` signature — construct mock context with bridge + null/mock client for existing tests.

## Must-Haves

- [ ] `ToolContext` type defined and used by all 8 tool factory functions
- [ ] `queryLspHints` checks `lsp.status()` before querying symbols
- [ ] `queryLspHints` returns `undefined` (not throws) on any failure
- [ ] `file://` prefix stripped from LSP symbol URIs
- [ ] LSP SymbolKind numbers mapped to AFT kind strings
- [ ] `lsp_hints` populated in bridge params for edit_symbol, zoom, move_symbol, inline_symbol, extract_function
- [ ] Mock client tests prove connected, disconnected, and error paths
- [ ] Existing bun tests pass without regression after factory signature change
- [ ] `bun test` passes (baseline 42 + new tests)

## Verification

- `bun test` in `opencode-plugin-aft/` — all tests pass (existing + new LSP tests)
- No TypeScript compilation errors (`bunx tsc --noEmit` or build step)

## Observability Impact

- Signals added: console.warn in `queryLspHints` on API error (visible in OpenCode's plugin log)
- How a future agent inspects this: check plugin stderr/console for LSP query warnings
- Failure state exposed: `queryLspHints` failures are silent at the tool level (returns undefined → no lsp_hints in request → binary uses tree-sitter fallback)

## Inputs

- T01 — `LspHints` wire format contract: `{ symbols: [{ name: string, file: string, line: number, kind?: string }] }`
- `opencode-plugin-aft/src/bridge.ts` — `BinaryBridge.send(command, params)` spreads params into JSON envelope
- `opencode-plugin-aft/src/index.ts` — current factory wiring pattern
- `opencode-plugin-aft/src/tools/*.ts` — current tool factory signatures
- OpenCode SDK types: `Symbol { name, kind: number, location: { uri, range } }`, `LspStatus { id, name, root, status }`

## Expected Output

- `opencode-plugin-aft/src/types.ts` — new: ToolContext type definition
- `opencode-plugin-aft/src/lsp.ts` — new: queryLspHints function, SymbolKind map
- `opencode-plugin-aft/src/index.ts` — modified: ToolContext construction, updated factory calls
- `opencode-plugin-aft/src/tools/editing.ts` — modified: ToolContext param, lsp_hints population for edit_symbol
- `opencode-plugin-aft/src/tools/reading.ts` — modified: ToolContext param, lsp_hints population for zoom
- `opencode-plugin-aft/src/tools/refactoring.ts` — modified: ToolContext param, lsp_hints population for move_symbol, extract_function, inline_symbol
- `opencode-plugin-aft/src/tools/navigation.ts` — modified: ToolContext param (signature only)
- `opencode-plugin-aft/src/__tests__/lsp.test.ts` — new: 5+ mock client tests
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — modified: adapted to ToolContext
