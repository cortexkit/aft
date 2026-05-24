---
id: S01
parent: M004
milestone: M004
provides:
  - move_symbol command handler with full multi-file mutation pipeline
  - compute_relative_import_path utility for TS/JS/TSX import path rewriting
  - Consumer import rewriting with alias preservation via callers_of + import scan
  - Checkpoint-based rollback for multi-file safety
  - Dry-run mode returning multi-file diffs without disk writes
  - Plugin tool aft_move_symbol with Zod schema in refactoring.ts tool group
  - 9 integration tests proving success/dry-run/checkpoint/error paths through binary protocol
  - 19 unit tests for path computation, import matching, symbol text manipulation, alias extraction
requires:
  - slice: M003/S01
    provides: CallGraph::callers_of() for consumer discovery, configure command for project_root
  - slice: M002/S01
    provides: imports::parse_imports(), find_insertion_point(), generate_import_line(), is_duplicate()
  - slice: M002/S03
    provides: edit::write_format_validate() for mutation tail
  - slice: M001/S04
    provides: CheckpointStore::create() for pre-operation safety
affects:
  - M004/S02 (extract_function and inline_symbol will reuse multi-file mutation patterns)
  - M004/S03 (LSP-enhanced resolution will enrich move_symbol's symbol resolution)
key_files:
  - src/commands/move_symbol.rs
  - src/commands/mod.rs
  - src/main.rs
  - src/callgraph.rs (added project_root() getter)
  - tests/integration/move_symbol_test.rs
  - tests/fixtures/move_symbol/ (8 fixture files)
  - opencode-plugin-aft/src/tools/refactoring.ts
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
key_decisions:
  - D106: Relative path strips TS/JS/TSX extensions for idiomatic imports
  - D109: Reverse-order import editing to maintain valid byte offsets through multi-edit passes
  - D110: Import rewriting scoped to TS/JS/TSX (Python/Rust/Go deferred)
  - D111: Canonicalize source/dest paths in handler to match callgraph's canonical internal paths
  - D108: Plugin refactoring.ts tool group for aft_move_symbol, extensible for S02
patterns_established:
  - move_symbol follows handle_*(req, ctx) → Response pattern (D026)
  - Rollback pattern: checkpoint create → apply mutations → on failure restore checkpoint + cleanup new files
  - compute_relative_import_path as pure function with comprehensive unit tests
  - refactoringTools(bridge) factory follows same pattern as navigationTools, readingTools, etc.
observability_surfaces:
  - Response includes files_modified, consumers_updated, checkpoint_name
  - Stderr log: [aft] move_symbol: {symbol} from {source} to {dest} ({N} consumers updated)
  - Error response includes failed_file and rolled_back array
  - Checkpoint visible via list_checkpoints command
drill_down_paths:
  - .gsd/milestones/M004/slices/S01/tasks/T01-SUMMARY.md
  - .gsd/milestones/M004/slices/S01/tasks/T02-SUMMARY.md
  - .gsd/milestones/M004/slices/S01/tasks/T03-SUMMARY.md
duration: ~70min across 3 tasks
verification_result: passed
completed_at: 2026-03-14
---

# S01: Move Symbol with Import Rewiring

**Single-call `move_symbol` command that moves a top-level symbol between files and rewrites all consumer imports across the workspace, with checkpoint safety, dry-run preview, and alias preservation — verified by 28 Rust tests + 40 plugin tests.**

## What Happened

**T01** built the core `handle_move_symbol` handler (~500 lines) in `src/commands/move_symbol.rs`. The flow: param validation → call graph guard → symbol resolution with top-level check (D100) → symbol text extraction → auto-checkpoint (D105) → source file mutation (remove symbol, clean whitespace) → destination file mutation (append with export) → consumer discovery via `callers_of` → import path rewriting with alias preservation → all writes through `write_format_validate`. Dry-run mode computes diffs without disk writes. Rollback on failure restores checkpoint and deletes newly-created files. Created an 8-file fixture set with consumers at different directory depths, aliased imports, and split imports. 19 unit tests cover relative path computation, import matching, alias extraction, and text manipulation.

**T02** wrote 9 integration tests exercising the full binary protocol: basic move, 5+ consumer rewiring, aliased import preservation, dry-run, checkpoint create/restore, and 4 error paths (not_configured, symbol_not_found, non-top-level rejection, file_not_found). Tests surfaced two real bugs: (1) `callers_of` returns relative paths that needed resolving against `project_root()`, and (2) macOS `/var` → `/private/var` symlink caused path canonicalization mismatches. Both fixed in the handler.

**T03** created `refactoring.ts` tool group with `aft_move_symbol` tool definition (Zod schema for file, symbol, destination, scope, dry_run params), registered in `index.ts`, and added a bun test proving full plugin → binary → response round-trip with on-disk file verification.

## Verification

- `cargo test move_symbol` — 28 tests pass (19 unit + 9 integration) ✅
- `cargo test` — 396 tests pass (242 unit + 154 integration), zero failures, zero regressions ✅
- `bun test` in `opencode-plugin-aft/` — 40/40 pass including move_symbol round-trip ✅

Slice plan checklist:
- ✅ Basic move: symbol removed from source, added to destination, consumer imports updated
- ✅ 5+ consumer files all rewired correctly (different directory depths)
- ✅ Aliased import preserved after rewiring (`import { formatDate as fmtDate }` keeps alias)
- ✅ Dry-run returns multi-file diffs, files unchanged on disk
- ✅ `not_configured` error when call graph not initialized
- ✅ `symbol_not_found` error for nonexistent symbol
- ✅ Method/class member rejected with appropriate error (D100)
- ✅ Checkpoint created before mutations, restorable on failure
- ✅ Plugin round-trip test passes for `aft_move_symbol`

## Requirements Advanced

- R028 (Move symbol with import rewiring) — fully implemented and verified: single-call move with multi-file import rewriting, aliased import preservation, checkpoint safety, dry-run preview

## Requirements Validated

- R028 — 28 Rust tests + 1 plugin round-trip prove move_symbol across 5+ consumer files including aliased imports, dry-run, checkpoint/rollback, and error paths through binary protocol

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- Added `DateHelper` class to `service.ts` fixture for non-top-level rejection test (not in original T01 fixture plan, but needed for T02's test case).
- Fixed two path resolution bugs in the handler during T02 (relative CallerGroup.file paths; macOS canonicalization mismatch). These were genuine bugs surfaced by integration tests, not plan deviations.
- Added `project_root()` pub getter to `CallGraph` struct — not planned but required for consumer path resolution.
- 9 integration tests written instead of planned 8 minimum (added file_not_found error path).

## Known Limitations

- Import rewriting is scoped to TS/JS/TSX only (D110). Python/Rust/Go consumers are no-ops. This matches the web-first priority (D004).
- `require()` calls and barrel re-exports are not rewritten. Consumer discovery relies on ES import syntax.
- Symbol's own internal imports (imports the symbol itself depends on) are not transferred to the destination file. The agent may need to manually add them.

## Follow-ups

- S02 will add `aft_extract_function` and `aft_inline_symbol` to the `refactoring.ts` tool group created here.
- S03 will enhance move_symbol's symbol resolution with LSP workspace symbol data.

## Files Created/Modified

- `src/commands/move_symbol.rs` — new: complete command handler (~500 lines) with 19 unit tests
- `src/commands/mod.rs` — modified: added `pub mod move_symbol;`
- `src/main.rs` — modified: added `"move_symbol"` dispatch entry
- `src/callgraph.rs` — modified: added `pub fn project_root()` getter
- `tests/integration/move_symbol_test.rs` — new: 9 integration tests (~360 lines)
- `tests/integration/main.rs` — modified: added `mod move_symbol_test;`
- `tests/fixtures/move_symbol/` — new: 8 fixture files (service.ts, utils.ts, consumer_a-f.ts, features/consumer_e.ts)
- `opencode-plugin-aft/src/tools/refactoring.ts` — new: aft_move_symbol tool with Zod schema
- `opencode-plugin-aft/src/index.ts` — modified: registered refactoringTools
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — modified: added move_symbol round-trip test

## Forward Intelligence

### What the next slice should know
- `refactoring.ts` tool group is ready for S02's `aft_extract_function` and `aft_inline_symbol` — just add tool definitions and import the factory.
- The multi-file mutation pattern (checkpoint → mutate N files → rollback on failure) is proven and can be reused for extract_function if it modifies the original file + creates/updates a destination.
- `write_format_validate` is the mandatory mutation tail for all file modifications.

### What's fragile
- Import path matching (`import_path_matches_file`) uses heuristic extension stripping (`.ts`, `.tsx`, `.js`, `.jsx`) and `index` pattern matching. Unusual import conventions (explicit `.mjs` extensions, path aliases like `@/`) will miss matches.
- Consumer discovery combines `callers_of` (call sites) with the import scan. If the call graph is stale (file watcher hasn't drained), consumers could be missed.

### Authoritative diagnostics
- `cargo test move_symbol` — 28 tests covering all success and error paths. If this passes, the command works.
- `bun test` in `opencode-plugin-aft/` — plugin registration and round-trip verified. If this passes, agents can access the tool.
- Response JSON `consumers_updated` count — quick indicator of rewiring completeness.

### What assumptions changed
- Assumed `callers_of` returns absolute paths — actually returns paths relative to project root. Added `project_root()` getter and resolution logic.
- Assumed temp dir paths are canonical on macOS — `/var/folders` vs `/private/var/folders` mismatch required explicit canonicalization in the handler (D111).
