---
estimated_steps: 7
estimated_files: 16
---

# T01: Add dry-run infrastructure and wire into all 12 mutation handlers

**Slice:** S04 — Dry-run & Transactions
**Milestone:** M002

## Description

Add the `similar` crate for unified diff generation, create shared dry-run infrastructure in `edit.rs`, then wire `dry_run: true` support into all 12 mutation command handlers. Each handler gains ~5 lines: extract the dry_run bool, and early-return with a diff after computing the proposed content — skipping auto_backup and write_format_validate entirely.

## Steps

1. Add `similar = "2"` to `Cargo.toml` dependencies
2. In `src/edit.rs`: add `validate_syntax_str(content: &str, path: &Path) -> Option<bool>` — uses `detect_language(path)` + `grammar_for(lang)` + `parser.parse(content.as_bytes(), None)` to validate syntax of an in-memory string without touching disk. Returns `None` for unsupported languages, `Some(true/false)` for valid/invalid.
3. In `src/edit.rs`: add `DryRunResult { diff: String, syntax_valid: Option<bool> }` struct and `dry_run_diff(original: &str, proposed: &str, path: &Path) -> DryRunResult` function — uses `similar::TextDiff::from_lines(original, proposed).unified_diff().context_radius(3).header(&format!("a/{}", display_path), &format!("b/{}", display_path))` to generate unified diff, calls `validate_syntax_str` for syntax check.
4. In `src/edit.rs`: add a helper `is_dry_run(params: &serde_json::Value) -> bool` that extracts `dry_run` from request params (same pattern as `create_dirs`, `type_only`, etc.)
5. Wire dry-run into all 12 mutation handlers. In each handler, after computing `new_source` (the mutation result) and before `auto_backup`/`write_format_validate`: check `is_dry_run(&req.params)`, if true call `dry_run_diff(&source, &new_source, path)` and return early with `{ ok: true, id, dry_run: true, diff, syntax_valid }`. The 12 handlers are: write, edit_symbol, edit_match, batch, add_import, remove_import, organize_imports, add_member, add_derive, add_decorator, add_struct_tags, wrap_try_catch. For batch: dry-run uses the final combined content after all edits applied in memory.
6. Register `dryrun_test` module in `tests/integration/main.rs`
7. Write `tests/integration/dryrun_test.rs` with integration tests:
   - `write_dry_run_returns_diff` — write with dry_run:true returns unified diff, file content unchanged
   - `edit_symbol_dry_run` — edit_symbol replace with dry_run returns diff, file unchanged (milestone acceptance scenario)
   - `edit_match_dry_run` — edit_match with dry_run returns diff
   - `batch_dry_run` — batch with multiple edits returns combined diff
   - `add_import_dry_run` — add_import with dry_run returns diff
   - `dry_run_no_backup` — dry_run does not create a backup entry (verify via edit_history)
   - `dry_run_syntax_validation` — dry_run with intentionally broken syntax returns syntax_valid:false
   - `dry_run_empty_diff` — dry_run for a no-op (e.g., add_import for already-present import) returns empty diff

## Must-Haves

- [ ] `similar` crate added to Cargo.toml
- [ ] `validate_syntax_str` parses in-memory string, never touches disk
- [ ] `dry_run_diff` produces standard unified diff with `a/` and `b/` path prefixes
- [ ] All 12 mutation handlers check `dry_run` param and early-return with diff
- [ ] Dry-run skips `auto_backup` — no backup entries created
- [ ] Dry-run skips `write_format_validate` — no disk writes
- [ ] Batch dry-run returns combined diff of all edits
- [ ] Integration tests pass for representative commands

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all existing tests pass + new dryrun_test tests pass
- Manually verify: `edit_symbol` dry-run test proves file unchanged on disk (milestone acceptance criterion)

## Observability Impact

- **New response fields:** All 12 mutation commands now include `dry_run: true`, `diff`, and `syntax_valid` in responses when `dry_run: true` is passed. Agents can inspect the diff before committing.
- **Syntax pre-validation:** `validate_syntax_str` provides in-memory syntax checking without disk writes — future agent can verify proposed content validity before applying.
- **Diff inspection:** `dry_run_diff` produces standard unified diff format parseable by any diff tool or agent. Empty diff indicates a no-op edit.
- **Failure visibility:** `syntax_valid: false` in dry-run response signals that the proposed edit would break syntax, allowing agents to correct before applying.

## Inputs

- `src/edit.rs` — shared edit infrastructure (write_format_validate, auto_backup, validate_syntax)
- `src/parser.rs` — `detect_language()`, `grammar_for()` (both pub) for creating parsers
- All 12 command handlers in `src/commands/` — each follows the pattern: extract params → read source → compute new_source → auto_backup → write_format_validate → build response
- `tests/integration/helpers.rs` — AftProcess test helper

## Expected Output

- `Cargo.toml` — `similar` dependency added
- `src/edit.rs` — `DryRunResult`, `validate_syntax_str()`, `dry_run_diff()`, `is_dry_run()` added
- 12 command handler files — each gains dry-run early return (~5 lines each)
- `tests/integration/dryrun_test.rs` — 8 integration tests
- `tests/integration/main.rs` — `dryrun_test` module registered
