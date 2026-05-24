---
id: T03
parent: S03
milestone: M002
provides:
  - validate_full() type-checker invocation for 4 language groups (TS, Python, Rust, Go)
  - ValidationError struct with line/column/message/severity (serializable)
  - run_external_tool_capture() subprocess runner that captures output on non-zero exit
  - parse_checker_output() with per-checker parsers (tsc, pyright, cargo, go vet)
  - WriteResult extended with validate_requested, validation_errors, validate_skipped_reason
  - All 12 mutation commands emit validation fields when validate:"full" is requested
  - All 12 plugin tool definitions expose optional validate param
  - 4 validation integration tests through binary protocol
key_files:
  - src/format.rs
  - src/edit.rs
  - src/commands/*.rs (all 12 mutation commands)
  - opencode-plugin-aft/src/tools/editing.ts
  - opencode-plugin-aft/src/tools/imports.ts
  - opencode-plugin-aft/src/tools/structure.ts
  - tests/integration/format_test.rs
key_decisions:
  - "D068: run_external_tool_capture() variant returns Ok on non-zero exit — type checkers use exit code to signal errors found, not tool failure"
  - "D069: validation_errors array is only included in response when validate:'full' is requested (via validate_requested flag on WriteResult), keeping default responses lean"
  - "D070: cargo check runs with --message-format=json for structured parsing; tsc with --pretty false for parseable text output; pyright with --outputjson for JSON output"
patterns_established:
  - "validate param flows through existing &req.params → write_format_validate pipeline with zero call-site changes (as designed in T01)"
  - "All checker output parsers filter to errors in the edited file — especially important for cargo check which reports project-wide errors"
observability_surfaces:
  - "[aft] validate: {file} ({checker}, {N} errors) on stderr when checker runs"
  - "[aft] validate: {file} (skipped: {reason}) on stderr when checker is skipped"
  - "Response fields: validation_errors (array of {line, column, message, severity}), validate_skipped_reason (string)"
duration: 1 task
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: Full validation mode + plugin updates

**Added opt-in type-checker validation (validate:"full") with structured error output for TS/Python/Rust/Go, updated all 12 plugin tool definitions with validate param.**

## What Happened

Added `run_external_tool_capture()` — a subprocess runner variant that captures stdout/stderr regardless of exit code. Type checkers return non-zero when they find errors, which is expected behavior not tool failure, so the existing `run_external_tool()` (which treats non-zero as `FormatError::Failed`) was wrong for this use case.

Implemented `detect_type_checker()` covering all 4 language groups: TS/JS/TSX → `npx tsc --noEmit` (fallback `tsc`), Python → `pyright --outputjson`, Rust → `cargo check --message-format=json`, Go → `go vet`. Each checker uses structured or parseable output flags for reliable parsing.

Built per-checker output parsers: `parse_tsc_output` (text format `path(line,col): error TSxxxx: message`), `parse_pyright_output` (JSON `generalDiagnostics` array), `parse_cargo_output` (JSON `compiler-message` lines filtered by primary span), `parse_go_vet_output` (text `path:line:col: message`). All parsers filter errors to the target file.

`validate_full()` orchestrates detection → invocation → parsing and returns `(Vec<ValidationError>, Option<String>)` with the same skip-reason pattern as `auto_format()`.

Extended `WriteResult` with `validate_requested`, `validation_errors`, and `validate_skipped_reason`. The `write_format_validate` function checks `params.get("validate")` — when `"full"`, invokes `validate_full()` after syntax validation. Zero call-site changes in command handlers as designed in T01 (params already flowed through as `&req.params`).

Added `validation_errors` and `validate_skipped_reason` to all 12 command handler responses. The `validation_errors` array is only included when `validate_requested` is true, keeping default responses lean.

Updated all 12 plugin tool definitions across 3 files (editing.ts, imports.ts, structure.ts) to expose the optional `validate` parameter and document the new response fields.

Added 8 unit tests for output parsing and 4 integration tests through the binary protocol.

## Verification

- `cargo build` — 0 warnings ✓
- `cargo test` — 175 unit tests + 105 integration tests, all pass, 0 regressions ✓
- `cargo test -- validate` — 5 validation tests pass (1 unit + 4 integration) ✓
- `bun test` — 36 plugin tests pass ✓
- Manual verification: `validate:"full"` on .rs file → response includes `validation_errors: []` with `[aft] validate: file (cargo, 0 errors)` on stderr ✓
- Manual verification: `validate:"full"` on .txt file → response includes `validate_skipped_reason: "unsupported_language"` with skip message on stderr ✓

### Slice-level verification status (S03):
- `cargo build` — 0 warnings ✓
- `cargo test` — all pass, 0 regressions ✓
- `cargo test -- format` — format unit tests pass ✓
- `cargo test -- format_integration` — format integration tests pass (applied, unsupported, not_found, add_import, edit_symbol, fields_always_present) ✓
- `cargo test -- validate` — validation tests pass ✓
- `bun test` — 36 plugin tests pass ✓
- All slice verification checks pass ✓

## Diagnostics

- Grep stderr for `[aft] validate:` to see which files were validated and which were skipped
- Check response JSON `validation_errors` (array of {line, column, message, severity}) and `validate_skipped_reason` (string)
- ValidationError struct is serializable — can be inspected programmatically from response JSON
- Skip reasons: "not_found" (no checker on PATH), "timeout" (exceeded type_checker_timeout_secs), "error" (checker spawn/exec error), "unsupported_language" (no checker mapping)

## Deviations

- Added `validate_requested` flag to WriteResult — not in original plan but necessary to distinguish "validation not requested" (omit field) from "validation ran, zero errors" (include empty array). This keeps default response payloads lean while giving callers a clear signal that validation was invoked.
- Changed command handler pattern from `if !write_result.validation_errors.is_empty()` to `if write_result.validate_requested` — ensures `validation_errors: []` appears in response when validate:"full" is requested, even when code is valid.

## Known Issues

- `cargo check` on an isolated .rs file (no Cargo.toml) produces an error skip reason — this is expected behavior; the checker needs a project context. In practice, files being edited through aft are always in a project.
- `pyright` parsing not exercised in integration tests because pyright is not installed in CI. The unit test with sample JSON output covers the parser logic.

## Files Created/Modified

- `src/format.rs` — Added run_external_tool_capture(), ValidationError struct, detect_type_checker(), parse_checker_output() + 4 per-checker parsers, validate_full(), 10 new unit tests (~350 lines added)
- `src/edit.rs` — Extended WriteResult with validate_requested/validation_errors/validate_skipped_reason; write_format_validate now checks params for validate:"full"
- `src/commands/*.rs` (12 files) — Added validation_errors and validate_skipped_reason to response JSON in all mutation command handlers
- `opencode-plugin-aft/src/tools/editing.ts` — Added validate param to write, edit_symbol, edit_match, batch; updated descriptions
- `opencode-plugin-aft/src/tools/imports.ts` — Added validate param to add_import, remove_import, organize_imports; updated descriptions
- `opencode-plugin-aft/src/tools/structure.ts` — Added validate param to add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags; updated descriptions
- `tests/integration/format_test.rs` — Added 4 validation integration tests
