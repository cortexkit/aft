# S02: Extract Function & Inline Symbol

**Goal:** Two single-file refactoring commands — `extract_function` and `inline_symbol` — that handle parameter inference, return value detection, argument substitution, and scope conflict reporting for TS/JS/TSX and Python.

**Demo:** Agent calls `aft_extract_function` with a file and line range → gets a new function with auto-detected parameters and return type, original range replaced with a call. Agent calls `aft_inline_symbol` with a file and function call → call site replaced with the function body, arguments substituted for parameters, scope conflicts reported if present.

## Must-Haves

- `extract_function` command: takes `file`, `name`, `start_line`, `end_line` → extracts code into new function with inferred parameters and return value, replaces original range with call site
- Free variable detection: walks AST range, classifies identifiers as local declarations (skip), function-scope references (become parameters), module-scope references (skip), `this`/`self` references (error with recommendation)
- Return value detection: explicit `return` in range → return type; variable assigned in range and used after → return value; neither → void
- `inline_symbol` command: takes `file`, `symbol`, `call_site_line` → replaces call with function body, parameter→argument substitution, scope conflict detection
- Single-return guard (D102): reject functions with multiple returns
- Scope conflict reporting (D103): detect variable name collisions, return structured error with suggestions
- Language guard (D101): TS/JS/TSX and Python only, return `unsupported_language` for Rust/Go
- Both commands support `dry_run: true` (D071)
- Both commands use `write_format_validate()` (D046, D066)
- Both commands use `auto_backup()` for undo
- Plugin tools `aft_extract_function` and `aft_inline_symbol` registered in `refactoring.ts` with Zod schemas
- Integration tests through binary protocol proving success paths, dry-run, and error paths across TS and Python

## Proof Level

- This slice proves: contract + integration
- Real runtime required: no (binary protocol tests are sufficient)
- Human/UAT required: no

## Verification

- `cargo test extract_function` — unit tests for free variable detection, return value inference, scope conflict detection, and command handler
- `cargo test inline_symbol` — unit tests for single-return validation, argument substitution, scope conflicts, and command handler
- `cargo test --test integration` — integration tests for both commands through binary protocol (success, dry-run, error paths, TS + Python)
- `bun test` in `opencode-plugin-aft/` — plugin tool round-trip tests for `aft_extract_function` and `aft_inline_symbol`
- All existing tests still pass (`cargo test` baseline 396, `bun test` baseline 40)

## Observability / Diagnostics

- Runtime signals: stderr logs `[aft] extract_function: {name} from {file}:{start}-{end} ({N} params)` and `[aft] inline_symbol: {symbol} at {file}:{line}`
- Inspection surfaces: response JSON includes `parameters` (extract), `return_type` (extract), `conflicts` (inline) for agent introspection
- Failure visibility: structured error codes — `unsupported_language`, `this_reference_in_range`, `multiple_returns`, `scope_conflict` with `conflicting_names` and `suggestions`

## Integration Closure

- Upstream surfaces consumed: `src/parser.rs` (grammar_for, detect_language, node_text, node_range), `src/edit.rs` (line_col_to_byte, auto_backup, write_format_validate, dry_run_diff, is_dry_run), `src/calls.rs` (extract_calls_in_range), `src/indent.rs` (detect_indent), `src/symbols.rs` (Symbol, Range)
- New wiring introduced in this slice: `extract_function` and `inline_symbol` dispatch entries in main.rs, two new modules in commands/, shared `extract.rs` module, two new plugin tools in refactoring.ts
- What remains before the milestone is truly usable end-to-end: S03 (LSP-enhanced symbol resolution) — optional accuracy enhancement, not a functional dependency

## Tasks

- [x] **T01: Build extract_function command with free variable detection** `est:2h`
  - Why: Core complexity of the slice — free variable classification and return value detection are the novel algorithms. extract_function's `extract.rs` utilities are reused by T02's inline_symbol.
  - Files: `src/extract.rs`, `src/commands/extract_function.rs`, `src/commands/mod.rs`, `src/main.rs`, `src/lib.rs`, `tests/fixtures/extract_function/`
  - Do: Build `extract.rs` with `detect_free_variables()` (AST walk over byte range, classify identifiers by scope level — local decl / enclosing function param / module-level / this-self) and `detect_return_value()` (scan for return stmts, post-range variable usage). Build `handle_extract_function` following edit_symbol.rs pattern: validate params → read file → parse AST → detect free vars → detect return → generate function text with `detect_indent` → replace range with call → dry_run check → auto_backup → write_format_validate → response. Guard with D101 language check. Handle `this`/`self` as structured error. Unit tests for free variable detection across TS and Python patterns (simple refs, property access, module-level, destructured, this/self). Wire dispatch entry.
  - Verify: `cargo test extract_function` passes all unit tests; `cargo test` passes with zero regressions
  - Done when: `extract_function` command works through dispatch for TS/JS/TSX and Python — extracts a block with free variables into a new function with correct parameters and return value, rejects `this`/`self` and unsupported languages with structured errors

- [x] **T02: Build inline_symbol command with scope conflict detection** `est:1.5h`
  - Why: Completes the second refactoring command. Reuses free variable detection from T01 for scope conflict checking. Simpler than extract_function due to single-return constraint.
  - Files: `src/commands/inline_symbol.rs`, `src/extract.rs` (add scope conflict utility), `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/inline_symbol/`
  - Do: Build `handle_inline_symbol`: validate params → resolve function symbol → verify single-return (D102) → find call site at specified line → extract function body → build param→arg substitution map → check scope conflicts at call site using extract.rs utilities (D103) → replace call expression with substituted body → dry_run check → auto_backup → write_format_validate → response. Handle expression-body arrow functions (implicit return). Handle call-as-assignment-RHS vs standalone-statement. Add `detect_scope_conflicts()` to extract.rs. Unit tests for substitution, single-return validation, scope conflict detection.
  - Verify: `cargo test inline_symbol` passes all unit tests; `cargo test` passes with zero regressions
  - Done when: `inline_symbol` command works through dispatch — inlines a single-return function at a call site with correct argument substitution, rejects multi-return functions and reports scope conflicts as structured errors

- [x] **T03: Integration tests and plugin tool registration** `est:1h`
  - Why: Proves both commands work end-to-end through the binary protocol and are accessible to agents via the plugin. Closes the integration loop.
  - Files: `tests/integration/extract_function_test.rs`, `tests/integration/inline_symbol_test.rs`, `tests/integration/main.rs`, `opencode-plugin-aft/src/tools/refactoring.ts`, `opencode-plugin-aft/src/__tests__/tools.test.ts`
  - Do: Write integration tests using AftProcess pattern from move_symbol_test.rs — temp dir fixtures, binary protocol round-trips. Extract tests: basic extract (TS), extract with return value, extract Python, dry-run, unsupported language error, this-reference error. Inline tests: basic inline (TS), inline expression-body arrow, inline Python, dry-run, multiple-returns error, scope-conflict error. Add `aft_extract_function` and `aft_inline_symbol` tool definitions to refactoring.ts with Zod schemas (file, name/symbol, start_line/end_line/call_site_line, scope, dry_run). Add bun tests for plugin round-trips.
  - Verify: `cargo test --test integration` passes all new tests; `bun test` in `opencode-plugin-aft/` passes; `cargo test` full suite passes
  - Done when: ≥6 extract_function + ≥6 inline_symbol integration tests pass through binary protocol; plugin tools registered and round-trip tested via bun

## Files Likely Touched

- `src/extract.rs` (new — shared free variable detection and scope conflict utilities)
- `src/lib.rs` (add `pub mod extract`)
- `src/commands/extract_function.rs` (new — command handler)
- `src/commands/inline_symbol.rs` (new — command handler)
- `src/commands/mod.rs` (add two module declarations)
- `src/main.rs` (add two dispatch entries)
- `tests/fixtures/extract_function/` (new — TS and Python fixture files)
- `tests/fixtures/inline_symbol/` (new — TS and Python fixture files)
- `tests/integration/extract_function_test.rs` (new)
- `tests/integration/inline_symbol_test.rs` (new)
- `tests/integration/main.rs` (add two module declarations)
- `opencode-plugin-aft/src/tools/refactoring.ts` (add two tool definitions)
- `opencode-plugin-aft/src/__tests__/tools.test.ts` (add round-trip tests)
