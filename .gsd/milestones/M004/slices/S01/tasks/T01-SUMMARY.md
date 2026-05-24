---
id: T01
parent: S01
milestone: M004
provides:
  - handle_move_symbol command handler with full mutation pipeline
  - compute_relative_import_path utility for import path computation
  - Consumer import rewriting with alias preservation
  - Multi-file fixture set for integration tests
key_files:
  - src/commands/move_symbol.rs
  - src/commands/mod.rs
  - src/main.rs
  - tests/fixtures/move_symbol/
key_decisions:
  - Reverse-order import editing (process matching imports from end to start) to maintain valid byte offsets through multi-edit passes
  - Checkpoint before any mutation; rollback restores checkpoint + deletes newly-created files
  - Import path matching resolves extensionless imports by trying .ts/.tsx/.js/.jsx suffixes and index patterns
  - TS/JS/TSX scoped for import rewriting (Python/Rust/Go import rewriting deferred as out-of-scope for this slice)
patterns_established:
  - move_symbol follows the same handle_*(req, ctx) → Response pattern as all other commands
  - Rollback pattern: checkpoint create → apply mutations → on failure restore checkpoint + cleanup new files (similar to transaction.rs but using CheckpointStore)
  - compute_relative_import_path as a pure function with comprehensive unit tests
observability_surfaces:
  - Response includes files_modified, consumers_updated, checkpoint_name
  - Stderr log: [aft] move_symbol: {symbol} from {source} to {dest} ({N} consumers updated)
  - Error response includes failed_file and rolled_back array
  - Checkpoint visible via list_checkpoints command
duration: ~25min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Implement move_symbol command handler with relative path computation

**Built complete `move_symbol` command handler with relative path computation, consumer import rewriting with alias preservation, checkpoint-based rollback, dry-run support, and 8-file fixture set.**

## What Happened

Created `src/commands/move_symbol.rs` (~500 lines) implementing the full move_symbol flow:
1. Parameter validation (file, symbol, destination, optional scope/dry_run)
2. Call graph guard (returns `not_configured` when graph absent)
3. Symbol resolution with disambiguation (same pattern as edit_symbol)
4. Top-level guard: rejects methods and scoped symbols
5. Symbol text extraction and export prefix handling
6. Source file mutation (symbol removal with whitespace cleanup)
7. Destination file mutation (symbol appended with export)
8. Consumer discovery via `callers_of` and import rewriting
9. Relative path computation (pure function, unit-tested)
10. Import alias preservation (`{ X as Y }` pattern detected and maintained)
11. Dry-run mode returning multi-file diffs without disk writes
12. Checkpoint-based rollback on any write failure

Wired into dispatch (`src/main.rs`) and module registry (`src/commands/mod.rs`).

Created fixture set in `tests/fixtures/move_symbol/` with 8 files:
- `service.ts` — source with 2 exported functions + 1 exported const
- `utils.ts` — destination with existing content
- `consumer_a.ts` — imports only the moved symbol
- `consumer_b.ts` — imports both moved and non-moved symbols (split import test)
- `consumer_c.ts` — aliased import (`formatDate as fmtDate`)
- `consumer_d.ts` — imports unrelated symbol only (should not be modified)
- `consumer_f.ts` — imports unrelated symbol only (second no-change case)
- `features/consumer_e.ts` — subdirectory consumer (tests `../` path computation)

19 unit tests covering: relative path computation (6 cases), import path matching (4 cases), symbol text manipulation (5 cases), path normalization (2 cases), alias extraction (2 cases).

## Verification

- `cargo build` — compiles without errors or warnings ✅
- `cargo test --lib` — 242 tests pass (0 failures, including 19 new move_symbol tests) ✅
- `cargo test move_symbol` — all 19 move_symbol-specific tests pass ✅
- Dispatch wiring confirmed: `grep "move_symbol" src/main.rs` shows match arm ✅
- Fixture files syntactically valid (8 .ts files, manually verified content) ✅

### Slice-level verification status (T01 is task 1 of 2):
- Basic move: ⏳ (needs integration test in T02)
- 5+ consumer files rewired: ⏳ (fixture set ready, needs integration test)
- Aliased import preserved: ⏳ (logic implemented + unit tested, needs integration test)
- Dry-run returns diffs: ⏳ (logic implemented, needs integration test)
- not_configured error: ⏳ (guard implemented, needs integration test)
- symbol_not_found error: ⏳ (guard implemented, needs integration test)
- Method/class member rejected: ⏳ (guard implemented, needs integration test)
- Checkpoint created/restorable: ⏳ (logic implemented, needs integration test)
- Plugin round-trip test: ⏳ (T02 scope)

## Diagnostics

- Response JSON includes `files_modified`, `consumers_updated`, `checkpoint_name` for post-move inspection
- `list_checkpoints` command shows `move_symbol:{name}` checkpoint
- `callers` command can verify consumer list pre/post move
- On failure: error includes `failed_file` and `rolled_back` array listing restored/deleted files
- Stderr: `[aft] move_symbol: ...` on success, `[aft] move_symbol failed: ...` with rollback count on failure

## Deviations

None. Implementation follows the task plan exactly.

## Known Issues

- Import rewriting is scoped to TS/JS/TSX. Python/Rust/Go consumer rewriting returns `None` (no-op). This matches the slice scope which targets TS/JS workflows.
- Consumer file paths from `callers_of` are relative to project root. In integration tests, path resolution needs to account for this (may require canonicalization in test setup).

## Files Created/Modified

- `src/commands/move_symbol.rs` — new: complete command handler (~500 lines) with 19 unit tests
- `src/commands/mod.rs` — modified: added `pub mod move_symbol;`
- `src/main.rs` — modified: added `"move_symbol"` dispatch entry
- `tests/fixtures/move_symbol/service.ts` — new: source file with exportable symbols
- `tests/fixtures/move_symbol/utils.ts` — new: destination file with existing content
- `tests/fixtures/move_symbol/consumer_a.ts` — new: single-import consumer
- `tests/fixtures/move_symbol/consumer_b.ts` — new: multi-import consumer (split test)
- `tests/fixtures/move_symbol/consumer_c.ts` — new: aliased import consumer
- `tests/fixtures/move_symbol/consumer_d.ts` — new: unrelated import consumer (no-change)
- `tests/fixtures/move_symbol/consumer_f.ts` — new: unrelated import consumer (no-change)
- `tests/fixtures/move_symbol/features/consumer_e.ts` — new: subdirectory consumer (../ path)
- `.gsd/milestones/M004/slices/S01/tasks/T01-PLAN.md` — modified: added Observability Impact section
