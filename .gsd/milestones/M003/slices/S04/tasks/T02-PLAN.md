---
estimated_steps: 8
estimated_files: 8
---

# T02: Data flow tracking command — trace expressions through assignments and parameters

**Slice:** S04 — Data Flow Tracking + Impact Analysis
**Milestone:** M003

## Description

Implement the `trace_data` command (R024) — tracks how an expression flows through variable assignments within a function body and across function boundaries via argument-to-parameter matching. The minimal viable scope (D082) covers direct assignments and function parameters only; destructuring, spread, and conditional assignments are marked as approximations and stop tracking.

## Steps

1. Create new fixture files for data flow testing. `tests/fixtures/callgraph/data_flow.ts`: a function that takes an input, assigns it to a local variable, passes it to a cross-file function call. `tests/fixtures/callgraph/data_processor.ts`: the target function that receives the value as a parameter and does further processing. These fixtures must demonstrate the assignment→call→parameter tracking chain.
2. Add response types to `src/callgraph.rs`: `DataFlowHop` (file, symbol, variable, line, flow_type: "parameter"|"assignment"|"return", approximate: bool) and `TraceDataResult` (expression, origin_file, origin_symbol, hops: Vec<DataFlowHop>, depth_limited: bool).
3. Implement `trace_data()` on `CallGraph`. Algorithm: (a) Parse the function body of `symbol` using tree-sitter, find the node matching `expression`. (b) Walk the function body's AST for `variable_declarator`/`assignment_expression` nodes that reference the expression — when `const x = <expr>`, `x` becomes the new tracking name (add an "assignment" hop). (c) When the tracked name appears as an argument in a call expression, resolve the callee via `resolve_cross_file_edge`, match the argument's position index to the callee's parameter name using `extract_parameters` (from T01), and recurse into the callee's body tracking the parameter name (add a "parameter" hop). (d) Depth-limit cross-file hops (default 5). (e) When hitting destructuring (`object_pattern`, `array_pattern`), spread (`spread_element`), or unresolved calls, add a hop with `approximate: true` and stop tracking through that branch.
4. Use tree-sitter node kinds per language for the AST walk: `variable_declarator` (TS/JS), `assignment` (Python), `let_declaration` (Rust), `short_var_declaration` (Go). For call argument matching, use `arguments`/`argument_list` child nodes and count position. The function body scope is determined by looking up the symbol in `calls_by_symbol` to confirm existence, then re-parsing the file to walk the symbol's AST range.
5. Create `src/commands/trace_data.rs` with `handle_trace_data()`: extract `file`, `symbol`, `expression` (required), `depth` (default 5) params, configure guard, symbol existence check, call `graph.trace_data()`, serialize.
6. Add `pub mod trace_data` to `src/commands/mod.rs`. Add `"trace_data"` dispatch entry in `src/main.rs`.
7. Add `aft_trace_data` tool definition in `opencode-plugin-aft/src/tools/navigation.ts` with Zod schema (file, symbol, expression, depth optional).
8. Write integration tests: trace_data on expression in data_flow.ts tracking through assignment, trace_data tracking across file boundary via function parameter into data_processor.ts, approximation case, not_configured guard, symbol_not_found error.

## Must-Haves

- [ ] `DataFlowHop` and `TraceDataResult` types with Serialize derive
- [ ] `trace_data()` method on CallGraph with assignment tracking and cross-file parameter matching
- [ ] Approximation markers on destructuring/spread/unresolved calls
- [ ] Depth-limited cross-file hops
- [ ] `handle_trace_data` command handler with configure guard
- [ ] Dispatch entry in main.rs
- [ ] `aft_trace_data` plugin tool with Zod schema
- [ ] New fixture files for data flow scenarios
- [ ] Integration tests through binary protocol

## Verification

- `cargo test -- trace_data` — all trace_data tests pass
- `cargo test` — full suite passes with 0 failures (all existing + new)
- `bun test` — all plugin tests pass with 0 failures
- Manually verify: expression tracking produces hops with correct flow_type values

## Observability Impact

- Signals added: `TraceDataResult.depth_limited` indicates when tracking stopped due to depth limit; `DataFlowHop.approximate` flags individual hops where tracking is uncertain
- How a future agent inspects this: query `trace_data` through binary protocol, check `depth_limited` and `approximate` fields in response
- Failure state exposed: "approximate" hops indicate where static analysis lost confidence — agents can decide whether to trust the result

## Inputs

- `src/callgraph.rs` — CallGraph, extract_parameters() from T01, build_file(), resolve_cross_file_edge()
- `src/calls.rs` — extract_calls_full() for call expression node walking patterns
- `src/parser.rs` — grammar_for(), FileParser for AST access
- `src/commands/impact.rs` — T01's handler as pattern reference
- `tests/fixtures/callgraph/` — existing fixtures + new ones from step 1

## Expected Output

- `tests/fixtures/callgraph/data_flow.ts` — new fixture with assignment chains
- `tests/fixtures/callgraph/data_processor.ts` — new fixture as cross-file target
- `src/callgraph.rs` — DataFlowHop, TraceDataResult, trace_data() method
- `src/commands/trace_data.rs` — handle_trace_data command handler
- `src/commands/mod.rs` — added pub mod trace_data
- `src/main.rs` — added "trace_data" dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added aft_trace_data tool
- `tests/integration/callgraph_test.rs` — 4+ new integration tests
