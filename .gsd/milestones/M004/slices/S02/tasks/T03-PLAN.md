---
estimated_steps: 5
estimated_files: 5
---

# T03: Integration tests and plugin tool registration

**Slice:** S02 — Extract Function & Inline Symbol
**Milestone:** M004

## Description

Wire both commands through the full integration stack: binary protocol tests proving end-to-end behavior, and plugin tool definitions making them accessible to agents. This is the verification closure — proving the commands work through the same path agents use, not just as Rust unit tests.

## Steps

1. Create `tests/integration/extract_function_test.rs` with integration tests using the AftProcess pattern from move_symbol_test.rs (temp dir with fixture copies): (a) basic extract — TS function with 3 free variables → verify new function has 3 params, original range replaced with call; (b) extract with return value — variable assigned in range used after; (c) Python extract — verify correct `def` syntax; (d) dry-run — file unchanged, diff returned; (e) unsupported language error — `.rs` file returns `unsupported_language`; (f) this-reference error — range containing `this` returns `this_reference_in_range`.
2. Create `tests/integration/inline_symbol_test.rs` with integration tests: (a) basic inline — TS helper function call replaced with body; (b) expression-body arrow function — implicit return inlined correctly; (c) Python inline; (d) dry-run — file unchanged; (e) multiple-returns error — function with 2 returns rejected; (f) scope-conflict error — response includes conflicting variable names and suggestions.
3. Register both test modules in `tests/integration/main.rs`.
4. Add `aft_extract_function` and `aft_inline_symbol` tool definitions to `opencode-plugin-aft/src/tools/refactoring.ts` with Zod schemas. extract_function: file (string), name (string), start_line (number), end_line (number), dry_run (boolean optional). inline_symbol: file (string), symbol (string), call_site_line (number), dry_run (boolean optional). Both follow the bridge.send pattern from aft_move_symbol.
5. Add bun tests in `opencode-plugin-aft/src/__tests__/tools.test.ts` for both tools: extract_function round-trip (write fixture → extract → verify response has parameters), inline_symbol round-trip (write fixture with helper + call → inline → verify response). Run `bun test` and `cargo test` full suite.

## Must-Haves

- [ ] ≥6 integration tests for extract_function (success TS, success with return, success Python, dry-run, unsupported language, this-reference)
- [ ] ≥6 integration tests for inline_symbol (success TS, expression body, success Python, dry-run, multiple returns, scope conflict)
- [ ] Both test modules registered in integration main.rs
- [ ] `aft_extract_function` tool registered in refactoring.ts with Zod schema
- [ ] `aft_inline_symbol` tool registered in refactoring.ts with Zod schema
- [ ] Bun tests pass for both plugin round-trips
- [ ] Full `cargo test` suite passes (baseline 396 + new tests)
- [ ] Full `bun test` suite passes (baseline 40 + new tests)

## Verification

- `cargo test --test integration extract_function` — ≥6 tests pass
- `cargo test --test integration inline_symbol` — ≥6 tests pass
- `cargo test` — full suite passes, zero regressions
- `cd opencode-plugin-aft && bun test` — all tests pass including new round-trips

## Inputs

- `src/commands/extract_function.rs` — working command handler from T01
- `src/commands/inline_symbol.rs` — working command handler from T02
- `tests/integration/move_symbol_test.rs` — integration test pattern reference (AftProcess, temp dir, fixture copy)
- `opencode-plugin-aft/src/tools/refactoring.ts` — existing `aft_move_symbol` tool definition to follow

## Expected Output

- `tests/integration/extract_function_test.rs` — ≥6 integration tests (~200 lines)
- `tests/integration/inline_symbol_test.rs` — ≥6 integration tests (~200 lines)
- `tests/integration/main.rs` — two new module declarations
- `opencode-plugin-aft/src/tools/refactoring.ts` — two new tool definitions added to refactoringTools()
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — two new round-trip tests

## Observability Impact

This task adds the verification surface — no new runtime signals, but it proves the existing observability works end-to-end:

- **Integration tests validate stderr log output**: The `[aft] extract_function:` and `[aft] inline_symbol:` stderr lines fire during integration test runs, confirming the runtime signals from T01/T02 work through the binary protocol.
- **Plugin tools expose all response fields**: `aft_extract_function` returns `parameters`, `return_type`, `diff` (dry-run); `aft_inline_symbol` returns `call_context`, `substitutions`, `conflicts`, `diff` (dry-run) — all inspectable by agents.
- **Error paths tested**: structured error codes (`unsupported_language`, `this_reference_in_range`, `multiple_returns`, `scope_conflict`) verified through integration tests with correct diagnostic data (`return_count`, `conflicting_names`, `suggestions`).
- **Future agent debugging**: If a tool call fails, run with `dry_run: true` first to get the diff preview and response diagnostics without mutation.
