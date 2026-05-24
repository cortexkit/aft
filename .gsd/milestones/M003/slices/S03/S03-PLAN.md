# S03: Trace to Entry Points

**Goal:** `trace_to` command traces backward from any symbol to all entry points (exported functions, main/init, test functions), rendered top-down with complete paths.
**Demo:** Agent calls `aft_trace_to` on `checkFormat` (deeply nested) and receives paths like `main → processData → validate → checkFormat`, proven by integration tests with multi-layer, multi-path call chains.

## Must-Haves

- `is_entry_point(name, kind, exported, lang)` pure function detecting exported functions, main/init patterns, and test patterns per language
- `SymbolInfo` extended with `kind: SymbolKind` and `FileCallData` extended with per-symbol metadata so entry point detection doesn't re-parse
- `trace_to(file, symbol, max_depth)` backward path traversal using the existing reverse index, collecting complete paths and reversing for top-down rendering
- `TraceHop`, `TracePath`, `TraceToResult` response types with Serialize
- `handle_trace_to` command handler following the configure-then-use guard pattern
- `trace_to` dispatch entry in `main.rs`
- `aft_trace_to` plugin tool with Zod schema
- Integration tests proving: single-path trace, multi-path trace (utility called from 2+ entry points), no-entry-point graceful handling, depth limiting, cycle detection, not_configured guard
- Multi-path test fixture files in `tests/fixtures/callgraph/`

## Proof Level

- This slice proves: contract + integration
- Real runtime required: yes (binary protocol round-trips)
- Human/UAT required: no

## Verification

- `cargo test -- callgraph`: all existing + new trace_to unit tests pass (0 failures)
- `cargo test -- trace_to`: integration tests for trace_to through binary protocol
- `cargo test`: full suite 0 failures (existing 326+ new)
- `bun test`: plugin tests 0 failures (existing 39+ new)
- Integration test: `trace_to` on `checkFormat` returns path through `main → processData → validate → checkFormat`
- Integration test: `trace_to` on a utility called from two exported functions returns 2 distinct paths

## Observability / Diagnostics

- Runtime signals: `trace_to` response includes `total_paths`, `entry_points_found`, `max_depth_reached` — agents know when results might be incomplete
- Inspection surfaces: `trace_to` response includes `truncated_paths` count for dead-end paths that never reached an entry point
- Failure visibility: `not_configured` and `symbol_not_found` structured error codes (same as callers/call_tree)
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: `CallGraph::build_reverse_index()`, `callers_of()` reverse index, `walk_project_files()`, `build_file_data()` — all from S01/S02
- New wiring introduced in this slice: `trace_to` dispatch entry, `aft_trace_to` plugin tool
- What remains before the milestone is truly usable end-to-end: S04 (trace_data + impact) — two more commands building on this infrastructure

## Tasks

- [x] **T01: Implement entry point detection and backward path traversal** `est:45m`
  - Why: Core algorithm — extends callgraph data model with symbol kind metadata, implements `is_entry_point()` heuristics, and builds the `trace_to()` backward traversal that collects complete paths from entry points to target
  - Files: `src/callgraph.rs`, `src/symbols.rs`, `tests/fixtures/callgraph/` (new fixture files)
  - Do: Add `kind` field to `SymbolInfo`, add `symbol_metadata` map to `FileCallData`, implement `is_entry_point()` as pure function (exported functions, main/init/setup/run patterns, test patterns per language), implement `trace_to()` BFS with path tracking using reverse index, add `TraceHop`/`TracePath`/`TraceToResult` types, create multi-path fixture files (a utility called from 2 exported functions + a test), write unit tests. All path lookups must go through `canonicalize()` — S02 forward intelligence warns about this.
  - Verify: `cargo test -- callgraph` passes with new trace_to and is_entry_point unit tests
  - Done when: `trace_to()` returns correct top-down paths from entry points to target in unit tests, `is_entry_point()` correctly classifies exported functions, main patterns, and test patterns

- [x] **T02: Wire trace_to command, dispatch, plugin tool, and integration tests** `est:30m`
  - Why: Completes the binary protocol path and plugin integration — proves `trace_to` works end-to-end through the same protocol stack agents use
  - Files: `src/commands/trace_to.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/integration/callgraph_test.rs`, `opencode-plugin-aft/src/tools/navigation.ts`
  - Do: Create `handle_trace_to` command handler (extract file/symbol/depth params, configure-then-use guard, call `graph.trace_to()`, serialize result), add `pub mod trace_to` to mod.rs, add `"trace_to"` dispatch entry in main.rs, add `aft_trace_to` tool definition in navigation.ts with Zod schema, write integration tests: single-path trace through protocol, multi-path trace, not_configured error, symbol_not_found error, depth limiting
  - Verify: `cargo test -- trace_to` and `cargo test` pass with 0 failures; `bun test` passes with 0 failures
  - Done when: Integration tests prove trace_to returns correct multi-file paths through binary protocol, plugin tool registered and schema-valid

## Files Likely Touched

- `src/callgraph.rs`
- `src/commands/trace_to.rs` (new)
- `src/commands/mod.rs`
- `src/main.rs`
- `tests/integration/callgraph_test.rs`
- `tests/fixtures/callgraph/` (new multi-path fixture files)
- `opencode-plugin-aft/src/tools/navigation.ts`
