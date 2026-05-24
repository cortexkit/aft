# S04: Data Flow Tracking + Impact Analysis

**Goal:** Complete M003 by shipping `aft_impact` and `aft_trace_data` — the final two call graph navigation commands.
**Demo:** Agent calls `aft_impact` on a function and sees all affected callers with signatures and entry point annotations. Agent calls `aft_trace_data` on an expression and sees variable renames through assignments and function parameters across files. Both proven by integration tests and plugin tool round-trips.

## Must-Haves

- `impact` command returns all callers (transitive, depth-limited) annotated with caller signature, call expression line, and entry point status
- `trace_data` command tracks an expression through direct assignments and function parameter passing, marking approximations when tracking breaks (destructuring, spread, unresolved calls)
- Both commands wired through binary protocol dispatch
- Both commands registered as plugin tools (`aft_impact`, `aft_trace_data`) with Zod schemas
- Integration tests prove both commands through the binary protocol using callgraph fixtures
- Per-language parameter extraction from signature text (handling self/&self/this receivers)
- `cargo test` passes with 0 failures, `bun test` passes with 0 failures

## Proof Level

- This slice proves: contract + integration (both commands through binary protocol and plugin tools)
- Real runtime required: no (binary protocol integration tests are sufficient)
- Human/UAT required: no

## Verification

- `cargo test -- impact`: integration tests prove impact on multi-caller symbol returns callers grouped with entry point flags, handles not_configured and symbol_not_found
- `cargo test -- trace_data`: integration tests prove data flow tracking through assignments and cross-file parameter passing, marks approximations
- `cargo test`: all existing + new tests pass with 0 failures
- `bun test`: all existing + new tests pass with 0 failures

## Observability / Diagnostics

- Runtime signals: `ImpactResult` includes `total_affected` and `affected_files` counts; `TraceDataResult` includes `hops` list with `flow_type` per hop and `approximate` flag when tracking breaks
- Inspection surfaces: both commands return structured JSON with diagnostic metadata via binary protocol
- Failure visibility: structured error codes (`not_configured`, `symbol_not_found`, `invalid_request`) consistent with existing navigation commands
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: `CallGraph` (forward_tree, callers_of, trace_to, build_file, resolve_cross_file_edge, is_entry_point, symbol_metadata), `calls.rs` (extract_calls_full), existing fixtures
- New wiring introduced in this slice: 2 dispatch entries in main.rs, 2 command handler modules, 2 graph methods on CallGraph, 2 plugin tools in navigation.ts
- What remains before the milestone is truly usable end-to-end: nothing — S04 is terminal

## Tasks

- [x] **T01: Impact analysis command — callers with signature and entry point annotations** `est:1h`
  - Why: R025 — agents need to know what breaks when a function signature changes. Impact is an enriched callers query that adds signature context and entry point classification per caller.
  - Files: `src/callgraph.rs`, `src/commands/impact.rs`, `src/commands/mod.rs`, `src/main.rs`, `opencode-plugin-aft/src/tools/navigation.ts`, `tests/integration/callgraph_test.rs`
  - Do: Add `ImpactResult`/`ImpactCaller` response types to callgraph.rs. Implement `impact()` method on CallGraph that calls `callers_of` transitively, then enriches each caller with its signature from `symbol_metadata` and entry point status from `is_entry_point()`. Extracts the source line at the call site for context. Create `handle_impact` command handler following trace_to.rs pattern exactly. Wire dispatch in main.rs. Add `aft_impact` plugin tool in navigation.ts. Write integration tests covering: impact on multi-caller symbol, not_configured guard, symbol_not_found error, entry point annotations present. Per-language parameter extraction from signatures (skip self/&self/&mut self/this).
  - Verify: `cargo test -- impact` passes, `bun test` passes
  - Done when: `impact` command returns callers with signatures, entry point flags, and call site context through binary protocol and plugin tool

- [x] **T02: Data flow tracking command — trace expressions through assignments and parameters** `est:1.5h`
  - Why: R024 — agents need to follow a value through the code to understand how data transforms across function boundaries. trace_data walks assignment chains and matches argument positions to parameter names across calls.
  - Files: `src/callgraph.rs`, `src/commands/trace_data.rs`, `src/commands/mod.rs`, `src/main.rs`, `opencode-plugin-aft/src/tools/navigation.ts`, `tests/integration/callgraph_test.rs`, `tests/fixtures/callgraph/data_flow.ts`, `tests/fixtures/callgraph/data_processor.ts`
  - Do: Add `DataFlowHop`/`TraceDataResult` response types. Implement `trace_data()` on CallGraph: parse function body AST to find the target expression, walk variable_declarator and assignment_expression nodes to track renames, when the tracked expression flows into a call argument resolve the callee and match argument position to parameter name (using per-language signature parsing from T01), recurse into the callee's body tracking the parameter name. Mark `approximate: true` when hitting destructuring, spread, or unresolved calls — stop tracking through those. Depth-limit cross-file hops (default 5). Create `handle_trace_data` handler, wire dispatch, add `aft_trace_data` plugin tool. Create new fixture files with assignment chains for testing. Write integration tests covering: expression tracked through local assignment, expression tracked across file boundary via function parameter, approximation marker on unresolved tracking, not_configured guard, symbol_not_found error.
  - Verify: `cargo test -- trace_data` passes, `bun test` passes, `cargo test` all pass with 0 failures
  - Done when: `trace_data` command returns data flow hops with variable names, flow types, and approximation markers through binary protocol and plugin tool; all M003 tests pass

## Files Likely Touched

- `src/callgraph.rs`
- `src/commands/impact.rs`
- `src/commands/trace_data.rs`
- `src/commands/mod.rs`
- `src/main.rs`
- `opencode-plugin-aft/src/tools/navigation.ts`
- `tests/integration/callgraph_test.rs`
- `tests/fixtures/callgraph/data_flow.ts`
- `tests/fixtures/callgraph/data_processor.ts`
