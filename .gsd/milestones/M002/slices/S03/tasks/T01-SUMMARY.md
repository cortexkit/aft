---
id: T01
parent: S03
milestone: M002
provides:
  - format module with subprocess runner, formatter detection, auto_format
  - WriteResult struct and write_format_validate shared pipeline in edit.rs
  - Config extended with formatter_timeout_secs and type_checker_timeout_secs
  - 4 M001 mutation commands (write, edit_symbol, edit_match, batch) migrated to write_format_validate
key_files:
  - src/format.rs
  - src/edit.rs
  - src/config.rs
  - src/commands/write.rs
  - src/commands/edit_symbol.rs
  - src/commands/edit_match.rs
  - src/commands/batch.rs
key_decisions:
  - Format runs BEFORE validate_syntax so we validate the formatted content (D046)
  - write_format_validate takes &serde_json::Value for params to allow T03 to extract validate param without changing call sites
  - FormatError is internal to the format module; auto_format returns (bool, Option<String>) skip reasons for response JSON
  - tool_available checks PATH by attempting spawn with --version rather than which/where
patterns_established:
  - Mutation command tail pattern: auto_backup → edit → write_format_validate(path, content, config, params) → add format fields to response
  - Skip reason strings: "not_found", "timeout", "error", "unsupported_language"
observability_surfaces:
  - stderr: "[aft] format: {file} ({formatter})" on successful format
  - stderr: "[aft] format: {file} (skipped: {reason})" when format is skipped
  - response JSON: "formatted" (bool) and "format_skipped_reason" (string) on all 4 migrated commands
duration: 15min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Auto-format infrastructure + core command integration

**Built the external tool runner, formatter detection, shared write pipeline, and migrated 4 M001 mutation commands to auto-format on every write.**

## What Happened

Created `src/format.rs` (~280 lines) with three layers:

1. **`run_external_tool`** — spawns subprocess, polls `try_wait()` at 50ms intervals, kills on timeout, handles `ErrorKind::NotFound` for missing binaries. Returns `ExternalToolResult` or `FormatError`.

2. **`detect_formatter`** — maps LangId to formatter command+args: TS/JS/TSX → `prettier --write`, Python → `ruff format` (fallback `black`), Rust → `rustfmt`, Go → `gofmt -w`. Checks PATH availability by attempting spawn.

3. **`auto_format`** — detects language, finds formatter, runs with config timeout, returns `(formatted, skip_reason)`. Emits diagnostic messages to stderr.

Extended `Config` with `formatter_timeout_secs: u32` (default 10) and `type_checker_timeout_secs: u32` (default 30).

Added `WriteResult` struct and `write_format_validate()` to `src/edit.rs` — the shared tail that writes → formats → validates in correct order. Takes `&serde_json::Value` params for T03's future `validate` parameter.

Updated all 4 M001 commands (write, edit_symbol, edit_match, batch) to replace their `fs::write` + `validate_syntax` blocks with `write_format_validate()`, adding `formatted` and `format_skipped_reason` to response JSON.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 163 unit tests + 95 integration tests, all pass, 0 regressions ✅
- `cargo test -- format` — 10 format-specific tests pass ✅
  - `run_external_tool_timeout_kills_subprocess` — confirms `sleep 60` is killed after 1s timeout
  - `run_external_tool_not_found` — confirms NotFound error for nonexistent binary
  - `auto_format_happy_path_rustfmt` — confirms rustfmt reformats a .rs file
  - `auto_format_unsupported_language` — confirms .txt files return unsupported_language
  - `detect_formatter_*` — verifies correct command+args mapping for Rust, Go, Python

### Slice-level verification status (T01 is task 1 of 3):
- ✅ `cargo build` — 0 warnings
- ✅ `cargo test` — all tests pass, 0 regressions
- ✅ `cargo test -- format` — format module unit tests pass
- ⬜ `cargo test -- format_integration` — integration tests not yet written (T02)
- ⬜ `bun test` — plugin updates not yet done (T03)

## Diagnostics

- Grep stderr for `[aft] format:` to see which files were formatted and which were skipped
- Check response JSON `formatted` (bool) and `format_skipped_reason` (string) fields
- FormatError variants: NotFound, Timeout, Failed, UnsupportedLanguage — each maps to a skip_reason string

## Deviations

- Added `tempfile` as a dev-dependency for format unit tests (needed for temp .rs files in happy-path test). Not in original plan but necessary for proper test isolation.

## Known Issues

None.

## Files Created/Modified

- `src/format.rs` — new module: subprocess runner, formatter detection, auto_format, unit tests
- `src/edit.rs` — added WriteResult struct and write_format_validate function
- `src/config.rs` — added formatter_timeout_secs and type_checker_timeout_secs fields
- `src/lib.rs` — registered format module, updated config_default_values test
- `src/commands/write.rs` — migrated to write_format_validate, added format response fields
- `src/commands/edit_symbol.rs` — migrated to write_format_validate, added format response fields
- `src/commands/edit_match.rs` — migrated to write_format_validate, added format response fields
- `src/commands/batch.rs` — migrated to write_format_validate, added format response fields
- `Cargo.toml` — added tempfile dev-dependency
