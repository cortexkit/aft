---
estimated_steps: 7
estimated_files: 6
---

# T01: Impact analysis command — callers with signature and entry point annotations

**Slice:** S04 — Data Flow Tracking + Impact Analysis
**Milestone:** M003

## Description

Implement the `impact` command (R025) — an enriched callers query that shows all call sites affected by a symbol change, annotated with the caller's signature, whether the caller is an entry point, and the source line at the call site. Includes per-language parameter extraction from signature text for the response and for T02's argument-to-parameter matching.

## Steps

1. Add response types to `src/callgraph.rs`: `ImpactCaller` (caller_symbol, caller_file, line, signature, is_entry_point, call_expression, parameters) and `ImpactResult` (symbol, file, signature, parameters, total_affected, affected_files, callers). Add `extract_parameters(signature, lang)` utility that parses parameter names from signature text — skip `self`/`&self`/`&mut self` for Rust, `self` for Python, handle TS/JS/Go normally. Split on commas, strip types/defaults.
2. Implement `impact()` method on `CallGraph`: call `callers_of` with depth, then for each `CallerSite` enrich with the caller's `SymbolMeta` (signature, kind, exported) and call `is_entry_point()` to set the entry point flag. Read the source line at the call site's line number for `call_expression`. Collect unique caller files for `affected_files` count.
3. Create `src/commands/impact.rs` with `handle_impact()` following the `trace_to.rs` handler pattern: extract `file`, `symbol`, `depth` (default 5) params, configure guard, symbol existence check (calls_by_symbol || exported_symbols || symbol_metadata), call `graph.impact()`, serialize.
4. Add `pub mod impact` to `src/commands/mod.rs`. Add `"impact"` dispatch entry in `src/main.rs`.
5. Add `aft_impact` tool definition in `opencode-plugin-aft/src/tools/navigation.ts` with Zod schema (file, symbol, depth optional).
6. Write unit tests for `extract_parameters` covering TS, Python (self skipped), Rust (&self/&mut self skipped), Go, edge cases (empty params, defaults, rest params).
7. Write integration tests in `tests/integration/callgraph_test.rs`: impact on `validate` (multi-caller), verify total_affected >= 2, verify entry point flags present, verify signature included, not_configured guard, symbol_not_found error.

## Must-Haves

- [ ] `ImpactResult` and `ImpactCaller` types with Serialize derive
- [ ] `impact()` method on CallGraph using `callers_of` with enrichment
- [ ] `extract_parameters()` with per-language receiver skipping
- [ ] `handle_impact` command handler with configure guard and symbol existence check
- [ ] Dispatch entry in main.rs
- [ ] `aft_impact` plugin tool with Zod schema
- [ ] Integration tests through binary protocol (impact query, not_configured, symbol_not_found)
- [ ] Unit tests for parameter extraction

## Verification

- `cargo test -- impact` — all impact-related tests pass
- `cargo test -- extract_parameters` — parameter extraction unit tests pass
- `bun test` — plugin tests pass (tool registration verified)
- `cargo test` — all 345+ existing tests still pass

## Observability Impact

- **New runtime signals:** `ImpactResult` includes `total_affected` (count of call sites) and `affected_files` (count of distinct files) — agents can gauge blast radius from these two numbers without parsing callers.
- **Inspection surface:** `impact` command returns structured JSON via binary protocol. Each `ImpactCaller` includes `is_entry_point`, `signature`, `call_expression`, and `parameters` — a future agent can grep for entry points to focus review.
- **Failure visibility:** Three structured error codes: `not_configured` (configure not called), `symbol_not_found` (bad symbol name or file), `invalid_request` (missing required params). All consistent with existing navigation commands.
- **How to inspect:** Send `{"command":"impact","file":"<path>","symbol":"<name>"}` via binary protocol. Check `total_affected` and `affected_files` for quick triage. Examine `callers[].is_entry_point` to find public API surfaces affected.

## Inputs

- `src/callgraph.rs` — CallGraph with callers_of(), is_entry_point(), SymbolMeta, CallerSite
- `src/commands/trace_to.rs` — handler pattern to replicate exactly
- `opencode-plugin-aft/src/tools/navigation.ts` — 4 existing tool definitions to extend
- `tests/fixtures/callgraph/` — existing fixtures (validate has callers from utils.ts, test_helpers.ts, service.ts)

## Expected Output

- `src/callgraph.rs` — ImpactResult, ImpactCaller, extract_parameters(), impact() method, unit tests
- `src/commands/impact.rs` — handle_impact command handler
- `src/commands/mod.rs` — added pub mod impact
- `src/main.rs` — added "impact" dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added aft_impact tool
- `tests/integration/callgraph_test.rs` — 3+ new integration tests
