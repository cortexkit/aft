# S03: Auto-format & Validation

**Goal:** Every mutation command auto-formats via the project's formatter when available, and `validate: "full"` invokes the project's type checker — with graceful degradation when tools aren't found and subprocess timeout protection.
**Demo:** `add_import` on a `.rs` file → import lands in correct group AND file is auto-formatted by rustfmt → response includes `formatted: true`. Same command on a file with no formatter available → response includes `formatted: false, format_skipped_reason: "not_found"`. Mutation with `validate: "full"` → response includes structured type checker output.

## Must-Haves

- External tool runner with subprocess timeout + kill + not-found handling
- Formatter detection per language: TS/JS/TSX → prettier, Python → ruff/black, Rust → rustfmt, Go → gofmt
- `auto_format()` function that detects and invokes the correct formatter for a file
- `WriteResult` struct + `write_format_validate()` shared function in `src/edit.rs` replacing `fs::write` + `validate_syntax` across all 12 mutation commands
- All 12 mutation command responses include `formatted` and `format_skipped_reason` fields
- `validate: "full"` parameter on mutation commands invokes type checker (tsc, pyright, cargo check, go vet)
- Config extended with `formatter_timeout_secs` (default 10) and `type_checker_timeout_secs` (default 30)
- Plugin tool definitions updated with `validate` param and format/validation response documentation
- Integration tests proving format-applied and format-not-found paths through binary protocol
- Unit tests proving subprocess timeout and kill behavior

## Proof Level

- This slice proves: operational (external tool invocation with timeout/kill/not-found graceful degradation)
- Real runtime required: yes (subprocess spawning, formatter invocation)
- Human/UAT required: no

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all existing tests pass (0 regressions) + new format/validation tests pass
- `cargo test -- format` — unit tests for subprocess runner, formatter detection, format pipeline
- `cargo test -- format_integration` — integration tests through binary protocol proving:
  - Mutation + formatter available → `formatted: true`, file content is formatted
  - Mutation + formatter not available → `formatted: false`, `format_skipped_reason` present
  - Mutation + `validate: "full"` → `validation_errors` in response (or `validate_skipped_reason` if checker not found)
- `bun test` — plugin tests pass with updated tool definitions

## Observability / Diagnostics

- Runtime signals: `[aft] format: {file} ({formatter})` / `[aft] format: {file} (skipped: {reason})` / `[aft] validate: {file} ({checker})` on stderr
- Inspection surfaces: `formatted`, `format_skipped_reason`, `validation_errors`, `validate_skipped_reason` fields in every mutation response
- Failure visibility: format_skipped_reason distinguishes "not_found" / "timeout" / "error" / "unsupported_language"; validation_errors include line/column/message/severity
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: S01's import commands (verify imports + auto-format integrated), S02's compound operations (verify add_member etc. + auto-format)
- New wiring introduced in this slice: format hook in edit pipeline via `write_format_validate()` — all existing and future mutation commands inherit auto-formatting
- What remains before the milestone is truly usable end-to-end: S04 (dry-run mode on all mutations, multi-file transactions with rollback)

## Tasks

- [x] **T01: Auto-format infrastructure + core command integration** `est:2h`
  - Why: Builds the external tool runner, formatter detection, and edit pipeline hook — then proves it end-to-end by wiring the 4 M001 mutation commands (write, edit_symbol, edit_match, batch)
  - Files: `src/format.rs` (new), `src/edit.rs`, `src/config.rs`, `src/lib.rs`, `src/commands/write.rs`, `src/commands/edit_symbol.rs`, `src/commands/edit_match.rs`, `src/commands/batch.rs`
  - Do: Create format module with subprocess runner (spawn + timeout polling via try_wait + kill), per-language formatter detection (D063), auto_format function. Add WriteResult struct and write_format_validate to edit.rs (D066). Extend Config with timeout fields (D043). Update 4 M001 commands to use write_format_validate. Unit tests for subprocess timeout, not-found, and happy path.
  - Verify: `cargo build` 0 warnings, `cargo test` 0 failures, unit tests prove timeout/kill/not-found/happy-path
  - Done when: write_format_validate works end-to-end for M001 commands, format module has unit test coverage for all error paths

- [x] **T02: Complete command migration + format integration tests** `est:1h`
  - Why: Updates remaining 8 M002 mutation commands (identical mechanical substitution) and adds comprehensive integration tests proving the format pipeline through the binary protocol
  - Files: `src/commands/add_import.rs`, `src/commands/remove_import.rs`, `src/commands/organize_imports.rs`, `src/commands/add_member.rs`, `src/commands/add_derive.rs`, `src/commands/wrap_try_catch.rs`, `src/commands/add_decorator.rs`, `src/commands/add_struct_tags.rs`, `tests/integration/format_test.rs`, `tests/integration/main.rs`
  - Do: Replace fs::write + validate_syntax with write_format_validate in all 8 handlers. Write integration tests: (1) write .rs file with bad formatting → formatted: true + content verified, (2) write file with no formatter → formatted: false + reason, (3) add_import .rs file → import placed correctly AND formatted. Each command handler change is identical ~10-line substitution.
  - Verify: `cargo test` 0 failures, `cargo test -- format_integration` proves format-applied and format-not-found paths
  - Done when: all 12 mutation commands use write_format_validate, integration tests prove both formatter-found and formatter-not-found paths

- [x] **T03: Full validation mode + plugin updates** `est:1.5h`
  - Why: Adds opt-in type checker invocation (R017) and updates plugin tool definitions so agents can use validate:"full" and see format response fields
  - Files: `src/format.rs`, `src/edit.rs`, `opencode-plugin-aft/src/tools/editing.ts`, `opencode-plugin-aft/src/tools/imports.ts`, `opencode-plugin-aft/src/tools/structure.ts`, `tests/integration/format_test.rs`
  - Do: Add type checker detection per language (tsc/npx tsc, pyright, cargo check, go vet) to format.rs. Add validate_full function with subprocess invocation + output parsing. Extend write_format_validate to accept optional validate param and return validation_errors. Update all 3 plugin tool files: add optional validate param to all mutation tools, document formatted/format_skipped_reason/validation_errors response fields. Integration tests for validate:"full" paths.
  - Verify: `cargo test` 0 failures, `bun test` 0 failures, integration tests prove validation-applied and validation-not-found paths
  - Done when: validate:"full" works through binary protocol, plugin tools expose validate param, all format/validation response fields documented

## Files Likely Touched

- `src/format.rs` (new — external tool runner, formatter/type-checker detection, auto_format, validate_full)
- `src/edit.rs` (WriteResult struct, write_format_validate function)
- `src/config.rs` (formatter_timeout_secs, type_checker_timeout_secs)
- `src/lib.rs` (register format module)
- `src/commands/write.rs`
- `src/commands/edit_symbol.rs`
- `src/commands/edit_match.rs`
- `src/commands/batch.rs`
- `src/commands/add_import.rs`
- `src/commands/remove_import.rs`
- `src/commands/organize_imports.rs`
- `src/commands/add_member.rs`
- `src/commands/add_derive.rs`
- `src/commands/wrap_try_catch.rs`
- `src/commands/add_decorator.rs`
- `src/commands/add_struct_tags.rs`
- `opencode-plugin-aft/src/tools/editing.ts`
- `opencode-plugin-aft/src/tools/imports.ts`
- `opencode-plugin-aft/src/tools/structure.ts`
- `tests/integration/format_test.rs` (new)
- `tests/integration/main.rs`
