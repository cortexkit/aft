---
id: T01
parent: S03
milestone: M003
provides:
  - is_entry_point() heuristic function for detecting entry points across 6 languages
  - SymbolMeta struct and symbol_metadata on FileCallData for entry point classification without re-parsing
  - TraceHop/TracePath/TraceToResult response types for backward path traversal
  - trace_to() BFS backward traversal from any symbol to all reachable entry points
  - lookup_file_data() helper for canonical/non-canonical path resolution in data cache
  - Multi-path test fixtures (service.ts, test_helpers.ts)
key_files:
  - src/callgraph.rs
  - tests/fixtures/callgraph/service.ts
  - tests/fixtures/callgraph/test_helpers.ts
key_decisions:
  - Entry point detection continues BFS past intermediate entry points — if validate is exported (entry point), we record that path AND continue backward to find deeper paths through processData, main, etc. This gives the richest possible trace.
  - Added lookup_file_data() helper that tries canonical and non-canonical path forms because build_reverse_index stores data under raw walker paths while CallerSite uses canonical paths. The alternative was normalizing keys in build_reverse_index, but that would change existing callers_of behavior.
  - BFS uses per-path visited sets (not global) so the same intermediate symbol can appear in multiple distinct paths.
patterns_established:
  - is_entry_point is a standalone pure function — easy to test and extend per-language without touching graph traversal logic
  - trace_to follows the same lazy reverse-index pattern as callers_of (build on first call, cache until invalidation)
observability_surfaces:
  - TraceToResult includes total_paths, entry_points_found, max_depth_reached, truncated_paths — callers can assess completeness
  - symbol_metadata on FileCallData makes entry point classification inspectable per-file
duration: 45m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Implement entry point detection and backward path traversal

**Extended the callgraph with `is_entry_point()` heuristics, `SymbolMeta` metadata, and `trace_to()` BFS backward traversal that finds all paths from entry points to any target symbol.**

## What Happened

Added `kind: SymbolKind` to the private `SymbolInfo` struct and mapped it from `Symbol.kind` in `list_symbols_from_tree()`. Created `SymbolMeta` struct (kind, exported, signature) and added `symbol_metadata: HashMap<String, SymbolMeta>` to `FileCallData`, populated in `build_file_data()`.

Implemented `is_entry_point()` as a standalone pure function covering: exported functions (not methods), main/init/setup/bootstrap/run patterns (case-insensitive), and language-specific test patterns (TS/JS/TSX: describe/it/test/testX/specX; Python: test_/setUp/tearDown; Rust: test_; Go: Test).

Implemented `trace_to()` on `CallGraph` using BFS backward traversal through the reverse index. Key design choices: continues searching past intermediate entry points to find all reachable paths (not just nearest entry point), uses per-path visited sets for cycle detection, reverses collected paths to top-down (entry point first, target last), and sorts paths deterministically.

Hit a path canonicalization bug: `build_reverse_index` stores file data under raw walker paths but `CallerSite.caller_file` uses canonical paths, so `self.data.get(&site.caller_file)` failed silently for entry point detection. Fixed by adding `lookup_file_data()` that tries both forms.

Created multi-path fixture files: `service.ts` (exported `handleRequest` → processData) and `test_helpers.ts` (testValidation → validate).

## Verification

- `cargo test -- callgraph`: 33 tests pass (20 existing + 13 new)
- `cargo test -- is_entry_point`: 8 tests pass (exported function, exported method, main/init, TS/Python/Rust/Go patterns, negative case)
- `cargo test -- trace_to`: 5 tests pass (multi_path, single_path, cycle_detection, depth_limit, entry_point_target)
- `cargo test`: 340 total (208 unit + 132 integration), 0 failures
- `bun test`: 39 pass, 0 failures
- No existing tests broken

**Slice-level checks (partial — T01 is intermediate):**
- ✅ `cargo test -- callgraph`: all pass
- ⏳ `cargo test -- trace_to` integration tests: T02 scope
- ✅ `cargo test`: full suite 0 failures
- ✅ `bun test`: 0 failures
- ⏳ Integration test for trace_to on checkFormat: T02 scope
- ⏳ Integration test for multi-path: T02 scope

## Diagnostics

`TraceToResult` response includes diagnostic fields:
- `total_paths`: number of complete paths found
- `entry_points_found`: distinct entry points across all paths
- `max_depth_reached`: true if any path was cut short by depth limit
- `truncated_paths`: count of dead-end paths (no callers, not entry point)

When trace_to returns 0 paths, check `entry_points_found == 0` (disconnected code) vs `truncated_paths > 0` (paths exist but don't reach entry points).

## Deviations

- Added `lookup_file_data()` helper method (not in original plan) to handle path canonicalization mismatch between data cache and reverse index. This is a targeted fix; the root cause is `build_reverse_index` not canonicalizing keys, but fixing that would be a broader change affecting existing callers_of behavior.

## Known Issues

- `build_reverse_index` inserts data under non-canonical walker paths, requiring the `lookup_file_data()` workaround. A future cleanup could canonicalize all keys in `build_reverse_index`, but current tests all pass.
- The `lookup_file_data` fallback path does an O(n) scan over all data keys when both direct lookups miss. Fine for current project sizes but worth noting.

## Files Created/Modified

- `src/callgraph.rs` — Added SymbolMeta, is_entry_point(), TraceHop/TracePath/TraceToResult types, trace_to() method, lookup_file_data() helper, 13 new unit tests
- `tests/fixtures/callgraph/service.ts` — New fixture: exported handleRequest calling processData
- `tests/fixtures/callgraph/test_helpers.ts` — New fixture: testValidation calling validate
- `.gsd/milestones/M003/slices/S03/tasks/T01-PLAN.md` — Added Observability Impact section
