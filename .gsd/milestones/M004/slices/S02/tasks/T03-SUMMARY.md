---
id: T03
parent: S02
milestone: M004
provides:
  - 12 integration tests (6 extract_function + 6 inline_symbol) through binary protocol
  - aft_extract_function and aft_inline_symbol plugin tool definitions with Zod schemas
  - 2 bun round-trip tests for plugin tools
key_files:
  - tests/integration/extract_function_test.rs
  - tests/integration/inline_symbol_test.rs
  - opencode-plugin-aft/src/tools/refactoring.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
key_decisions:
  - Integration tests use temp-dir isolation with fixture copies (same pattern as move_symbol_test.rs) — each test gets a clean copy to avoid cross-test contamination
  - Plugin tool schemas use z.number() for start_line/end_line/call_site_line (matching binary protocol u64 params) — not z.string() despite being line references
patterns_established:
  - extract_function/inline_symbol integration test modules follow identical setup_*_fixture() + configure() pattern — reusable template for future single-file refactoring commands
observability_surfaces:
  - Integration tests validate stderr log lines fire during binary protocol execution
  - Plugin tools expose all diagnostic response fields (parameters, return_type, call_context, substitutions, conflicts) for agent introspection
  - Error paths verified with structured codes and diagnostic data (return_count, conflicting_names, suggestions)
duration: 20min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: Integration tests and plugin tool registration

**Wired extract_function and inline_symbol through full integration stack — 12 binary protocol tests, 2 plugin tool definitions, 2 bun round-trip tests. All pass.**

## What Happened

Created integration tests following the AftProcess temp-dir pattern from move_symbol_test.rs. Each test module copies its fixtures to a temp dir, spawns the aft binary, sends commands through the protocol, and verifies both response JSON and file system state.

extract_function_test.rs covers: basic TS extract (free variable detection + file mutation), extract with return value, Python extract (`def` syntax), dry-run (file unchanged + diff returned), unsupported language error (.rs file), this-reference error (class method with `this`).

inline_symbol_test.rs covers: basic TS inline (assignment context), expression-body arrow function, Python inline, dry-run, multiple-returns rejection (return_count in response), scope-conflict error (conflicting_names + suggestions arrays).

Added `aft_extract_function` and `aft_inline_symbol` to refactoringTools() in refactoring.ts with Zod schemas matching the binary protocol params. Added bun round-trip tests that create temp fixtures, configure, exercise the tool, and verify response structure.

Fixed two off-by-one line numbers in inline tests (0-indexed vs 1-indexed) — caught by test failures, corrected immediately.

## Verification

- `cargo test --test integration extract_function` — 6 tests pass ✓
- `cargo test --test integration inline_symbol` — 6 tests pass ✓
- `cargo test` — 280 unit + 166 integration = 446 total, zero failures ✓
- `cd opencode-plugin-aft && bun test` — 42 tests pass (baseline 40 + 2 new) ✓

Slice-level verification (all checks pass — this is the final task):
- `cargo test extract_function` — unit + integration tests pass ✓
- `cargo test inline_symbol` — unit + integration tests pass ✓
- `cargo test --test integration` — 166 tests pass ✓
- `bun test` in opencode-plugin-aft/ — 42 tests pass ✓
- All existing tests still pass (no regressions) ✓

## Diagnostics

- Send `extract_function` or `inline_symbol` commands with `dry_run: true` to preview without mutation
- Integration test failures surface exact response JSON in assertion messages
- Plugin tool round-trips verify the full path: TypeScript → bridge.send → binary protocol → Rust handler → response JSON → TypeScript parse

## Deviations

Fixed 0-indexed line numbers for inline_symbol tests (Python `add` call at line 9, TS `double` call at line 17). Plan didn't specify exact line numbers — derived from fixture files.

## Known Issues

None.

## Files Created/Modified

- `tests/integration/extract_function_test.rs` — 6 integration tests for extract_function through binary protocol
- `tests/integration/inline_symbol_test.rs` — 6 integration tests for inline_symbol through binary protocol
- `tests/integration/main.rs` — added extract_function_test and inline_symbol_test module declarations
- `opencode-plugin-aft/src/tools/refactoring.ts` — added aft_extract_function and aft_inline_symbol tool definitions with Zod schemas
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — added extract_function and inline_symbol round-trip tests
- `.gsd/milestones/M004/slices/S02/tasks/T03-PLAN.md` — added Observability Impact section (pre-flight fix)
