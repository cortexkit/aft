---
id: S03
parent: M003
milestone: M003
provides:
  - is_entry_point() heuristic function detecting exported functions, main/init patterns, test patterns across 6 languages
  - SymbolMeta struct and symbol_metadata on FileCallData for entry point classification without re-parsing
  - trace_to() BFS backward traversal from any symbol to all reachable entry points via reverse index
  - TraceHop/TracePath/TraceToResult response types with diagnostic fields (total_paths, entry_points_found, max_depth_reached, truncated_paths)
  - handle_trace_to command handler with configure-then-use guard
  - trace_to dispatch entry in main.rs
  - aft_trace_to plugin tool with Zod schema
  - lookup_file_data() helper for canonical/non-canonical path resolution in graph data cache
requires:
  - slice: S01
    provides: CallGraph struct with forward graph, resolve_cross_file_edge(), worktree-scoped file walking
  - slice: S02
    provides: Reverse index (build_reverse_index, callers_of), file watcher invalidation, invalidate_file()
affects:
  - S04
key_files:
  - src/callgraph.rs
  - src/commands/trace_to.rs
  - src/main.rs
  - opencode-plugin-aft/src/tools/navigation.ts
  - tests/integration/callgraph_test.rs
  - tests/fixtures/callgraph/service.ts
  - tests/fixtures/callgraph/test_helpers.ts
key_decisions:
  - D093 — SymbolMeta in FileCallData for entry point detection
  - D094 — Per-path visited sets in trace_to BFS
  - D095 — Default trace_to depth 10
  - trace_to continues BFS past intermediate entry points to find all reachable paths (not just nearest entry point)
  - symbol_metadata used as third symbol existence check in trace_to handler (leaf functions without outgoing calls or exports)
patterns_established:
  - is_entry_point is a standalone pure function — easy to test and extend per-language without touching traversal logic
  - trace_to command handler follows identical pattern to callers.rs — param extraction, configure guard, build_file, symbol check, graph call, serialize
observability_surfaces:
  - TraceToResult includes total_paths, entry_points_found, max_depth_reached, truncated_paths — agents assess completeness
  - symbol_metadata on FileCallData makes entry point classification inspectable per-file
  - Structured error codes: not_configured, symbol_not_found, invalid_request (uniform with callers/call_tree)
drill_down_paths:
  - .gsd/milestones/M003/slices/S03/tasks/T01-SUMMARY.md
  - .gsd/milestones/M003/slices/S03/tasks/T02-SUMMARY.md
duration: 57m
verification_result: passed
completed_at: 2026-03-14
---

# S03: Trace to Entry Points

**Backward path traversal from any symbol to all entry points — `trace_to` command returns complete top-down paths through the call graph, proven by multi-path integration tests through binary protocol.**

## What Happened

T01 extended the callgraph data model and implemented the core algorithm. Added `kind: SymbolKind` to `SymbolInfo` and created `SymbolMeta` (kind, exported, signature) with a `symbol_metadata` map on `FileCallData`, populated from the existing `list_symbols_from_tree` call in `build_file_data()`. Implemented `is_entry_point()` as a standalone pure function covering exported functions (not methods), main/init/setup/bootstrap/run name patterns, and language-specific test patterns (TS/JS describe/it/test, Python test_/setUp/tearDown, Rust test_, Go Test).

The core `trace_to()` uses BFS backward traversal through the reverse index. Key algorithmic choice: continues searching past intermediate entry points — if `validate` is exported (entry point), it records that path AND continues backward to find deeper paths through `processData`, `main`, etc. Uses per-path visited sets (D094) so the same intermediate symbol can appear in multiple distinct paths. Paths are reversed to top-down rendering (entry point first, target last) and sorted deterministically.

Hit a path canonicalization bug: `build_reverse_index` stores file data under raw walker paths while `CallerSite.caller_file` uses canonical paths. Added `lookup_file_data()` helper that tries both forms.

T02 wired the command through binary protocol. Created `handle_trace_to` following the callers.rs pattern exactly, with one addition: `symbol_metadata.contains_key()` as a third symbol existence check because leaf functions (no outgoing calls, not exported) wouldn't be found otherwise. Added dispatch entry, plugin tool with Zod schema, and 5 integration tests proving the full stack.

## Verification

- `cargo test -- callgraph`: 33 tests pass (20 existing + 13 new)
- `cargo test -- trace_to`: 10 tests pass (5 unit + 5 integration)
- `cargo test`: 345 total (208 unit + 137 integration), 0 failures
- `bun test`: 39 pass, 0 failures
- Integration: `trace_to` on `checkFormat` returns path starting at `main`, ending at `checkFormat` (top-down)
- Integration: `trace_to` on `validate` returns 2+ paths with 2+ distinct entry points
- Integration: not_configured guard, symbol_not_found error, no-entry-point graceful handling all verified

## Requirements Advanced

- R023 (Reverse trace to entry points) — `trace_to` command fully implemented and proven through binary protocol with multi-path integration tests
- R026 (Entry point detection heuristics) — `is_entry_point()` covers generic patterns (exported functions, main/init, test patterns per language)

## Requirements Validated

- R023 — Integration tests prove backward traversal from deeply-nested utility to all entry points, rendered top-down, with multi-path support, cycle detection, depth limiting, and diagnostic metadata
- R026 — Unit tests prove classification of exported functions, main/init/setup/run patterns, TS/JS/Python/Rust/Go test patterns, and negative cases (non-exported, methods, non-test names)

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- Added `lookup_file_data()` helper method (not in plan) to handle path canonicalization mismatch between data cache keys and reverse index caller paths
- Added `symbol_metadata.contains_key()` as third condition in trace_to symbol existence check — callers.rs pattern only checks `calls_by_symbol` and `exported_symbols`, insufficient for leaf functions

## Known Limitations

- `lookup_file_data()` fallback does O(n) scan over all data keys when both direct lookups miss — fine for current project sizes
- `build_reverse_index` stores data under non-canonical walker paths, requiring the workaround — root cause not fixed to avoid changing callers_of behavior
- Framework-specific entry point patterns (Express routes, Flask decorators, Axum handlers) not yet implemented — deferred per D081

## Follow-ups

- none — S04 (trace_data + impact) is the next planned slice

## Files Created/Modified

- `src/callgraph.rs` — SymbolMeta, is_entry_point(), TraceHop/TracePath/TraceToResult, trace_to(), lookup_file_data(), 13 unit tests
- `src/commands/trace_to.rs` — new command handler
- `src/commands/mod.rs` — added `pub mod trace_to`
- `src/main.rs` — added `"trace_to"` dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added `aft_trace_to` tool with Zod schema
- `tests/integration/callgraph_test.rs` — 5 new integration tests
- `tests/fixtures/callgraph/service.ts` — fixture: exported handleRequest → processData
- `tests/fixtures/callgraph/test_helpers.ts` — fixture: testValidation → validate

## Forward Intelligence

### What the next slice should know
- `trace_to()` provides the backward traversal infrastructure S04's `impact` command needs — it already finds all callers-of-callers transitively
- `is_entry_point()` is a pure function taking `(name, kind, exported, lang)` — can be reused in `impact` to annotate whether affected callers are entry points
- `symbol_metadata` on `FileCallData` provides kind/exported/signature per symbol — `trace_data` can use signatures to track parameter names through call chains

### What's fragile
- Path canonicalization mismatch between `build_reverse_index` (raw walker paths) and `CallerSite` (canonical paths) — `lookup_file_data()` papers over it, but any new code touching the data cache should be aware
- `is_entry_point()` test patterns are string-based heuristics — false positives possible for functions named `test_*` that aren't tests

### Authoritative diagnostics
- `TraceToResult.total_paths == 0` with `truncated_paths > 0` means paths exist but don't reach entry points — check if the entry points are detected correctly
- `cargo test -- is_entry_point` runs all 8 classification unit tests — check these first if entry point detection seems wrong

### What assumptions changed
- Assumed `calls_by_symbol` + `exported_symbols` would be sufficient for symbol existence checks — leaf functions (no outgoing calls, not exported) require `symbol_metadata` as a third source
