---
id: T02
parent: S03
milestone: M003
provides:
  - trace_to command handler wired through binary protocol dispatch
  - aft_trace_to plugin tool with Zod schema for agent access
  - 5 integration tests proving full-stack trace_to through binary protocol
key_files:
  - src/commands/trace_to.rs
  - src/main.rs
  - opencode-plugin-aft/src/tools/navigation.ts
  - tests/integration/callgraph_test.rs
key_decisions:
  - Added symbol_metadata check to trace_to symbol existence validation — checkFormat has no outgoing calls and isn't exported, so callers.rs pattern (calls_by_symbol || exported_symbols) misses it. trace_to checks symbol_metadata as a third source.
patterns_established:
  - trace_to command handler follows identical pattern to callers.rs — param extraction, configure guard, build_file, symbol check, graph call, serialize
observability_surfaces:
  - trace_to response includes total_paths, entry_points_found, max_depth_reached, truncated_paths for diagnostic inspection
  - not_configured and symbol_not_found structured error codes (uniform with callers/call_tree)
duration: 12m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Wire trace_to command, dispatch, plugin tool, and integration tests

**Wired trace_to through binary protocol dispatch, registered aft_trace_to plugin tool, proved full stack with 5 integration tests including multi-path verification.**

## What Happened

Created `src/commands/trace_to.rs` following the exact callers.rs pattern — param extraction for file/symbol/depth, configure-then-use guard, symbol existence check, graph.trace_to() call, JSON serialization. One deviation from callers.rs: added `symbol_metadata.contains_key()` as a third symbol existence check because `checkFormat` (a leaf function with no outgoing calls and not exported) wouldn't be found via `calls_by_symbol` or `exported_symbols` alone.

Added dispatch routing in main.rs and `pub mod trace_to` in commands/mod.rs. Registered `aft_trace_to` plugin tool in navigation.ts with description explaining backward trace to entry points, diagnostic response fields, and optional depth parameter.

Wrote 5 integration tests exercising the full binary protocol path:
- `callgraph_trace_to_not_configured` — guard works
- `callgraph_trace_to_symbol_not_found` — error code correct
- `callgraph_trace_to_single_path` — checkFormat traced to main as entry point, path reads top-down
- `callgraph_trace_to_multi_path` — validate traced to 2+ entry points with 2+ distinct paths
- `callgraph_trace_to_no_entry_points` — main (an entry point itself) handled gracefully

## Verification

- `cargo test -- trace_to`: 10 tests pass (5 unit from T01 + 5 new integration)
- `cargo test`: 345 tests pass, 0 failures (208 unit + 137 integration)
- `bun test`: 39 tests pass, 0 failures
- Integration test confirms `trace_to` on `checkFormat` returns path starting at `main` and ending at `checkFormat` (top-down)
- Integration test confirms `trace_to` on `validate` returns 2+ paths with 2+ distinct entry points

### Slice-level verification status
- ✅ `cargo test -- callgraph`: all existing + new trace_to unit tests pass
- ✅ `cargo test -- trace_to`: integration tests through binary protocol pass
- ✅ `cargo test`: full suite 0 failures (345 tests)
- ✅ `bun test`: plugin tests 0 failures (39 tests)
- ✅ Integration test: `trace_to` on `checkFormat` returns path through main → processData → validate → checkFormat
- ✅ Integration test: `trace_to` on validate returns 2+ distinct paths from 2+ entry points

All slice verification checks pass. S03 is complete.

## Diagnostics

- Send `trace_to` command through binary protocol and check response fields: `total_paths`, `entry_points_found`, `max_depth_reached`, `truncated_paths`
- When `total_paths == 0`: check `entry_points_found == 0` (disconnected code) vs `truncated_paths > 0` (paths exist but don't reach entry points)
- Error codes: `not_configured` (no configure call), `symbol_not_found` (bad file/symbol), `invalid_request` (missing params)

## Deviations

Added `symbol_metadata.contains_key(symbol)` as a third condition in the symbol existence check. The callers.rs pattern only checks `calls_by_symbol` and `exported_symbols`, but leaf functions like `checkFormat` (no outgoing calls, not exported) need `symbol_metadata` to be found. This is a narrowly-scoped improvement for trace_to only — callers.rs has the same gap but it's less impactful there since you'd typically look up callers of exported/calling symbols.

## Known Issues

None.

## Files Created/Modified

- `src/commands/trace_to.rs` — new command handler for trace_to
- `src/commands/mod.rs` — added `pub mod trace_to`
- `src/main.rs` — added `"trace_to"` dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added `aft_trace_to` tool definition with Zod schema
- `tests/integration/callgraph_test.rs` — 5 new integration tests for trace_to
- `.gsd/milestones/M003/slices/S03/tasks/T02-PLAN.md` — added Observability Impact section
