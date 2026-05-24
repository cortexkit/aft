---
estimated_steps: 8
estimated_files: 9
---

# T01: Auto-format infrastructure + core command integration

**Slice:** S03 — Auto-format & Validation
**Milestone:** M002

## Description

Build the external tool runner and formatter detection in a new `src/format.rs` module, create the shared `write_format_validate()` function in `src/edit.rs`, and prove the pipeline end-to-end by updating the 4 M001 mutation commands (write, edit_symbol, edit_match, batch). This establishes the pattern that T02 replicates for the remaining 8 commands.

The subprocess runner uses `std::process::Command` with `try_wait()` polling for timeout detection — single-threaded compatible (D014). Formatter detection walks a priority list per language (D063). The `WriteResult` struct (D066) replaces the per-command `fs::write` + `validate_syntax` tail blocks.

## Steps

1. Create `src/format.rs` with:
   - `run_external_tool(command, args, working_dir, timeout_secs) -> Result<ExternalToolResult, FormatError>` — spawns subprocess, polls `try_wait()` at 50ms intervals, kills on timeout, returns stdout/stderr/exit_code. Handles `ErrorKind::NotFound` for missing binaries.
   - `FormatError` enum: `NotFound { tool }`, `Timeout { tool, timeout_secs }`, `Failed { tool, stderr }`, `UnsupportedLanguage`
   - `detect_formatter(path, lang) -> Option<(String, Vec<String>)>` — returns (command, args) for the file's language. TS/JS/TSX → `prettier --write <file>`, Python → `ruff format <file>` (fallback `black <file>`), Rust → `rustfmt <file>`, Go → `gofmt -w <file>`. Checks PATH availability by attempting spawn.
   - `auto_format(path, config) -> (bool, Option<String>)` — returns (formatted, skip_reason). Detects language, finds formatter, runs with `formatter_timeout_secs`, returns result. Skip reasons: "not_found", "timeout", "error", "unsupported_language".
   - Unit tests: timeout kills subprocess (use `sleep` as test command), not-found returns FormatError::NotFound, happy path with rustfmt on a .rs file.

2. Extend `Config` in `src/config.rs` with `formatter_timeout_secs: u32` (default 10) and `type_checker_timeout_secs: u32` (default 30) per D043.

3. Register `pub mod format;` in `src/lib.rs`.

4. Add to `src/edit.rs`:
   - `WriteResult` struct: `{ syntax_valid: Option<bool>, formatted: bool, format_skipped_reason: Option<String> }`. Fields for validation (validation_errors, validate_skipped_reason) will be added in T03.
   - `write_format_validate(path, content, config, params) -> Result<WriteResult, AftError>` — takes the request `params` as `&serde_json::Value` so T03 can extract the `validate` param without changing any call sites. Writes content via `fs::write`, calls `auto_format`, calls `validate_syntax`, returns combined result. The format step runs BEFORE validate_syntax so we validate the formatted content (D046). In T01, the `params` argument is passed through but not used for validation — T03 adds that logic.

5. Update `src/commands/write.rs` — replace the `fs::write` + `validate_syntax` block with `edit::write_format_validate(path, content, ctx.config(), &req.params)`. Add `formatted` and `format_skipped_reason` to the response JSON from WriteResult fields. Keep the existing `create_dirs` logic and error handling before the write call.

6. Update `src/commands/edit_symbol.rs` — same substitution: replace `fs::write` + `validate_syntax` tail with `write_format_validate()`, add format fields to response.

7. Update `src/commands/edit_match.rs` — same substitution.

8. Update `src/commands/batch.rs` — same substitution. Note: batch writes the accumulated content after all edits are applied, so write_format_validate replaces the single `fs::write` + `validate_syntax` at the end.

## Must-Haves

- [ ] `run_external_tool` handles NotFound, Timeout (with kill), and successful exit
- [ ] `detect_formatter` returns correct command+args for all 6 languages (TS/JS/TSX/Python/Rust/Go)
- [ ] `auto_format` returns `(false, Some("unsupported_language"))` for unknown file types
- [ ] `write_format_validate` writes → formats → validates in correct order
- [ ] All 4 M001 commands (write, edit_symbol, edit_match, batch) use write_format_validate
- [ ] All 4 commands include `formatted` and `format_skipped_reason` in response
- [ ] Config has `formatter_timeout_secs` and `type_checker_timeout_secs` with correct defaults
- [ ] Unit tests cover timeout+kill, not-found, and happy-path formatting

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all tests pass, 0 regressions
- `cargo test -- format` — format module unit tests pass (timeout, not-found, happy path)
- Verify subprocess timeout test actually kills the child process (not just times out)

## Observability Impact

- Signals added: `[aft] format: {file} ({formatter})` and `[aft] format: {file} (skipped: {reason})` on stderr for every mutation
- How a future agent inspects this: grep stderr for `[aft] format:`, check response `formatted`/`format_skipped_reason` fields
- Failure state exposed: FormatError variants distinguish not_found/timeout/failed/unsupported — each maps to a skip_reason string in the response

## Inputs

- `src/edit.rs` — existing `validate_syntax`, `auto_backup`, `replace_byte_range` functions
- `src/config.rs` — existing Config struct to extend
- `src/parser.rs` — `detect_language()` and `LangId` enum for language-to-formatter mapping
- `src/commands/write.rs`, `edit_symbol.rs`, `edit_match.rs`, `batch.rs` — existing `fs::write` + `validate_syntax` pattern to replace

## Expected Output

- `src/format.rs` — new module (~200-250 lines) with subprocess runner, formatter detection, auto_format
- `src/edit.rs` — WriteResult struct + write_format_validate function added (~30-40 lines)
- `src/config.rs` — 2 new fields with defaults
- `src/commands/write.rs`, `edit_symbol.rs`, `edit_match.rs`, `batch.rs` — updated to use write_format_validate, each ~10 lines shorter
