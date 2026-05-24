---
estimated_steps: 4
estimated_files: 2
---

# T02: Integration tests for move_symbol through binary protocol

**Slice:** S01 — Move Symbol with Import Rewiring
**Milestone:** M004

## Description

Write integration tests that exercise `move_symbol` end-to-end through the binary protocol using `AftProcess`. Tests use the temp dir + fixture copy pattern (established in callgraph watcher tests) to create isolated environments where move_symbol can mutate files and we verify the results by reading file contents and checking import statements.

This task retires the key risk from the roadmap: "Import rewiring completeness — move_symbol must find ALL files that import the moved symbol." The tests verify that 5+ consumer files are all correctly updated, including an aliased import case.

## Steps

1. **Create `tests/integration/move_symbol_test.rs`** with a `setup_move_fixture()` helper that copies the `tests/fixtures/move_symbol/` directory into a temp dir and returns `(TempDir, root_path)`. Register the module in `tests/integration/main.rs`.

2. **Write core success tests.** (a) `move_symbol_basic`: configure → move a function from source to destination → verify source file no longer contains the function → verify destination file contains the function with export → verify all consumer files import from the destination path instead of the source path. (b) `move_symbol_multiple_consumers`: same as basic but explicitly assert on all 5+ consumer files, checking that each has the correct relative import path to the destination. (c) `move_symbol_aliased_import`: verify that a consumer using `import { X as Y }` has the alias preserved after the move (only the module path changes).

3. **Write dry_run and safety tests.** (a) `move_symbol_dry_run`: send with `dry_run: true` → verify response contains diffs for all affected files → verify no files were modified on disk. (b) `move_symbol_checkpoint`: after a successful move, verify `list_checkpoints` shows the auto-created checkpoint → restore it → verify all files are back to their original state.

4. **Write error path tests.** (a) `move_symbol_not_configured`: send move_symbol without prior configure → expect `not_configured` error. (b) `move_symbol_symbol_not_found`: reference a nonexistent symbol → expect `symbol_not_found` error. (c) `move_symbol_non_top_level`: reference a method inside a class → expect error rejecting non-top-level symbols.

## Must-Haves

- [ ] 8+ integration tests covering success, dry_run, checkpoint, and error paths
- [ ] At least 5 consumer files verified with correct import paths after move
- [ ] Aliased import test verifies alias preservation
- [ ] Dry-run test verifies no files modified on disk
- [ ] Checkpoint test verifies auto-creation and restorability
- [ ] Error tests verify `not_configured`, `symbol_not_found`, and non-top-level rejection
- [ ] All tests use temp dir isolation (no fixture file mutation)

## Verification

- `cargo test move_symbol` — all tests pass
- `cargo test` — no regressions (existing 400+ tests still pass)

## Inputs

- `tests/fixtures/move_symbol/` — fixture files created in T01
- `tests/integration/helpers.rs` — `AftProcess`, `fixture_path`
- `tests/integration/callgraph_test.rs` — reference for `setup_watcher_fixture()` temp dir pattern
- `src/commands/move_symbol.rs` — the command handler from T01

## Expected Output

- `tests/integration/move_symbol_test.rs` — 8+ integration tests (~400-500 lines)
- `tests/integration/main.rs` — updated with `mod move_symbol_test;`

## Observability Impact

- **Test-time signals:** Each test verifies specific response fields (`files_modified`, `consumers_updated`, `checkpoint_name`, `diffs`, `code`) that constitute the move_symbol command's diagnostic surface.
- **Inspection:** Tests exercise `list_checkpoints` → `restore_checkpoint` round-trip, verifying the checkpoint subsystem is wired correctly for move operations.
- **Failure visibility:** Error path tests confirm that `not_configured`, `symbol_not_found`, and `invalid_request` (non-top-level) codes are emitted correctly — a future agent can match on these codes to diagnose failures.
- **Future agent use:** Run `cargo test move_symbol` to verify the entire move pipeline. A failing test name tells you exactly which sub-feature broke (basic move, consumer rewiring, alias preservation, dry-run, checkpoint, or error handling).
