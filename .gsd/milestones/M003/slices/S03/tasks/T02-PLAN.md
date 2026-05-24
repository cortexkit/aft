---
estimated_steps: 6
estimated_files: 5
---

# T02: Wire trace_to command, dispatch, plugin tool, and integration tests

**Slice:** S03 — Trace to Entry Points
**Milestone:** M003

## Description

Wire the `trace_to` algorithm from T01 through the binary protocol and plugin tool layer. Create the command handler, add dispatch routing, register the plugin tool with Zod schema, and write integration tests proving the full stack works through the binary protocol — the same path agents use.

## Steps

1. Create `src/commands/trace_to.rs` with `handle_trace_to()`:
   - Extract params: `file` (required string), `symbol` (required string), `depth` (optional u64, default 10)
   - Configure-then-use guard: check `ctx.callgraph().borrow_mut().as_mut()`, return `not_configured` if None
   - Build file data, check symbol exists (same pattern as callers.rs)
   - Call `graph.trace_to(file_path, symbol, depth)`
   - Serialize `TraceToResult` to JSON, return success response

2. Add `pub mod trace_to;` to `src/commands/mod.rs`

3. Add `"trace_to" => aft::commands::trace_to::handle_trace_to(&req, ctx),` to `dispatch()` in `src/main.rs`

4. Add `aft_trace_to` tool to `opencode-plugin-aft/src/tools/navigation.ts`:
   - Description: trace backward from a symbol to all entry points, rendered top-down
   - Args: `file` (string, required), `symbol` (string, required), `depth` (number, optional, default 10)
   - Execute: bridge.send("trace_to", params)

5. Write integration tests in `tests/integration/callgraph_test.rs`:
   - `callgraph_trace_to_single_path`: configure → trace_to on `checkFormat` → verify path includes `main` as entry point and `checkFormat` as target, path reads top-down
   - `callgraph_trace_to_multi_path`: configure → trace_to on `validate` (called from `main` via `processData` AND from `handleRequest` via `processData` AND from `testValidation` directly) → verify multiple paths returned with correct `total_paths` and `entry_points_found`
   - `callgraph_trace_to_not_configured`: trace_to without configure → `not_configured` error
   - `callgraph_trace_to_symbol_not_found`: configure → trace_to on nonexistent symbol → `symbol_not_found` error
   - `callgraph_trace_to_no_entry_points`: configure → trace_to on an entry point itself → verify empty paths or self-path graceful handling

6. Run full verification: `cargo test`, `bun test`

## Must-Haves

- [ ] `handle_trace_to` command handler with param extraction and error handling
- [ ] Dispatch routing in main.rs
- [ ] `aft_trace_to` plugin tool with Zod schema
- [ ] Integration tests proving trace_to through binary protocol
- [ ] Multi-path integration test using fixture files from T01
- [ ] All existing tests still pass (326+ Rust, 39+ bun)

## Verification

- `cargo test -- trace_to`: all new integration tests pass
- `cargo test`: 0 failures (all existing + new)
- `bun test`: 0 failures (39+ existing + new)
- Integration test confirms `trace_to` returns top-down paths through binary protocol

## Inputs

- `src/callgraph.rs` — `trace_to()`, `TraceToResult` types from T01
- `src/commands/callers.rs` — command handler pattern to follow
- `opencode-plugin-aft/src/tools/navigation.ts` — plugin tool pattern to follow
- `tests/fixtures/callgraph/` — multi-path fixtures from T01
- `tests/integration/callgraph_test.rs` — existing AftProcess test harness

## Expected Output

- `src/commands/trace_to.rs` — new command handler
- `src/commands/mod.rs` — updated with `pub mod trace_to`
- `src/main.rs` — updated dispatch with `"trace_to"` entry
- `opencode-plugin-aft/src/tools/navigation.ts` — updated with `aft_trace_to` tool
- `tests/integration/callgraph_test.rs` — 5 new integration tests

## Observability Impact

- **New structured error codes:** `not_configured` and `symbol_not_found` on the `trace_to` command — same codes as `callers` and `call_tree`, agents handle them uniformly
- **Diagnostic response fields:** Every `trace_to` response includes `total_paths`, `entry_points_found`, `max_depth_reached`, `truncated_paths` — agents can detect incomplete results (depth limit hit, disconnected code) without parsing paths
- **Plugin tool surface:** `aft_trace_to` exposes the full diagnostic payload to agents — when `total_paths == 0`, check `entry_points_found == 0` (disconnected) vs `truncated_paths > 0` (paths exist but don't reach entry points)
- **Inspection:** Future agents debug trace_to by sending the command directly through the binary protocol and checking diagnostic fields in the response
