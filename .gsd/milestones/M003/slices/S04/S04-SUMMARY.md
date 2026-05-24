---
id: S04
parent: M003
milestone: M003
provides:
  - impact command returning callers annotated with signatures, entry point status, call expressions, and extracted parameters
  - trace_data command tracking expressions through assignments and cross-file parameter matching with approximation markers
  - extract_parameters utility for per-language parameter name extraction from signatures
  - aft_impact and aft_trace_data plugin tools with Zod schemas
  - All 5 M003 navigation commands complete (call_tree, callers, trace_to, impact, trace_data)
requires:
  - slice: S03
    provides: CallGraph with forward_tree, callers_of, trace_to, is_entry_point, symbol_metadata, reverse index
affects: []
key_files:
  - src/callgraph.rs
  - src/commands/impact.rs
  - src/commands/trace_data.rs
  - opencode-plugin-aft/src/tools/navigation.ts
  - tests/integration/callgraph_test.rs
  - tests/fixtures/callgraph/data_flow.ts
  - tests/fixtures/callgraph/data_processor.ts
key_decisions:
  - D098: extract_parameters uses bracket-depth-aware comma splitting to handle generics in type annotations
  - D099: trace_data re-parses file AST and walks symbol range directly rather than reusing build_file_data call sites
  - D082 applied: approximation stops tracking on that branch entirely (no guessing through destructuring/spread)
patterns_established:
  - extract_parameters(signature, lang) shared utility for argument-to-parameter matching
  - DataFlowHop with flow_type discriminator and approximate flag for agent-consumable uncertainty signals
  - ImpactCaller struct pattern (enriched caller with metadata)
observability_surfaces:
  - ImpactResult.total_affected and affected_files for blast radius at a glance
  - ImpactCaller.is_entry_point flags public API surfaces
  - TraceDataResult.depth_limited indicates cross-file depth limit reached
  - DataFlowHop.approximate flags individual hops where static analysis lost confidence
  - Structured error codes: not_configured, symbol_not_found, invalid_request (consistent with all navigation commands)
drill_down_paths:
  - .gsd/milestones/M003/slices/S04/tasks/T01-SUMMARY.md
  - .gsd/milestones/M003/slices/S04/tasks/T02-SUMMARY.md
duration: 55min
verification_result: passed
completed_at: 2026-03-14
---

# S04: Data Flow Tracking + Impact Analysis

**Shipped `aft_impact` and `aft_trace_data` — the final two call graph navigation commands, completing M003's five-command navigation suite.**

## What Happened

**T01 (Impact Analysis):** Built the `impact` command that returns all callers of a symbol annotated with signatures, entry point status, call site source lines, and extracted parameter names. The core `extract_parameters()` utility handles per-language receiver skipping (Rust `&self`/`&mut self`, Python `self`), rest/spread params, type annotations, defaults, and uses bracket-depth-aware comma splitting for generics. The `impact()` method reuses `callers_of`'s recursive traversal then enriches each caller with `SymbolMeta` (signature, kind, exported), entry point status via `is_entry_point()`, and source line context. Results are deduplicated by (file, symbol, line) and sorted deterministically.

**T02 (Data Flow Tracking):** Built the `trace_data` command that tracks how an expression flows through variable assignments and cross-file function parameter passing. The implementation re-parses the target file's AST and walks the symbol's range for `variable_declarator`/`assignment_expression` nodes — when the RHS matches a tracked name, the LHS becomes the new tracked name (assignment hop). When a tracked name appears as a call argument, the callee is resolved via `resolve_cross_file_edge` or same-file `symbol_metadata` lookup, argument position is matched to parameter name via `extract_parameters`, and tracking recurses into the callee (parameter hop). Destructuring, spread, and unresolved calls produce `approximate: true` hops that stop tracking on that branch.

Both commands follow the established handler pattern exactly (param extraction → configure guard → symbol existence check → graph method). Both wired through dispatch and registered as plugin tools.

## Verification

- `cargo test -- impact`: 3 integration tests pass (not_configured, symbol_not_found, multi_caller with entry point + signature + parameter assertions)
- `cargo test -- trace_data`: 5 integration tests pass (not_configured, symbol_not_found, assignment_tracking, cross_file, approximation)
- `cargo test -- extract_parameters`: 15 unit tests pass (TS, JS, Python, Rust, Go, edge cases)
- `cargo test`: 368 total (223 unit + 145 integration), 0 failures
- `bun test`: 39 tests pass, 0 failures

## Requirements Advanced

- R024 (Data flow tracking) — `trace_data` command tracks expressions through assignments and cross-file parameter matching with approximation markers
- R025 (Change impact analysis) — `impact` command returns all callers with signatures, entry point flags, and call site context

## Requirements Validated

- R024 — Proven by 5 integration tests: assignment tracking, cross-file parameter matching, approximation on destructuring/spread, error paths
- R025 — Proven by 3 integration tests: multi-caller impact with entry point + signature + parameter assertions, error paths

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

None.

## Known Limitations

- `trace_data` tracks through direct assignments and function parameters only — destructuring, spread, and conditional assignments produce approximation markers and stop tracking (per D082)
- `extract_parameters` operates on signature text, not full AST — generics with nested angle brackets are handled via bracket-depth tracking but very deep nesting could theoretically break
- `impact` and `trace_data` share the same depth limit as other navigation commands (default 5 for cross-file hops)

## Follow-ups

- none — S04 is terminal for M003

## Files Created/Modified

- `src/callgraph.rs` — ImpactResult, ImpactCaller, DataFlowHop, TraceDataResult types; extract_parameters/split_params/extract_param_name utilities; impact()/trace_data()/trace_data_inner()/walk_for_data_flow() methods; helper functions; 15 unit tests
- `src/commands/impact.rs` — new command handler
- `src/commands/trace_data.rs` — new command handler
- `src/commands/mod.rs` — added pub mod impact, pub mod trace_data
- `src/main.rs` — added "impact" and "trace_data" dispatch entries
- `opencode-plugin-aft/src/tools/navigation.ts` — added aft_impact and aft_trace_data tool definitions with Zod schemas
- `tests/integration/callgraph_test.rs` — added 8 integration tests (3 impact + 5 trace_data)
- `tests/fixtures/callgraph/data_flow.ts` — new fixture with assignment chain and cross-file call
- `tests/fixtures/callgraph/data_processor.ts` — new fixture as cross-file target function

## Forward Intelligence

### What the next slice should know
- M003 is complete — all 5 navigation commands ship. The call graph infrastructure (forward tree, reverse callers, trace_to, impact, trace_data) is proven with 368 tests.
- `extract_parameters()` is the shared utility for any future work needing argument-to-parameter position matching. It handles receiver skipping, generics, defaults, and rest params across all 6 languages.

### What's fragile
- `trace_data`'s AST walking uses string-based node kind matching (`variable_declarator`, `assignment_expression`) — language additions would need to extend the kind lists
- Cross-file parameter matching depends on `extract_parameters` correctly parsing signatures — complex generic types or multi-line signatures could produce incorrect parameter counts

### Authoritative diagnostics
- `cargo test -- impact` and `cargo test -- trace_data` — these integration tests exercise the full binary protocol round-trip and are the authoritative signal
- `extract_parameters` unit tests cover all 6 languages and edge cases — if parameter matching breaks, start here

### What assumptions changed
- No assumptions changed — both commands built cleanly on the S01-S03 infrastructure as planned
