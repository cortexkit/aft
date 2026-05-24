# S03: Auto-format & Validation — UAT

**Milestone:** M002
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All criteria are machine-verifiable through binary protocol integration tests and unit tests. No UI or human-experience surfaces.

## Preconditions

- Rust toolchain installed with `rustfmt` available on PATH
- `cargo build` succeeds with 0 warnings
- `gofmt` available on PATH (standard Go toolchain)
- Python formatter available on PATH: `black` or `ruff` >= 0.1.2
- For validation tests: `cargo` on PATH (for `cargo check`)

## Smoke Test

Run `cargo test -- format_integration_applied_rustfmt` — should pass, confirming the entire pipeline (write file → detect rustfmt → invoke → verify formatted content → return `formatted: true` in response).

## Test Cases

### 1. Mutation with available formatter produces formatted output

1. Send `write` command with a `.rs` file containing poorly formatted Rust code (e.g., `fn    main(){let x=1;}`)
2. Check response JSON for `formatted` field
3. Read the file content back
4. **Expected:** Response has `formatted: true`, no `format_skipped_reason`. File content has been reformatted by rustfmt (proper spacing, newlines).

### 2. Mutation with unavailable formatter degrades gracefully

1. Send `write` command with a `.txt` file (no formatter mapped)
2. Check response JSON for `formatted` and `format_skipped_reason` fields
3. **Expected:** Response has `formatted: false` and `format_skipped_reason: "unsupported_language"`. File content is written as-is.

### 3. Formatter not found on PATH degrades gracefully

1. Send `write` command with a `.py` file on a system without `ruff` or `black` installed
2. **Expected:** Response has `formatted: false` and `format_skipped_reason: "not_found"`. File content is written as-is, not corrupted.

### 4. add_import with auto-format (integrated S01 verification)

1. Send `add_import` command for a `.rs` file with existing imports
2. Check response JSON for both import placement fields and `formatted` field
3. **Expected:** Import is placed in the correct group AND response includes `formatted: true` (rustfmt ran on the result). The import management and auto-format pipelines are integrated.

### 5. edit_symbol with auto-format

1. Write a `.rs` file with a function, then send `edit_symbol` to replace it with poorly formatted code
2. **Expected:** Response has `formatted: true`. File content is reformatted. `backup_id` is present for undo.

### 6. All 12 mutation commands include format fields

1. For each of the 12 mutation commands (write, edit_symbol, edit_match, batch, add_import, remove_import, organize_imports, add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags): send a valid command
2. **Expected:** Every response includes `formatted` (bool). When `formatted` is false, `format_skipped_reason` is present with one of: "not_found", "timeout", "error", "unsupported_language".

### 7. validate:"full" with available type checker

1. Send `write` command with a valid `.rs` file and `validate: "full"` parameter (in a directory with Cargo.toml)
2. **Expected:** Response includes `validation_errors` array (empty if code is valid). stderr shows `[aft] validate: {file} (cargo, 0 errors)`.

### 8. validate:"full" with unavailable type checker

1. Send `write` command with a `.txt` file and `validate: "full"` parameter
2. **Expected:** Response includes `validate_skipped_reason: "unsupported_language"`. No crash, no error status.

### 9. validate:"full" not requested — field omitted

1. Send `write` command without `validate` parameter
2. **Expected:** Response does NOT include `validation_errors` or `validate_skipped_reason` fields. Only `formatted` and optionally `format_skipped_reason` are present.

### 10. Plugin tool definitions expose validate parameter

1. Inspect all 12 tool definitions in editing.ts, imports.ts, structure.ts
2. **Expected:** Each mutation tool has an optional `validate` parameter with description. Tool descriptions mention `formatted`, `format_skipped_reason`, `validation_errors` response fields.

## Edge Cases

### Subprocess timeout kills hung formatter

1. Configure `formatter_timeout_secs: 1` in Config
2. Invoke a formatter that takes longer than 1 second
3. **Expected:** Subprocess is killed. Response has `formatted: false`, `format_skipped_reason: "timeout"`. No hung process remains.

### ruff version guard prevents pre-release corruption

1. On a system with ruff < 0.1.2, send `write` for a `.py` file
2. **Expected:** ruff is NOT used for formatting. Falls back to `black` if available, or `not_found`. File content is NOT corrupted with `NOT_YET_IMPLEMENTED` stubs.

### Validation errors filtered to target file

1. In a project with multiple .rs files, one with a type error, send `write` with `validate: "full"` on a DIFFERENT valid file
2. **Expected:** `validation_errors` contains only errors for the target file, not the other file's errors.

### Format runs before syntax validation

1. Send `write` with a `.rs` file that has fixable formatting issues
2. **Expected:** `formatted: true` AND `syntax_valid: true`. The syntax validation runs on the post-formatted content, not the pre-formatted content.

## Failure Signals

- Any mutation response missing `formatted` field → incomplete migration
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` returns hits → command not migrated to write_format_validate
- `cargo test -- format` fails → subprocess runner or detection broken
- `bun test` fails on tool definition tests → plugin not updated
- `validation_errors` appearing in response when `validate` param not sent → lean-response logic broken
- Hung test → subprocess timeout/kill not working

## Requirements Proved By This UAT

- R016 (Auto-format on save) — test cases 1-6 prove formatter detection, invocation, graceful degradation, and universal field presence
- R017 (Full validation mode) — test cases 7-9 prove type checker invocation, structured error output, not-found degradation, and lean default responses

## Not Proven By This UAT

- Cross-project formatter config discovery (e.g., finding `.prettierrc` in parent directories) — formatters use their own config discovery
- pyright output parsing — pyright not typically available in test environments; covered by unit test with sample JSON
- Performance under large file formatting — no load testing included
- Windows path handling for formatter invocation — CI covers Linux/macOS only

## Notes for Tester

- Integration tests are the primary verification — `cargo test -- format_integration` and `cargo test -- validate` exercise the full pipeline through binary protocol
- The "formatter not found" test is conditional on the test environment — if ruff/black are installed, the Python not-found path won't trigger
- All format/validation stderr messages start with `[aft]` prefix — use `grep` to filter binary output during debugging
