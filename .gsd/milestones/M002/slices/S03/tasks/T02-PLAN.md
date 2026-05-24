---
estimated_steps: 4
estimated_files: 10
---

# T02: Complete command migration + format integration tests

**Slice:** S03 — Auto-format & Validation
**Milestone:** M002

## Description

Update the remaining 8 M002 mutation commands to use `write_format_validate()` (identical mechanical substitution from T01) and write comprehensive integration tests proving the auto-format pipeline works through the binary protocol. Each command handler change is the same ~10-line replacement: remove `fs::write` + `validate_syntax` block, replace with `write_format_validate()` call, add `formatted`/`format_skipped_reason` to response.

## Steps

1. Update all 8 M002 command handlers to use `write_format_validate()`:
   - `add_import.rs` — replace lines 174-189 (fs::write + validate_syntax + syntax_valid match) with write_format_validate call, add format fields to response
   - `remove_import.rs` — same substitution
   - `organize_imports.rs` — same substitution
   - `add_member.rs` — same substitution
   - `add_derive.rs` — same substitution
   - `wrap_try_catch.rs` — same substitution
   - `add_decorator.rs` — same substitution
   - `add_struct_tags.rs` — same substitution
   Each handler: replace `fs::write` error handling + `validate_syntax` match block with `write_format_validate(path, &new_source, ctx.config())?`, then build response from WriteResult fields.

2. Register `format_test` module in `tests/integration/main.rs`.

3. Write `tests/integration/format_test.rs` with integration tests:
   - **format_applied_rustfmt**: write a `.rs` file with poor formatting (e.g., `fn  main( ){  }`) → send `write` command → verify response has `formatted: true` and file content is properly formatted. Skip test if `rustfmt` not on PATH.
   - **format_not_found**: write a `.py` file → send `write` command → if neither ruff nor black is on PATH, verify `formatted: false` with `format_skipped_reason: "not_found"`. (Use a helper to check formatter availability.)
   - **format_unsupported_language**: write a `.txt` file (or `.md`) → send `write` command → verify `formatted: false` with `format_skipped_reason: "unsupported_language"`.
   - **add_import_with_format**: create a `.rs` fixture, send `add_import` → verify response has `formatted` field and import is correctly placed.
   - **edit_symbol_with_format**: create a `.rs` fixture, send `edit_symbol` to replace a function → verify `formatted` field in response.
   - **format_fields_always_present**: verify that even for unsupported languages, the `formatted` field is always present in mutation responses (never omitted).

4. Verify all existing tests still pass — no regressions from the command handler updates.

## Must-Haves

- [ ] All 8 M002 commands use write_format_validate (no remaining fs::write + validate_syntax patterns in any mutation command)
- [ ] All 8 commands include `formatted` and `format_skipped_reason` in response JSON
- [ ] Integration test proves formatter runs on .rs files (when rustfmt available)
- [ ] Integration test proves graceful degradation when formatter not found
- [ ] Integration test proves unsupported language path
- [ ] Zero regressions in existing tests

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all tests pass including existing S01/S02 integration tests
- `cargo test -- format_integration` — new format integration tests pass
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — only appears in non-mutation commands (if any), never in the 12 mutation handlers

## Observability Impact

- **Response JSON fields**: All 12 mutation commands (4 from T01 + 8 from this task) now include `formatted` (bool) and `format_skipped_reason` (string) in every response. A future agent can verify format pipeline health by checking these fields on any mutation response.
- **stderr diagnostics**: No new stderr signals — T01's `[aft] format:` messages cover all commands since they flow through the shared `write_format_validate()` pipeline.
- **Integration test signals**: New `format_test` integration tests exercise the binary protocol end-to-end. `cargo test -- format_integration` proves the pipeline works through the real binary, not just unit-level.
- **Failure inspection**: If a command migration is incomplete, `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` will find remaining un-migrated handlers. Zero hits in mutation commands = migration complete.

## Inputs

- `src/edit.rs` — `write_format_validate()` and `WriteResult` from T01
- `src/format.rs` — auto_format infrastructure from T01
- `src/commands/add_import.rs` through `add_struct_tags.rs` — 8 command handlers with the old fs::write + validate_syntax pattern
- `tests/integration/helpers.rs` — AftProcess test helper

## Expected Output

- 8 command files updated (each ~10 lines shorter, using write_format_validate)
- `tests/integration/format_test.rs` — 5-7 integration tests proving format pipeline through binary protocol
- `tests/integration/main.rs` — format_test module registered
