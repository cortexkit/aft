---
estimated_steps: 6
estimated_files: 7
---

# T03: Full validation mode + plugin updates

**Slice:** S03 — Auto-format & Validation
**Milestone:** M002

## Description

Add opt-in full type-checker validation (R017) and update all plugin tool definitions to expose the `validate` parameter and document the format/validation response fields. Type checkers are invoked as subprocesses with the same external tool runner from T01 — the pattern is identical to formatter invocation but with different commands and longer timeouts.

T01 designed `write_format_validate` to accept the request `params` as `&serde_json::Value` specifically so this task can add validate extraction without changing any of the 12 call sites. Commands already pass `&req.params` — this task adds the logic inside `write_format_validate` to check for `validate: "full"` and invoke the type checker.

## Steps

1. Add type checker detection and invocation to `src/format.rs`:
   - `detect_type_checker(path, lang) -> Option<(String, Vec<String>)>` — TS/JS/TSX → `npx tsc --noEmit` (fallback `tsc --noEmit`), Python → `pyright`, Rust → `cargo check`, Go → `go vet`. Return (command, args).
   - `ValidationError` struct: `{ line: u32, column: u32, message: String, severity: String }`. Derive `Serialize` for JSON response embedding.
   - `parse_checker_output(stdout, stderr, file) -> Vec<ValidationError>` — parse type checker output into structured errors. Filter to errors related to the edited file where feasible (especially for `cargo check` which reports project-wide errors).
   - `validate_full(path, config) -> (Vec<ValidationError>, Option<String>)` — returns (errors, skip_reason). Detects checker, runs with `type_checker_timeout_secs`, parses output. Skip reasons: "not_found", "timeout", "error", "unsupported_language".
   - Unit tests for output parsing with sample tsc/cargo/go vet output strings.

2. Extend `write_format_validate` and `WriteResult` in `src/edit.rs`:
   - Add `validation_errors: Vec<format::ValidationError>` and `validate_skipped_reason: Option<String>` to `WriteResult`
   - In `write_format_validate`, check `params.get("validate")` — when value is `"full"`, call `format::validate_full()` after syntax validation and populate the result fields
   - No changes needed to any command handler call sites — they already pass `&req.params` which now naturally includes the validate param when the agent sends it

3. Update `opencode-plugin-aft/src/tools/editing.ts`:
   - Add optional `validate` param to write, edit_symbol, edit_match, batch tool definitions: `validate: z.enum(["syntax", "full"]).optional().describe("Validation level: 'syntax' (default, tree-sitter only) or 'full' (invoke project type checker)")`
   - Pass validate param through to bridge.send when present
   - Update tool descriptions to mention formatted/format_skipped_reason/validation_errors/validate_skipped_reason response fields

4. Update `opencode-plugin-aft/src/tools/imports.ts`:
   - Add optional `validate` param to add_import, remove_import, organize_imports
   - Pass validate param through to bridge.send when present

5. Update `opencode-plugin-aft/src/tools/structure.ts`:
   - Add optional `validate` param to add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags
   - Pass validate param through to bridge.send when present

6. Add validation integration tests to `tests/integration/format_test.rs`:
   - **validate_full_default_no_errors**: send mutation without validate param → no validation_errors field in response (or empty)
   - **validate_full_with_checker**: send write with `validate: "full"` on a `.rs` file with valid code → if cargo available, response includes `validation_errors: []`
   - **validate_full_type_error**: send write with `validate: "full"` on a `.rs` file with a type error → response includes non-empty `validation_errors` with line/message
   - **validate_full_checker_not_found**: send write with `validate: "full"` on a `.py` file → if pyright not installed, `validate_skipped_reason: "not_found"`
   - Run `bun test` to verify plugin tool definitions compile and pass

## Must-Haves

- [ ] `validate_full()` detects and invokes type checkers for all 4 language groups (TS, Python, Rust, Go)
- [ ] Type checker output parsed into structured `ValidationError` records with line/column/message/severity
- [ ] `WriteResult` extended with validation fields — no changes to command handler call sites
- [ ] All 3 plugin tool files (editing.ts, imports.ts, structure.ts) expose optional `validate` param on all mutation tools
- [ ] Integration tests prove validate:"full" path with and without available checker
- [ ] `bun test` passes with updated plugin tools
- [ ] `cargo build` produces 0 warnings

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all tests pass, 0 regressions
- `cargo test -- validate` — validation integration tests pass
- `bun test` — plugin tests pass with updated tool definitions
- Verify validate param flows through: send mutation with `validate: "full"` → response includes `validation_errors` or `validate_skipped_reason`

## Observability Impact

- Signals added: `[aft] validate: {file} ({checker})` / `[aft] validate: {file} (skipped: {reason})` on stderr
- How a future agent inspects this: check response `validation_errors` array + `validate_skipped_reason` field
- Failure state exposed: validate_skipped_reason distinguishes not_found/timeout/error/unsupported_language

## Inputs

- `src/format.rs` — external tool runner infrastructure from T01 (run_external_tool, FormatError)
- `src/edit.rs` — WriteResult, write_format_validate (receives &serde_json::Value params) from T01
- All 12 command handlers — already using write_format_validate with WriteOptions::default() from T01+T02
- `opencode-plugin-aft/src/tools/editing.ts`, `imports.ts`, `structure.ts` — existing tool definitions to extend
- `tests/integration/format_test.rs` — existing format tests from T02 to extend

## Expected Output

- `src/format.rs` — type checker detection + validation + output parsing added (~150-200 lines), ValidationError struct
- `src/edit.rs` — WriteResult extended with validation fields, write_format_validate checks params for validate:"full"
- 3 plugin tool files — validate param added to all mutation tools, response docs updated
- `tests/integration/format_test.rs` — 3-4 additional validation integration tests
