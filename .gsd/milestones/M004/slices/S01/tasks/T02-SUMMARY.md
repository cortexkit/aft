---
id: T02
parent: S01
milestone: M004
provides:
  - 9 integration tests for move_symbol command exercising success, dry-run, checkpoint, and error paths through binary protocol
  - Bug fix: consumer file path resolution now canonicalizes paths to handle macOS /var→/private/var symlink
  - Bug fix: CallerGroup.file relative paths resolved against project root before filesystem operations
key_files:
  - tests/integration/move_symbol_test.rs
  - tests/integration/main.rs
  - src/commands/move_symbol.rs (bug fixes)
  - src/callgraph.rs (added project_root() getter)
  - tests/fixtures/move_symbol/service.ts (added DateHelper class for non-top-level test)
key_decisions:
  - D111: Canonicalize source/dest paths in move_symbol handler to match callgraph's canonicalized consumer paths
patterns_established:
  - setup_move_fixture() copies fixtures including subdirectories for move_symbol temp-dir isolation
  - configure() helper extracts common configure-and-assert pattern for move tests
observability_surfaces:
  - cargo test move_symbol — 9 tests verify success, dry-run, checkpoint, and error paths
  - Test names map directly to sub-features: move_symbol_basic, _multiple_consumers, _aliased_import, _dry_run, _checkpoint, _not_configured, _symbol_not_found, _non_top_level, _file_not_found
duration: 30min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Integration tests for move_symbol through binary protocol

**9 integration tests proving move_symbol end-to-end — caught and fixed two path resolution bugs in the handler along the way.**

## What Happened

Created `tests/integration/move_symbol_test.rs` with 9 tests covering all specified paths:

**Success tests (3):**
- `move_symbol_basic` — symbol removed from source, added to destination with export, consumer import updated
- `move_symbol_multiple_consumers` — explicitly verifies all 5+ consumer files (consumer_a through consumer_f plus features/consumer_e), including that consumer_d (imports DATE_FORMAT only) and consumer_f (imports parseDate only) are left unchanged
- `move_symbol_aliased_import` — verifies `import { formatDate as fmtDate }` preserves the alias after path rewrite

**Safety tests (2):**
- `move_symbol_dry_run` — snapshots file contents before, sends dry_run:true, verifies diffs returned for 3+ files, verifies zero files modified on disk
- `move_symbol_checkpoint` — performs move, verifies checkpoint appears in list_checkpoints, restores it, verifies all files return to original state

**Error tests (4):**
- `move_symbol_not_configured` — uses real temp files to pass file-exists guard, verifies not_configured error
- `move_symbol_symbol_not_found` — nonexistent symbol returns symbol_not_found
- `move_symbol_non_top_level` — `format` method inside DateHelper class returns invalid_request with "non-top-level" message
- `move_symbol_file_not_found` — nonexistent source file returns file_not_found

**Bugs found and fixed:**

1. **CallerGroup.file relative path resolution** — `callers_of` returns relative paths (e.g. `consumer_a.ts`), but the handler used them as-is in `PathBuf::from()`, causing `exists()` to fail. Fixed by resolving against `graph.project_root()`. Added `project_root()` getter to CallGraph.

2. **macOS path canonicalization mismatch** — On macOS, `tempdir()` returns `/var/folders/...` but `canonicalize()` returns `/private/var/folders/...`. The callgraph canonicalizes internal paths, so consumer file paths from callers_of are canonical (`/private/var/...`) while source/dest paths from the request are not. `import_path_matches_file` failed because it compared non-canonical source path against canonical consumer path. Fixed by canonicalizing source_path and dest_path early in the handler.

## Verification

- `cargo test move_symbol` — 28 tests pass (19 unit + 9 integration)
- `cargo test` — 396 tests pass (242 unit + 154 integration), zero failures, zero regressions

Slice-level verification status (T02 is task 2 of 3):
- ✅ Basic move: symbol removed from source, added to destination, consumer imports updated
- ✅ 5+ consumer files all rewired correctly (different directory depths)
- ✅ Aliased import preserved after rewiring
- ✅ Dry-run returns multi-file diff, files unchanged on disk
- ✅ `not_configured` error when call graph not initialized
- ✅ `symbol_not_found` error for nonexistent symbol
- ✅ Method/class member rejected with appropriate error (D100)
- ✅ Checkpoint created before mutations, restorable on failure
- ⏳ `bun test` in `opencode-plugin-aft/` — plugin round-trip test for `aft_move_symbol` (T03)

## Diagnostics

- Run `cargo test move_symbol` to verify the full move pipeline
- Test name → feature mapping: `_basic` (core move), `_multiple_consumers` (rewiring completeness), `_aliased_import` (alias preservation), `_dry_run` (preview mode), `_checkpoint` (safety/rollback), `_not_configured`/`_symbol_not_found`/`_non_top_level`/`_file_not_found` (error codes)
- If consumer rewiring breaks, `move_symbol_multiple_consumers` will show which specific consumer file has wrong import path

## Deviations

- Added `DateHelper` class with `format` method to `tests/fixtures/move_symbol/service.ts` — needed a class method in the fixture for the non-top-level rejection test. This was not in the original T01 fixture set but doesn't break any existing tests.
- Fixed two bugs in `src/commands/move_symbol.rs` during test execution (path resolution + canonicalization). These were genuine handler bugs that the integration tests correctly surfaced.
- Added `project_root()` public getter to `src/callgraph.rs` CallGraph struct — needed for consumer path resolution.
- 9 tests written instead of the planned 8 minimum — added `move_symbol_file_not_found` as a useful extra error path test.

## Known Issues

None.

## Files Created/Modified

- `tests/integration/move_symbol_test.rs` — 9 integration tests (~360 lines)
- `tests/integration/main.rs` — added `mod move_symbol_test;`
- `src/commands/move_symbol.rs` — canonicalize source/dest paths; resolve consumer paths against project root
- `src/callgraph.rs` — added `pub fn project_root(&self) -> &Path` getter
- `tests/fixtures/move_symbol/service.ts` — added DateHelper class with format method for non-top-level test
- `.gsd/milestones/M004/slices/S01/tasks/T02-PLAN.md` — added Observability Impact section
