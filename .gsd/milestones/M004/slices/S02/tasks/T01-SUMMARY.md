---
id: T01
parent: S02
milestone: M004
provides:
  - extract_function command handler with free variable detection and return value inference
  - shared extract.rs module (detect_free_variables, detect_return_value, generate_extracted_function)
  - test fixtures for extract_function (TS, Python, this-reference scenarios)
key_files:
  - src/extract.rs
  - src/commands/extract_function.rs
  - tests/fixtures/extract_function/
key_decisions:
  - Free variable classification uses AST scope walking (enclosing function params vs module-level) rather than simple text scanning — more accurate but language-specific
  - Property access identifiers filtered by checking parent node field name (member_expression.property, attribute.attribute) rather than node kind alone — handles Python attribute access correctly
  - Return value detection uses both explicit return scanning and post-range variable usage analysis within enclosing function boundary
  - this/self detection is an error (not a parameter) — extract as method is a fundamentally different operation
patterns_established:
  - extract.rs shared module pattern — utilities reusable by inline_symbol (T02) for scope conflict detection
  - find_deepest_ancestor uses child(i) iteration instead of cursor.node() to avoid tree-sitter lifetime issues with recursive calls
observability_surfaces:
  - stderr log: "[aft] extract_function: {name} from {file}:{start}-{end} ({N} params)"
  - response JSON: parameters array, return_type string, syntax_valid, backup_id
  - error codes: unsupported_language, this_reference_in_range (machine-parseable for agent retry)
  - dry_run mode returns diff + syntax_valid without mutation
duration: 35min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Build extract_function command with free variable detection

**Built extract_function command with AST-based free variable detection, return value inference, and function generation for TS/JS/Python.**

## What Happened

Created `src/extract.rs` with three core utilities:
- `detect_free_variables` — walks AST within byte range, collects identifier references, classifies them as: local declarations (skip), enclosing function scope refs (become parameters), module-level (skip), this/self (error flag). Filters property access identifiers (`obj.prop` — `prop` is NOT a free variable) by checking parent node field names.
- `detect_return_value` — checks for explicit `return` statements in range (→ Expression), variables declared in-range and referenced after the range within the enclosing function (→ Variable), or neither (→ Void).
- `generate_extracted_function` / `generate_call_site` — produce language-appropriate function declaration and call site text with correct indentation.

Created `src/commands/extract_function.rs` following the `edit_symbol.rs` handler pattern: validate params → language guard (TS/JS/TSX/Python only) → parse AST → convert line range to byte range → detect free variables → check this/self error → detect return value → generate function + call site → build new source → dry_run or auto_backup + write_format_validate → respond with structured JSON.

Wired dispatch in `main.rs`, modules in `lib.rs` and `commands/mod.rs`.

## Verification

- `cargo test extract_function` — 21 tests pass (14 extract module + 7 handler)
  - Free variable detection: enclosing function params, property access filtering, module-level skipping, this/self detection (TS and Python)
  - Return value: explicit return, post-range usage, void
  - Function generation: TS and Python output, call site variants (return var, void, expression)
  - Handler: missing params (file, name, start_line), unsupported language, invalid line range, this-reference error, dry_run
- `cargo test` — 417 total (263 + 154), zero regressions against 396 baseline
- Manual spot-check: sent `extract_function` via binary for TS fixture — correct parameters detected, function + call site generated, syntax_valid: true, backup_id present

## Diagnostics

- Send `extract_function` command with `dry_run: true` to preview without mutation
- Response JSON `parameters` array shows detected free variables — verify against source
- Error code `this_reference_in_range` includes recommendation to extract as method instead
- Error code `unsupported_language` for non-TS/JS/Python files
- stderr log line includes function name, file, line range, and parameter count

## Slice Verification Status (intermediate)

- ✅ `cargo test extract_function` — 21 tests pass
- ⬜ `cargo test inline_symbol` — T02 scope
- ⬜ `cargo test --test integration` — T03 scope
- ⬜ `bun test` — T03 scope
- ✅ All existing tests pass (417 total, zero regressions)

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/extract.rs` — new shared module with detect_free_variables, detect_return_value, generate_extracted_function + 14 unit tests
- `src/commands/extract_function.rs` — command handler + 7 unit tests
- `src/lib.rs` — added `pub mod extract`
- `src/commands/mod.rs` — added `pub mod extract_function`
- `src/main.rs` — added dispatch entry for `extract_function`
- `tests/fixtures/extract_function/sample.ts` — TS fixture with function-scoped refs, module-level refs, void function
- `tests/fixtures/extract_function/sample.py` — Python equivalent fixture
- `tests/fixtures/extract_function/sample_this.ts` — class method fixture with `this` reference
- `.gsd/milestones/M004/slices/S02/tasks/T01-PLAN.md` — added Observability Impact section
