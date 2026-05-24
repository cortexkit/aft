---
id: T01
parent: S02
milestone: M003
provides:
  - Reverse caller index in CallGraph with lazy full-project scan
  - callers protocol command returning call sites grouped by file with recursive depth
  - invalidate_file() for file watcher integration
  - aft_callers plugin tool with Zod schema
key_files:
  - src/callgraph.rs
  - src/commands/callers.rs
  - tests/integration/callgraph_test.rs
  - opencode-plugin-aft/src/tools/navigation.ts
key_decisions:
  - D092 (reverse index cleared entirely on invalidation — already recorded)
patterns_established:
  - Caller paths in reverse index must be canonicalized for consistent lookup across walk_project_files() output and EdgeResolution targets
observability_surfaces:
  - callers response includes total_callers and scanned_files counts
  - symbol_not_found error code when target symbol missing
  - not_configured error code when callers called before configure
duration: 25m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Reverse index + `callers` command + plugin tool

**Built reverse caller index, `callers` command handler, and `aft_callers` plugin tool with full cross-file resolution and recursive depth expansion.**

## What Happened

Added `CallerSite`, `CallerGroup`, `CallerEntry`, and `CallersResult` types to `callgraph.rs`. Implemented `build_reverse_index()` which scans all project files via `walk_project_files()`, builds file data for each, then inverts `calls_by_symbol` using `resolve_cross_file_edge()` to populate a `HashMap<(PathBuf, String), Vec<CallerSite>>`.

`callers_of(file, symbol, depth)` lazily triggers the index build, then uses `collect_callers_recursive()` with visited-set cycle detection to gather callers up to the requested depth. Results are grouped by file with deterministic sort order.

`invalidate_file(path)` removes the file from the data cache, clears the reverse index (for lazy rebuild), and clears the project_files cache (for create/remove events).

Created `src/commands/callers.rs` following the exact configure-then-use guard pattern from `call_tree.rs`. Wired into dispatch table and plugin tool registration.

Key implementation detail: caller file paths in `CallerSite` must be canonicalized because `walk_project_files()` returns non-canonical paths while `EdgeResolution::Resolved` targets are canonical (from `resolve_module_path → std::fs::canonicalize`). Without canonicalization, recursive lookups fail to match reverse index keys.

## Verification

- `cargo test -- callgraph`: 19 unit tests + 11 integration tests pass (4 new unit, 4 new integration)
- `cargo test`: 194 unit + 130 integration = 324 total, all pass
- `bun test`: 39 pass, `aft_callers` tool registered
- Manual protocol verification: configure → callers for `validate` in helpers.ts → response shows callers from utils.ts (processData) and aliased.ts (runCheck) with `total_callers: 2`, `scanned_files: 5`
- Slice-level checks:
  - ✅ `cargo test -- callgraph` — all existing 22 + 8 new tests pass
  - ✅ `cargo test` — all 324 tests pass
  - ✅ `bun test` — all 39 pass
  - ⬜ Integration test: configure → callers → modify fixture → drain → callers again (T02 — watcher not yet implemented)

## Diagnostics

- `callers` response always includes `total_callers` (int) and `scanned_files` (int) for observability
- `not_configured` error: send `callers` before `configure` → immediate structured error
- `symbol_not_found` error: send `callers` with invalid symbol → error with symbol name and file path
- Empty callers: `total_callers: 0` with empty `callers` array when symbol has no callers

## Deviations

None — implemented as planned.

## Known Issues

None.

## Files Created/Modified

- `src/callgraph.rs` — Added CallerSite/CallerGroup/CallerEntry/CallersResult types, reverse_index field, build_reverse_index(), callers_of(), collect_callers_recursive(), invalidate_file(), 4 unit tests
- `src/commands/callers.rs` — New command handler with configure-then-use guard and symbol_not_found check
- `src/commands/mod.rs` — Added `pub mod callers`
- `src/main.rs` — Added `"callers"` dispatch entry
- `tests/integration/callgraph_test.rs` — 4 new integration tests (without_configure, cross_file, empty_result, recursive)
- `opencode-plugin-aft/src/tools/navigation.ts` — Added `aft_callers` tool definition with Zod schema
- `.gsd/milestones/M003/slices/S02/S02-PLAN.md` — Added failure-path verification step, marked T01 done
