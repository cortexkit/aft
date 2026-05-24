---
id: T01
parent: S04
milestone: M002
provides:
  - dry-run infrastructure (DryRunResult, dry_run_diff, validate_syntax_str, is_dry_run)
  - dry_run: true support on all 12 mutation command handlers
  - 8 integration tests covering dry-run across write, edit_symbol, edit_match, batch, add_import, backup avoidance, syntax validation, and empty diff
key_files:
  - src/edit.rs
  - tests/integration/dryrun_test.rs
key_decisions:
  - Used `similar::TextDiff::from_lines` with 3-line context radius and a/b path prefixes for standard unified diff output
  - For handlers where auto_backup preceded new_source computation, wrapped backup in `if !is_dry_run` conditional to avoid creating backup entries during dry-run
  - batch dry-run clones the original source before applying edits in-memory to preserve both original and proposed for diff
patterns_established:
  - Dry-run early return pattern: after computing new_source, check `edit::is_dry_run(&req.params)`, return `{ ok, dry_run, diff, syntax_valid }` — consistent 5-line insertion across all handlers
  - `validate_syntax_str` pattern for in-memory syntax checking without disk I/O
observability_surfaces:
  - dry-run response includes `diff` (unified diff) and `syntax_valid` for proposed content inspection
  - syntax_valid: false signals broken syntax before any disk write
duration: 30m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Add dry-run infrastructure and wire into all 12 mutation handlers

**Added `similar` crate, created shared dry-run infrastructure in `edit.rs`, and wired `dry_run: true` support into all 12 mutation handlers with 8 integration tests.**

## What Happened

1. Added `similar = "2"` to `Cargo.toml` for unified diff generation.
2. Added four new functions/types to `src/edit.rs`:
   - `validate_syntax_str(content, path)` — in-memory syntax validation via tree-sitter without touching disk
   - `DryRunResult { diff, syntax_valid }` — return type for dry-run computations
   - `dry_run_diff(original, proposed, path)` — generates unified diff with `a/`/`b/` prefixes and 3-line context, plus syntax validation
   - `is_dry_run(params)` — extracts boolean from request params
3. Wired dry-run into all 12 mutation handlers: `write`, `edit_symbol`, `edit_match`, `batch`, `add_import`, `remove_import`, `organize_imports`, `add_member`, `add_derive`, `add_decorator`, `add_struct_tags`, `wrap_try_catch`. Each handler checks `is_dry_run` and early-returns with diff, skipping both `auto_backup` and `write_format_validate`.
4. For handlers where `auto_backup` preceded `new_source` computation (8 of 12), wrapped backup in conditional to prevent backup creation during dry-run.
5. For `batch`, cloned original source before applying edits to produce combined diff of all edits.
6. For `write`, added explicit read of original file content to produce meaningful diff.
7. Wrote 8 integration tests in `tests/integration/dryrun_test.rs` covering write, edit_symbol (milestone acceptance), edit_match, batch (combined diff), add_import, no-backup verification, syntax validation, and empty-diff no-op.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 175 unit tests + 113 integration tests all pass ✅
- `edit_symbol_dry_run` test verifies: file unchanged on disk after dry-run (milestone acceptance criterion) ✅
- `dry_run_no_backup` test verifies: no backup entries created via `edit_history` after dry-run ✅
- `dry_run_syntax_validation` test verifies: `syntax_valid: false` for broken syntax ✅
- `dry_run_empty_diff` test verifies: no-op produces empty diff ✅

## Diagnostics

- Send any mutation command with `"dry_run": true` to get `{ ok, dry_run, diff, syntax_valid }` without side effects.
- `diff` field contains standard unified diff — parseable by `patch`, `git apply`, or agent diff viewers.
- `syntax_valid` is `null` for unsupported languages, `true`/`false` for supported ones.
- Empty `diff` string indicates the edit would be a no-op.

## Deviations

- For 8 handlers where `auto_backup` was called before `new_source` computation, wrapped backup in conditional rather than reordering the calls. This preserves existing error handling flow while satisfying the no-backup-on-dry-run requirement.
- `batch.rs` required `source.clone()` before the move into `content` to preserve original for diff computation.

## Known Issues

None.

## Files Created/Modified

- `Cargo.toml` — added `similar = "2"` dependency
- `src/edit.rs` — added `validate_syntax_str`, `DryRunResult`, `dry_run_diff`, `is_dry_run`; updated imports
- `src/commands/write.rs` — dry-run early return with original content read
- `src/commands/edit_symbol.rs` — dry-run early return between new_source and auto_backup
- `src/commands/edit_match.rs` — conditional backup + dry-run early return
- `src/commands/batch.rs` — conditional backup + source clone + dry-run early return
- `src/commands/add_import.rs` — conditional backup + dry-run early return
- `src/commands/remove_import.rs` — conditional backup + dry-run early return
- `src/commands/organize_imports.rs` — conditional backup + dry-run early return
- `src/commands/add_member.rs` — conditional backup + dry-run early return
- `src/commands/add_derive.rs` — dry-run early return between new_source and auto_backup
- `src/commands/add_decorator.rs` — conditional backup + dry-run early return
- `src/commands/add_struct_tags.rs` — dry-run early return between new_source and auto_backup
- `src/commands/wrap_try_catch.rs` — conditional backup + dry-run early return
- `tests/integration/dryrun_test.rs` — 8 integration tests (created)
- `tests/integration/main.rs` — registered `dryrun_test` module
- `.gsd/milestones/M002/slices/S04/tasks/T01-PLAN.md` — added Observability Impact section
