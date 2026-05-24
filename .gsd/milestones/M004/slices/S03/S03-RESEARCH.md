# S03: LSP-Enhanced Symbol Resolution — Research

**Date:** 2026-03-14

## Summary

S03 wires LSP workspace symbol data from OpenCode's SDK through the plugin into the binary's existing `lsp_hints` field, enabling disambiguation of symbols that tree-sitter alone can't resolve. The integration surface is narrow but well-defined: the plugin calls `client.find.symbols({ query })` which returns `Array<{ name, kind, location: { uri, range } }>`, and `client.lsp.status()` which returns `Array<{ id, name, root, status }>`. This data flows through the existing `lsp_hints` JSON field in `RawRequest` (wired since M001, D003) to the binary, where command handlers use it as a secondary resolution source when tree-sitter produces ambiguous results.

The implementation has three clear layers: (1) binary-side — define the `LspHints` struct, parse it from `req.lsp_hints`, and use it in the disambiguation paths of `edit_symbol`, `zoom`, `move_symbol`, and `inline_symbol`; (2) plugin-side — query OpenCode's `find.symbols()` API before sending commands that involve symbol resolution, and populate the `lsp_hints` field; (3) testing — integration tests with mock `lsp_hints` JSON in binary protocol tests (binary doesn't need a real LSP server), plus plugin round-trip tests with mocked SDK client. The binary-side work is straightforward — `lsp_hints` is already deserialized from JSON, just never consumed. The plugin-side work requires threading the `input.client` through to tool execute functions, which currently only receive the `BinaryBridge`.

The accuracy improvement is real but modest. LSP workspace symbol search returns symbols with their file URI and range, which allows confirming "this `process` is the one at line 3 in `service.ts`, not the one at line 8 in `handler.ts`." Tree-sitter already provides file+line+kind for disambiguation, but the key value is cross-file: when an agent says `edit_symbol process` without specifying a file, LSP hints can identify which file's `process` the agent likely means based on workspace context. However, this only works when the plugin proactively queries LSP before sending the command — it's not retroactive.

## Recommendation

Structure as three tasks:

**T01: Binary-side lsp_hints consumption.** Define `LspHints` struct (symbol candidates with file/line/kind from LSP). Parse from `req.lsp_hints` in the four commands that use `resolve_symbol` (edit_symbol, zoom, move_symbol, inline_symbol). When `lsp_hints` contains a matching symbol with a specific file+line, use it to disambiguate — select the tree-sitter match whose file+line aligns with the LSP hint. If no LSP hints present, behavior is unchanged (pure tree-sitter fallback). Add unit tests for the disambiguation logic and integration tests sending `lsp_hints` JSON through the protocol.

**T02: Plugin-side LSP query and lsp_hints population.** Thread `input.client` through to tool factories so execute functions can query `client.find.symbols()` and `client.lsp.status()`. Before sending symbol-resolution commands, query LSP for matching symbols and populate the `lsp_hints` field in the bridge request. Handle the case where LSP is unavailable (no connected server per `lsp.status()`) by sending no hints. Add bun tests with a mock client.

**T03: End-to-end verification and fallback testing.** Integration test proving: (a) with lsp_hints populated, an ambiguous symbol resolves correctly without returning candidates; (b) without lsp_hints, behavior is unchanged (candidates returned as before); (c) with invalid/stale lsp_hints, graceful fallback to tree-sitter. Plugin round-trip test proving LSP query flows through the bridge.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Symbol resolution with disambiguation | `ctx.provider().resolve_symbol()` + scope filter in edit_symbol.rs | All four command handlers already have the disambiguation path (filter by scope). LSP hints add a second filter, not a new resolution path. |
| JSON deserialization of lsp_hints | `serde_json::from_value::<LspHints>()` on `req.lsp_hints` | Already `Option<serde_json::Value>` in RawRequest. Deserialize to a typed struct only when present. |
| LSP workspace symbol query | `client.find.symbols({ query })` from OpenCode SDK | SDK already handles LSP protocol, server management, and error handling. Plugin just calls the API. |
| LSP server availability check | `client.lsp.status()` from OpenCode SDK | Returns array of connected servers with status. Check before querying symbols to avoid unnecessary API calls. |
| Bridge request JSON construction | `BinaryBridge.send(command, params)` | `lsp_hints` is just another field in the params object — `{ ...params, lsp_hints: { ... } }`. No bridge modification needed. |

## Existing Code and Patterns

### Binary Side — Where LSP Hints Are Consumed

- `src/protocol.rs` — `RawRequest.lsp_hints: Option<serde_json::Value>` already deserialized from JSON. Never consumed by any handler. This is the entry point.
- `src/commands/edit_symbol.rs:93-149` — `resolve_symbol` → filter by scope → disambiguation response. LSP hints would add another filter stage: if `lsp_hints` specifies a file+line, select the matching candidate before falling back to scope filter or returning candidates.
- `src/commands/zoom.rs:79-105` — Same pattern: resolve_symbol → disambiguation. Same enhancement path.
- `src/commands/move_symbol.rs:116-135` — resolve_symbol → disambiguation. Additionally, move_symbol already requires the file param, so LSP is less useful here (file is known). Still worth enhancing for consistency.
- `src/commands/inline_symbol.rs:140-160` — resolve_symbol → disambiguation. Same pattern.
- `src/symbols.rs` — `SymbolMatch { symbol: Symbol, file: String }` — the match type already includes file path, which is what LSP hints match against.

### Plugin Side — Where LSP Hints Originate

- `opencode-plugin-aft/src/index.ts:30-46` — Plugin function receives `input: PluginInput` with `input.client` (SDK client). Currently only passes `input.directory` to `BinaryBridge`. Need to thread `input.client` to tool factories.
- `opencode-plugin-aft/src/bridge.ts:57-94` — `BinaryBridge.send(command, params)` builds JSON as `{ id, command, ...params }`. Adding `lsp_hints` to params means it automatically flows into the JSON envelope and gets deserialized as `req.lsp_hints` on the binary side. No protocol change needed.
- `opencode-plugin-aft/src/tools/refactoring.ts` — Tool execute functions build params and call `bridge.send()`. These need access to `client` to query LSP before sending. Pattern: check `lsp.status()` → if active, call `find.symbols({ query: symbolName })` → include results as `lsp_hints` in params.
- `opencode-plugin-aft/src/tools/editing.ts` — `editingTools(bridge)` — `aft_edit_symbol` is the primary target for LSP enhancement. Same threading pattern needed.
- `opencode-plugin-aft/src/tools/reading.ts` — `readingTools(bridge)` — `aft_zoom` is another target. Same pattern.

### SDK Types

- `Symbol` — `{ name: string, kind: number, location: { uri: string, range: Range } }`. `kind` is LSP SymbolKind (1=File, 2=Module, 5=Class, 6=Method, 12=Function, etc.). `uri` is file URI (e.g., `file:///path/to/file.ts`). `range` has `start: { line, character }` and `end: { line, character }`.
- `LspStatus` — `{ id: string, name: string, root: string, status: "connected" | "error" }`. Use to check if any LSP server is active before querying symbols.
- `FindSymbolsData` — `{ query: { query: string, directory?: string } }`. The query is a string pattern (symbol name).

## Constraints

- **Binary never connects to LSP directly** (D002, hard constraint). All LSP data comes through `lsp_hints` JSON field. Binary parses it but never makes network calls.
- **lsp_hints is `Option<serde_json::Value>`** — must deserialize defensively. Malformed hints should be ignored (warn on stderr), not crash the handler.
- **Plugin tool factories currently receive only `BinaryBridge`** — threading `client` requires changing all factory signatures from `(bridge: BinaryBridge)` to `(bridge: BinaryBridge, client: Client)` or similar. This touches every tool group file's type signature but not its logic.
- **OpenCode SDK calls are async and may fail** — LSP server might not be connected, `find.symbols()` might return empty, or the API might timeout. All paths must have clean fallback to "no hints" (i.e., existing tree-sitter behavior).
- **Single-threaded binary with RefCell** (D001, D014, D029). LSP hint consumption is pure data parsing — no concurrency concerns.
- **NDJSON protocol** (D009). lsp_hints flows as part of the existing request JSON — no protocol changes needed.
- **OpenCode SDK `find.symbols()` returns LSP SymbolKind numbers**, not strings. Need mapping: 5=Class, 6=Method, 12=Function, 23=Struct, 11=Interface, 10=Enum, 26=TypeParameter. Map to AFT's `SymbolKind` enum for comparison.
- **Plugin tests can't call real OpenCode SDK** — bun tests don't run inside an OpenCode session. Need to mock the client or test the hint-building logic in isolation.

## Common Pitfalls

- **Over-engineering the LanguageProvider trait** — Tempting to create an `LspEnhancedProvider` that wraps `TreeSitterProvider`. Don't. LSP hints are request-scoped data (they come with each command invocation), not a persistent state. Injecting them into the provider would require threading request data through the trait, violating its per-file interface. Instead, consume hints directly in command handlers at the disambiguation step.

- **Assuming LSP symbols match tree-sitter symbols 1:1** — LSP SymbolKind numbers don't map cleanly to AFT's SymbolKind. LSP has ~26 kinds; AFT has 7. A `const` declaration might be LSP kind 13 (Variable) but AFT doesn't have a Variable kind. Match on name + file + line range, not on kind.

- **URI vs path mismatch** — LSP returns `file:///path/to/file.ts` URIs. Binary works with filesystem paths. Must strip the `file://` prefix and handle URL encoding (%20, etc.) when comparing. Also macOS canonicalization (D111) applies.

- **Querying LSP for every command** — Calling `find.symbols()` on every `edit_symbol` invocation adds latency. Only query when the symbol name is likely ambiguous (short/common names) or when a previous call returned ambiguous. Better: always query but don't block — if LSP is slow, proceed without hints. However, since the tool execute function is async and must await bridge.send() anyway, one additional await for find.symbols() is acceptable latency-wise.

- **Plugin factory signature change ripple** — Changing all `*Tools(bridge)` to `*Tools(bridge, client)` touches every tool group. Minimize by passing `client` only to tool groups that need it (editing, reading, refactoring, navigation) or by creating a context object `{ bridge, client }` passed everywhere.

- **Mock client in bun tests** — The existing bun tests don't have an OpenCode client. Need a mock that implements `find.symbols()` and `lsp.status()` returning predetermined data. The mock must match the SDK's return type shape.

## Open Risks

- **OpenCode SDK client not available in test/CI** — bun tests run without an OpenCode session. Plugin code that calls `client.find.symbols()` can only be tested with mocks. The mock might not match the real API's error handling behavior. Mitigate by keeping the LSP path fully optional and testing the no-LSP fallback as the primary path.

- **LSP symbol data staleness** — The plugin queries `find.symbols()` at command invocation time. If the user has just created a new symbol that the language server hasn't indexed yet, the hints could be stale or missing. The binary should treat missing LSP data as "no hint" rather than "symbol doesn't exist."

- **SDK API stability** — `find.symbols()` is part of the public SDK but OpenCode is actively developed. If the API changes signature or return format, the LSP integration breaks. Mitigate by keeping the dependency surface minimal (one query type, defensive parsing) and the fallback path solid.

- **Plugin factory signature change** — Threading `client` to 4+ tool factory functions is a non-trivial refactor. If done poorly, it could break existing tests. The change is mechanical but touches many files. Consider a `ToolContext` type to bundle bridge + client.

## Candidate Requirements (Advisory)

1. **Define `lsp_hints` wire format** — Currently `Option<serde_json::Value>` with no schema. S03 should define a concrete `LspHints` struct that both plugin and binary agree on. This becomes the contract.

2. **LSP query opt-out** — Consider a plugin-level config to disable LSP queries for performance. Not essential for S03 but worth noting.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| LSP | `rysweet/amplihack@lsp-setup` | available (77 installs — general LSP setup, not relevant to plugin SDK integration) |
| LSP | `anton-abyzov/specweave@lsp-integration` | available (14 installs — too generic) |
| tree-sitter | `plurigrid/asi@tree-sitter` | available (7 installs — low adoption) |

No directly relevant professional skills found. The work is specific to OpenCode SDK integration and the existing AFT codebase patterns.

## Sources

- OpenCode SDK `sdk.gen.d.ts` — `Find` class has `symbols({ query })` returning `Array<Symbol>`, `files({ path })`, `text({ pattern })`. `Lsp` class has `status()` returning `Array<LspStatus>`.
- OpenCode SDK `types.gen.d.ts` — `Symbol = { name, kind: number, location: { uri, range } }`, `LspStatus = { id, name, root, status: "connected" | "error" }`, `Range = { start: { line, character }, end: { line, character } }`.
- Plugin types `@opencode-ai/plugin/dist/index.d.ts` — `PluginInput = { client, project, directory, worktree, serverUrl, $ }`. `client` is `ReturnType<typeof createOpencodeClient>` with `find`, `lsp` properties.
- Binary `src/protocol.rs` — `RawRequest.lsp_hints: Option<serde_json::Value>` already wired since M001 (D003).
- Binary disambiguation paths: `edit_symbol.rs:100-149`, `zoom.rs:79-105`, `move_symbol.rs:116-135`, `inline_symbol.rs:140-160` — all follow the same resolve → filter → disambiguate pattern.
- Existing test baselines: 446 Rust tests (280 unit + 166 integration), 42 bun tests.
