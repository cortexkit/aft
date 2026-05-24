---
id: S03
parent: M002
milestone: M002
provides:
  - External tool runner with subprocess timeout/kill/not-found handling
  - Formatter detection per language (prettier, ruff/black, rustfmt, gofmt) with PATH-based availability checking
  - auto_format() function integrating detection → invocation → result reporting
  - WriteResult struct and write_format_validate() shared pipeline replacing fs::write + validate_syntax across all 12 mutation commands
  - validate_full() type-checker invocation (tsc, pyright, cargo check, go vet) with per-checker output parsing
  - ValidationError struct (line/column/message/severity) for structured error reporting
  - Config extended with formatter_timeout_secs (10s) and type_checker_timeout_secs (30s)
  - All 12 plugin tool definitions expose optional validate param and document format/validation response fields
requires:
  - slice: S01
    provides: Import management commands (add_import, remove_import, organize_imports) — verified with auto-format integrated
  - slice: S02
    provides: Compound operation commands (add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags) — migrated to write_format_validate
affects:
  - S04 (dry-run and transactions will inherit auto-format through write_format_validate pipeline)
key_files:
  - src/format.rs
  - src/edit.rs
  - src/config.rs
  - src/commands/*.rs (all 12 mutation commands)
  - opencode-plugin-aft/src/tools/editing.ts
  - opencode-plugin-aft/src/tools/imports.ts
  - opencode-plugin-aft/src/tools/structure.ts
  - tests/integration/format_test.rs
key_decisions:
  - "D063: Formatter selection priority per language — prettier, ruff/black, rustfmt, gofmt"
  - "D066: WriteResult struct centralizes mutation tail — write_format_validate replaces per-command fs::write + validate_syntax"
  - "D067: ruff version guard requires >= 0.1.2 to prevent NOT_YET_IMPLEMENTED stub corruption"
  - "D068: run_external_tool_capture() treats non-zero exit as OK — type checkers signal errors-found via exit code"
  - "D069: validation_errors included only when validate:'full' requested — keeps default responses lean"
  - "D070: Per-checker output format flags — cargo --message-format=json, tsc --pretty false, pyright --outputjson"
patterns_established:
  - "Mutation command tail: auto_backup → edit → write_format_validate(path, content, config, params) → add format+validation fields to response"
  - "Skip reason strings: not_found, timeout, error, unsupported_language — used by both format and validate"
  - "Params passed as &serde_json::Value to pipeline — allows future params (validate, dry_run) without touching call sites"
observability_surfaces:
  - "stderr: [aft] format: {file} ({formatter}) / (skipped: {reason})"
  - "stderr: [aft] validate: {file} ({checker}, {N} errors) / (skipped: {reason})"
  - "response: formatted (bool), format_skipped_reason (string), validation_errors (array), validate_skipped_reason (string)"
drill_down_paths:
  - .gsd/milestones/M002/slices/S03/tasks/T01-SUMMARY.md
  - .gsd/milestones/M002/slices/S03/tasks/T02-SUMMARY.md
  - .gsd/milestones/M002/slices/S03/tasks/T03-SUMMARY.md
duration: 3 tasks
verification_result: passed
completed_at: 2026-03-14
---

# S03: Auto-format & Validation

**Every mutation command auto-formats via the project's formatter when available, with opt-in type-checker validation and structured error reporting — proven through binary protocol with formatter-found, formatter-not-found, and validation paths.**

## What Happened

Built three layers of external tool integration and wired them into all 12 mutation commands.

**T01 — Infrastructure + core migration.** Created `src/format.rs` (~280 lines) with the subprocess runner (`run_external_tool` — spawn, poll try_wait at 50ms, kill on timeout, handle NotFound), formatter detection per language (D063), and `auto_format()` orchestration. Added `WriteResult` struct and `write_format_validate()` to `src/edit.rs` as the shared mutation tail — writes file, formats, validates syntax in correct order. Extended Config with timeout fields. Migrated 4 M001 commands (write, edit_symbol, edit_match, batch). 10 format unit tests cover timeout/kill/not-found/happy-path.

**T02 — Complete migration + integration tests.** Migrated remaining 8 M002 commands (add_import through add_struct_tags) — identical ~10-line substitution per handler. Wrote 6 integration tests through binary protocol proving format-applied (rustfmt), format-not-found, and fields-always-present paths. Discovered and fixed ruff pre-release corruption: added `ruff_format_available()` version guard requiring >= 0.1.2 (D067).

**T03 — Validation + plugin updates.** Added `run_external_tool_capture()` variant that returns Ok on non-zero exit (D068 — type checkers signal errors via exit code). Built `validate_full()` with per-checker output parsers: tsc text parsing, pyright JSON parsing, cargo check JSON message parsing, go vet text parsing. All parsers filter errors to the edited file. Extended WriteResult with validate_requested/validation_errors/validate_skipped_reason. Updated all 12 plugin tool definitions across 3 files with validate param and response field documentation. 8 parser unit tests + 4 integration tests.

The pipeline design from T01 (params as `&serde_json::Value`) paid off — T03 added validate extraction with zero call-site changes in command handlers.

## Verification

- `cargo build` — 0 warnings
- `cargo test` — 175 unit tests + 105 integration tests, all pass, 0 regressions
- `cargo test -- format` — 10 format unit tests pass (subprocess timeout/kill, not-found, happy-path, language detection)
- `cargo test -- format_integration` — 6 integration tests pass (applied-rustfmt, unsupported-language, not-found, add_import-with-format, edit_symbol-with-format, fields-always-present)
- `cargo test -- validate` — 5 validation tests pass (parser unit tests + integration through binary protocol)
- `bun test` — 36 plugin tests pass (tool registrations + round-trips with format/validate fields)
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — 0 hits (confirms complete migration)

## Requirements Validated

- R016 (Auto-format on save) — integration tests prove formatter detection, invocation, not-found graceful degradation across all 12 mutation commands. Response includes `formatted: true/false` with reason.
- R017 (Full validation mode) — integration tests prove `validate:"full"` invokes type checker and returns structured errors. Graceful degradation when checker not found.

## New Requirements Surfaced

None.

## Requirements Invalidated or Re-scoped

None.

## Deviations

- Added `tempfile` as dev-dependency for format unit tests (not in original plan, necessary for test isolation)
- Added `ruff_format_available()` version guard (D067) — discovered during T02 testing that ruff < 0.1.2 corrupts Python files
- Added `validate_requested` flag to WriteResult (not in plan) — needed to distinguish "validation not requested" (omit field) from "validation ran, zero errors" (include empty array)

## Known Limitations

- `cargo check` on an isolated .rs file (no Cargo.toml) produces an error skip reason — expected; files edited through aft are always in a project context
- `pyright` parser not exercised in integration tests (pyright not typically installed in CI) — unit test with sample JSON covers parser logic
- No user configuration for formatter choice — auto-detect from PATH only (D063 notes this as revisable)
- ruff version guard is a point-in-time fix; can be removed when minimum ruff version in ecosystem moves past 0.1.2

## Follow-ups

- S04 (Dry-run & Transactions) will consume the write_format_validate pipeline — dry-run should show the formatted result, not the raw edit (D047)

## Files Created/Modified

- `src/format.rs` — new module: subprocess runner, formatter/type-checker detection, auto_format, validate_full, output parsers, unit tests (~630 lines)
- `src/edit.rs` — WriteResult struct, write_format_validate shared pipeline
- `src/config.rs` — formatter_timeout_secs, type_checker_timeout_secs fields
- `src/lib.rs` — registered format module, updated config test
- `src/commands/write.rs` — migrated to write_format_validate
- `src/commands/edit_symbol.rs` — migrated to write_format_validate
- `src/commands/edit_match.rs` — migrated to write_format_validate
- `src/commands/batch.rs` — migrated to write_format_validate
- `src/commands/add_import.rs` — migrated to write_format_validate
- `src/commands/remove_import.rs` — migrated to write_format_validate
- `src/commands/organize_imports.rs` — migrated to write_format_validate
- `src/commands/add_member.rs` — migrated to write_format_validate
- `src/commands/add_derive.rs` — migrated to write_format_validate
- `src/commands/wrap_try_catch.rs` — migrated to write_format_validate
- `src/commands/add_decorator.rs` — migrated to write_format_validate
- `src/commands/add_struct_tags.rs` — migrated to write_format_validate
- `opencode-plugin-aft/src/tools/editing.ts` — validate param + response docs on write, edit_symbol, edit_match, batch
- `opencode-plugin-aft/src/tools/imports.ts` — validate param + response docs on add_import, remove_import, organize_imports
- `opencode-plugin-aft/src/tools/structure.ts` — validate param + response docs on add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags
- `tests/integration/format_test.rs` — 10 integration tests (6 format + 4 validation)
- `tests/integration/main.rs` — registered format_test module
- `Cargo.toml` — tempfile dev-dependency

## Forward Intelligence

### What the next slice should know
- `write_format_validate()` in `src/edit.rs` is the single entry point for all mutation tails. S04's dry-run mode should intercept before the actual `fs::write` inside this function — the format step should still run on the proposed content so the diff shows the formatted result (D047).
- Params flow through as `&serde_json::Value` — S04 can extract `dry_run` from the same params object with zero call-site changes, exactly as `validate` was added.
- The `transaction` command (S04) needs to call `write_format_validate` per file and collect results. Rollback should use BackupStore snapshots already taken by each command.

### What's fragile
- ruff version detection parses `ruff --version` output with string splitting — if ruff changes their version output format, the guard breaks (falls back to black, no data loss)
- Per-checker output parsers are format-dependent: tsc `--pretty false`, pyright `--outputjson`, cargo `--message-format=json`. Version changes to these tools' output formats could break parsing (graceful — unparsed errors are dropped, not crash)

### Authoritative diagnostics
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — must return 0 hits; confirms all commands use the shared pipeline
- `cargo test -- format` — covers all subprocess error paths
- stderr `[aft] format:` and `[aft] validate:` messages — grep these for runtime debugging

### What assumptions changed
- Original plan assumed ruff was always safe to use for formatting — actually requires version >= 0.1.2 (D067)
- Original plan didn't anticipate needing `validate_requested` flag — but it's necessary to distinguish "no validation" from "validation with zero errors" in response JSON
