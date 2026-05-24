---
id: T02
parent: S03
milestone: M002
provides:
  - All 12 mutation commands use write_format_validate (complete migration)
  - 6 format integration tests through binary protocol
  - ruff version guard preventing broken pre-0.1.2 formatters
key_files:
  - src/commands/add_import.rs
  - src/commands/remove_import.rs
  - src/commands/organize_imports.rs
  - src/commands/add_member.rs
  - src/commands/add_derive.rs
  - src/commands/wrap_try_catch.rs
  - src/commands/add_decorator.rs
  - src/commands/add_struct_tags.rs
  - tests/integration/format_test.rs
  - src/format.rs
key_decisions:
  - "D067: ruff_format_available() checks ruff >= 0.1.2 before using ruff format; falls back to black for pre-release ruff versions that output NOT_YET_IMPLEMENTED stubs"
patterns_established:
  - All 12 mutation commands now follow identical tail pattern: auto_backup → edit → write_format_validate → add format fields to response
observability_surfaces:
  - "All 12 mutation responses include formatted (bool) and format_skipped_reason (string) fields"
  - "grep -rn 'fs::write\\|validate_syntax' src/commands/*.rs returns 0 hits — verifies complete migration"
duration: 15min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Complete command migration + format integration tests

**Migrated all 8 remaining M002 mutation commands to write_format_validate, wrote 6 integration tests proving the format pipeline through the binary protocol, and fixed a ruff version guard to prevent broken formatting.**

## What Happened

Replaced the `fs::write` + `validate_syntax` blocks in all 8 remaining command handlers (`add_import`, `remove_import`, `organize_imports`, `add_member`, `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`) with `write_format_validate()` calls. Each handler now includes `formatted` and `format_skipped_reason` in its response JSON.

Wrote 6 integration tests in `tests/integration/format_test.rs`:
- `format_integration_applied_rustfmt` — verifies rustfmt runs and reformats a `.rs` file
- `format_integration_unsupported_language` — verifies `.txt` files get `format_skipped_reason: "unsupported_language"`
- `format_integration_not_found` — verifies missing Python formatter returns `not_found` (conditional on ruff/black not installed)
- `format_integration_add_import_with_format` — verifies `add_import` response includes `formatted` field
- `format_integration_edit_symbol_with_format` — verifies `edit_symbol` response includes `formatted` field
- `format_integration_fields_always_present` — verifies `formatted` is always present, even for unsupported languages

During testing, discovered that ruff 0.0.272 (pre-release) outputs `NOT_YET_IMPLEMENTED_*` stubs, corrupting Python files. Added `ruff_format_available()` that parses ruff's version and requires >= 0.1.2 (when ruff format became stable). Falls back to black when ruff is too old.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 163 unit tests + 101 integration tests, all pass, 0 regressions ✅
- `cargo test -- format_integration` — all 6 format integration tests pass ✅
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — 0 hits ✅

### Slice-level verification status (T02 is task 2 of 3):
- ✅ `cargo build` — 0 warnings
- ✅ `cargo test` — all tests pass, 0 regressions
- ✅ `cargo test -- format` — format module unit tests pass
- ✅ `cargo test -- format_integration` — integration tests pass
- ⬜ `bun test` — plugin updates not yet done (T03)

## Diagnostics

- All 12 mutation commands now emit `[aft] format:` messages on stderr
- All 12 mutation responses include `formatted` (bool) and `format_skipped_reason` (string)
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — 0 hits confirms complete migration

## Deviations

- Added `ruff_format_available()` version guard to `src/format.rs` — not in original plan but necessary to prevent ruff 0.0.x from corrupting Python files. The old `tool_available("ruff")` check was insufficient because pre-release ruff had a broken formatter that exits 0 but outputs stubs.

## Known Issues

None.

## Files Created/Modified

- `src/commands/add_import.rs` — migrated to write_format_validate, added format response fields
- `src/commands/remove_import.rs` — migrated to write_format_validate, added format response fields
- `src/commands/organize_imports.rs` — migrated to write_format_validate, added format response fields
- `src/commands/add_member.rs` — migrated to write_format_validate, added format response fields
- `src/commands/add_derive.rs` — migrated to write_format_validate, added format response fields
- `src/commands/wrap_try_catch.rs` — migrated to write_format_validate, added format response fields
- `src/commands/add_decorator.rs` — migrated to write_format_validate, added format response fields
- `src/commands/add_struct_tags.rs` — migrated to write_format_validate, added format response fields
- `src/format.rs` — added ruff_format_available() version guard, updated detect_formatter for Python
- `tests/integration/format_test.rs` — new: 6 format integration tests through binary protocol
- `tests/integration/main.rs` — registered format_test module
- `.gsd/milestones/M002/slices/S03/tasks/T02-PLAN.md` — added Observability Impact section
