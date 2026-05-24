---
id: T02
parent: S04
milestone: M003
provides:
  - trace_data command tracking expressions through assignments and cross-file parameter matching
  - DataFlowHop and TraceDataResult response types
  - aft_trace_data plugin tool with Zod schema
key_files:
  - src/callgraph.rs
  - src/commands/trace_data.rs
  - opencode-plugin-aft/src/tools/navigation.ts
  - tests/integration/callgraph_test.rs
  - tests/fixtures/callgraph/data_flow.ts
  - tests/fixtures/callgraph/data_processor.ts
key_decisions:
  - trace_data_inner re-parses the file and walks the symbol's AST range directly rather than reusing build_file_data's call sites — needed because we're tracking variable renames through individual AST nodes, not just call edges
  - Approximation stops tracking on that branch entirely (doesn't try to guess through destructuring) — cleaner signal for agents consuming the output
  - Same-file calls resolved via symbol_metadata lookup when resolve_cross_file_edge returns Unresolved (no import needed for local functions)
patterns_established:
  - DataFlowHop with flow_type discriminator ("assignment"|"parameter") and approximate flag for agent-consumable uncertainty signals
  - walk_for_data_flow recursive AST walker with tracked_names accumulator pattern for following variable renames
observability_surfaces:
  - TraceDataResult.depth_limited indicates when tracking stopped due to cross-file depth limit
  - DataFlowHop.approximate flags individual hops where static analysis lost confidence
  - Structured error codes: not_configured, symbol_not_found, invalid_request (consistent with all navigation commands)
duration: 25min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Data flow tracking command — trace expressions through assignments and parameters

**Shipped `trace_data` command that tracks how an expression flows through variable assignments and cross-file function parameters, with approximation markers on destructuring/spread/unresolved calls.**

## What Happened

Added `DataFlowHop` and `TraceDataResult` types to `callgraph.rs`. The `trace_data()` method on `CallGraph` works by: (a) finding the symbol's AST range, (b) walking for `variable_declarator`/`assignment_expression` nodes where the RHS matches a tracked name — when found, the LHS becomes the new tracked name (assignment hop), (c) when a tracked name appears as a call argument, resolving the callee via `resolve_cross_file_edge` or same-file lookup, matching argument position to parameter name via `extract_parameters`, and recursing into the callee (parameter hop), (d) flagging destructuring (`object_pattern`/`array_pattern`) and spread as approximate hops that stop tracking.

Created `src/commands/trace_data.rs` following the exact handler pattern from `impact.rs`: param extraction, configure guard, symbol existence check, then graph method call. Wired dispatch in `main.rs` and added `aft_trace_data` plugin tool in `navigation.ts`.

Created fixture files `data_flow.ts` (assignment chain + cross-file call) and `data_processor.ts` (target function receiving the value).

## Verification

- `cargo test -- trace_data`: 5 integration tests pass (not_configured, symbol_not_found, assignment_tracking, cross_file, approximation)
- `cargo test`: 368 total tests (223 unit + 145 integration), 0 failures
- `bun test`: 39 tests pass, 0 failures
- Manual binary protocol verification confirms correct hop chain: rawInput → cleaned (assignment) → input (parameter in processInput) → normalized (assignment in processInput)
- Manual verification confirms destructuring produces approximate hop with `{ name, value }` variable text

Slice-level checks (T02 is final task — all must pass):
- ✅ `cargo test -- impact`: passes
- ✅ `cargo test -- trace_data`: passes
- ✅ `cargo test`: all pass with 0 failures
- ✅ `bun test`: all pass with 0 failures

## Diagnostics

Send `{"command":"trace_data","file":"<path>","symbol":"<name>","expression":"<expr>","depth":5}` via binary protocol. Response includes `depth_limited` boolean and `hops[]` array. Each hop has `flow_type` ("assignment"|"parameter"), `approximate` flag, `file`, `symbol`, `variable`, and `line`. Error responses use standard codes: `not_configured`, `symbol_not_found`, `invalid_request`.

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `tests/fixtures/callgraph/data_flow.ts` — new fixture with assignment chain and cross-file call to processInput
- `tests/fixtures/callgraph/data_processor.ts` — new fixture as cross-file target function
- `src/callgraph.rs` — added DataFlowHop, TraceDataResult types, trace_data()/trace_data_inner()/walk_for_data_flow()/extract_assignment_info()/check_call_for_data_flow() methods, node_text/find_node_covering_range/find_child_by_kind/extract_callee_names helpers
- `src/commands/trace_data.rs` — new command handler following impact.rs pattern
- `src/commands/mod.rs` — added `pub mod trace_data`
- `src/main.rs` — added `"trace_data"` dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added aft_trace_data tool definition with Zod schema
- `tests/integration/callgraph_test.rs` — added 5 integration tests (not_configured, symbol_not_found, assignment_tracking, cross_file, approximation)
