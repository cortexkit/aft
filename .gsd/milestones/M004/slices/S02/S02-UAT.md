# S02: Extract Function & Inline Symbol — UAT

**Milestone:** M004
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: Both commands are protocol-level operations verified through binary stdin/stdout — no live runtime, UI, or human-experience testing needed. Integration tests prove the full path from JSON command to file mutation and response.

## Preconditions

- `cargo build` succeeds — the `aft` binary is built at `target/debug/aft`
- Test fixtures exist in `tests/fixtures/extract_function/` and `tests/fixtures/inline_symbol/`
- A writable temp directory is available for fixture copies

## Smoke Test

Run `cargo test extract_function_basic_ts` — should pass in <5s, proving the binary accepts an extract_function command through the protocol, detects free variables, generates a function, replaces the range, and returns a valid response with `parameters`, `return_type`, and `syntax_valid: true`.

## Test Cases

### 1. Extract function with free variables (TypeScript)

1. Copy `tests/fixtures/extract_function/sample.ts` to a temp directory
2. Send `configure` with project root, then `extract_function` with `file: sample.ts`, `name: "extracted"`, `start_line` and `end_line` covering a range that references enclosing-function parameters (e.g., `a` and `b`)
3. **Expected:** Response has `ok: true`, `parameters` array containing the free variable names, `return_type` reflecting the return behavior, `syntax_valid: true`, `backup_id` present. File on disk contains a new function `extracted` with the free variables as parameters and the original range replaced with `extracted(a, b)` (or similar call).

### 2. Extract function with return value detection

1. Use a fixture where the extracted range contains `return expr;`
2. Send `extract_function` for that range
3. **Expected:** `return_type` is `"Expression"`, generated function includes a `return` statement, call site is `const result = extracted(...)` or equivalent assignment pattern.

### 3. Extract function — Python

1. Copy `tests/fixtures/extract_function/sample.py` to temp
2. Send `extract_function` targeting a range with free variables
3. **Expected:** Generated function uses `def extracted(param):` syntax (not `function`), call site uses Python syntax, `syntax_valid: true`.

### 4. Extract function — dry run

1. Send `extract_function` with `dry_run: true`
2. **Expected:** Response includes a unified `diff` showing the proposed change, `syntax_valid` field present. File on disk is unchanged (byte-for-byte identical to original).

### 5. Inline symbol — basic (TypeScript)

1. Copy `tests/fixtures/inline_symbol/sample.ts` to temp
2. Send `inline_symbol` with `file: sample.ts`, `symbol: "add"` (a single-return function), `call_site_line` pointing to a line with `add(x, y)`
3. **Expected:** Response has `ok: true`, `call_context` (e.g., `"assignment"`), `substitutions` count > 0. File on disk has the call replaced with the function body, parameters substituted with arguments.

### 6. Inline symbol — expression-body arrow function

1. Use a fixture with `const double = (x: number) => x * 2;` and a call `double(5)`
2. Send `inline_symbol` targeting that call
3. **Expected:** Call replaced with `5 * 2` (or equivalent with argument substituted for parameter), no return statement artifacts.

### 7. Inline symbol — Python

1. Copy `tests/fixtures/inline_symbol/sample.py` to temp
2. Send `inline_symbol` for a Python function call
3. **Expected:** Correct substitution using Python syntax, `syntax_valid: true`.

### 8. Inline symbol — dry run

1. Send `inline_symbol` with `dry_run: true`
2. **Expected:** Response includes `diff`, file unchanged.

### 9. Plugin tool round-trip — extract_function

1. In `opencode-plugin-aft/`, run the extract_function round-trip test
2. **Expected:** Plugin creates temp fixture, sends `aft_extract_function` through BinaryBridge, receives response with `parameters` array and `syntax_valid: true`.

### 10. Plugin tool round-trip — inline_symbol

1. Run the inline_symbol round-trip test
2. **Expected:** Plugin creates temp fixture, sends `aft_inline_symbol` through BinaryBridge, receives response with `call_context` and `substitutions`.

## Edge Cases

### Unsupported language (extract_function)

1. Send `extract_function` with a `.rs` or `.go` file
2. **Expected:** Response has `ok: false`, error code `unsupported_language`, no file modification.

### this/self reference in range (extract_function)

1. Use `tests/fixtures/extract_function/sample_this.ts` — class method with `this.value`
2. Send `extract_function` covering the range with `this`
3. **Expected:** Response has `ok: false`, error code `this_reference_in_range`, includes recommendation text about extracting as a method instead.

### Multiple returns (inline_symbol)

1. Use `tests/fixtures/inline_symbol/sample_multi.ts` — function with 2+ return statements
2. Send `inline_symbol` targeting that function
3. **Expected:** Response has `ok: false`, error code `multiple_returns`, `return_count` field shows the actual count.

### Scope conflict (inline_symbol)

1. Use `tests/fixtures/inline_symbol/sample_conflict.ts` — function body declares a variable that already exists at the call site scope
2. Send `inline_symbol` targeting that call
3. **Expected:** Response has `ok: false`, error code `scope_conflict`, `conflicting_names` array lists the colliding variable names, `suggestions` array maps each conflict to a suggested rename (e.g., `temp` → `temp_inlined`).

### Invalid line range (extract_function)

1. Send `extract_function` with `start_line: 100`, `end_line: 200` on a 20-line file
2. **Expected:** Error response indicating invalid range.

### Missing required params

1. Send `extract_function` without `name` field
2. **Expected:** Error response about missing parameter.

## Failure Signals

- Any `cargo test` failure in `extract_function` or `inline_symbol` test modules
- `bun test` failures in the refactoring round-trip tests
- Response JSON missing `parameters` (extract) or `call_context` (inline) fields on success
- `syntax_valid: false` on a known-good extraction or inlining
- File modified when `dry_run: true` was specified
- Error code mismatch (e.g., getting `symbol_not_found` instead of `unsupported_language`)

## Requirements Proved By This UAT

- R029 — extract_function correctly extracts a code block with free variables into a new function with inferred parameters and return type, replaces original range with call, across TS and Python
- R030 — inline_symbol correctly replaces a function call with the function body, argument substitution correct, scope conflicts detected and reported, across TS and Python

## Not Proven By This UAT

- Rust/Go language support for either command (deferred per D101)
- Multi-return function inlining (rejected by design per D102)
- Auto-resolution of scope conflicts (reported only per D103)
- LSP-enhanced symbol resolution for either command (S03 scope)

## Notes for Tester

- All test cases are fully automated via `cargo test` and `bun test`. The cases above describe what each test proves for human review of coverage adequacy.
- Fixture files in `tests/fixtures/extract_function/` and `tests/fixtures/inline_symbol/` are the canonical inputs — inspect them to understand exact line numbers referenced in tests.
- The `dry_run` tests are critical — they verify the non-destructive preview path that agents will use before committing refactoring operations.
