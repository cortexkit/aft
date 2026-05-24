# S03: Structural Reading — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: live-runtime
- Why this mode is sufficient: Both commands are exercised through the actual binary protocol (spawn binary, send JSON on stdin, read JSON from stdout). This proves the real runtime path end-to-end.

## Preconditions

- `cargo build` succeeds with 0 errors and 0 warnings
- Binary at `target/debug/aft` exists and is executable
- Test fixture files exist: `tests/fixtures/sample.ts`, `tests/fixtures/sample.py`, `tests/fixtures/sample.rs`, `tests/fixtures/calls.ts`

## Smoke Test

Spawn the binary, send `{"id":"1","command":"outline","file":"tests/fixtures/sample.ts"}`, verify response contains `"ok":true` and a non-empty `entries` array.

## Test Cases

### 1. Outline — TypeScript nested structure

1. Spawn `target/debug/aft` and send: `{"id":"1","command":"outline","file":"tests/fixtures/sample.ts"}`
2. Parse JSON response
3. **Expected:** `ok: true`, `entries` array contains top-level items including a class entry with `kind: "class"`. The class entry has a `members` array containing method entries with `kind: "method"`. Methods do NOT appear as separate top-level entries.

### 2. Outline — Python multi-level nesting

1. Send: `{"id":"2","command":"outline","file":"tests/fixtures/sample.py"}`
2. Parse JSON response
3. **Expected:** `ok: true`, `entries` contains a class. Nested classes appear as members of the outer class. Methods of inner classes appear as members of the inner class (multi-level nesting works).

### 3. Outline — Rust file with impl blocks

1. Send: `{"id":"3","command":"outline","file":"tests/fixtures/sample.rs"}`
2. Parse JSON response
3. **Expected:** `ok: true`, `entries` contains struct entries and function entries. Methods from impl blocks appear as members of their parent struct.

### 4. Outline — all symbol kinds present

1. Send outline for each fixture file (TS, Python, Rust)
2. Collect all `kind` values from entries and their members recursively
3. **Expected:** Across the fixtures, the following kinds appear: `function`, `class`, `method`, `struct`, `interface`, `enum`, `type_alias`. Each is correctly categorized.

### 5. Outline — export flag preserved

1. Send outline for `tests/fixtures/sample.ts`
2. Check `exported` field on entries
3. **Expected:** Exported symbols have `exported: true`, non-exported symbols have `exported: false`.

### 6. Zoom — success with annotations

1. Send: `{"id":"10","command":"zoom","file":"tests/fixtures/calls.ts","symbol":"orchestrate"}`
2. Parse JSON response
3. **Expected:** `ok: true`. Response contains:
   - `name: "orchestrate"`
   - `kind: "function"`
   - `content` — the function body as a string
   - `context_before` — up to 3 lines before the function
   - `context_after` — up to 3 lines after the function
   - `annotations.calls_out` — array containing `"compute"` (since orchestrate calls compute)
   - `annotations.called_by` — empty array (nothing in the file calls orchestrate at top level, or populated if something does)

### 7. Zoom — custom context lines

1. Send: `{"id":"11","command":"zoom","file":"tests/fixtures/calls.ts","symbol":"helper","context_lines":1}`
2. Parse JSON response
3. **Expected:** `ok: true`. `context_before` contains at most 1 line. `context_after` contains at most 1 line.

### 8. Zoom — symbol not found

1. Send: `{"id":"12","command":"zoom","file":"tests/fixtures/calls.ts","symbol":"nonexistent_function"}`
2. Parse JSON response
3. **Expected:** `ok: false`, error `code: "symbol_not_found"`, `message` contains "nonexistent_function".

### 9. Zoom — unused function has empty annotations

1. Send: `{"id":"13","command":"zoom","file":"tests/fixtures/calls.ts","symbol":"unusedFunction"}`
2. Parse JSON response
3. **Expected:** `ok: true`. `annotations.calls_out` is empty (function calls nothing in file scope). `annotations.called_by` is empty (nothing calls it).

## Edge Cases

### Missing file parameter (outline)

1. Send: `{"id":"20","command":"outline"}`
2. **Expected:** `ok: false`, error `code: "invalid_request"`, message indicates missing `file` parameter.

### Missing file parameter (zoom)

1. Send: `{"id":"21","command":"zoom","symbol":"foo"}`
2. **Expected:** `ok: false`, error `code: "invalid_request"`, message indicates missing `file` parameter.

### Missing symbol parameter (zoom)

1. Send: `{"id":"22","command":"zoom","file":"tests/fixtures/calls.ts"}`
2. **Expected:** `ok: false`, error `code: "invalid_request"`, message indicates missing `symbol` parameter.

### Nonexistent file (outline)

1. Send: `{"id":"23","command":"outline","file":"nonexistent/path.ts"}`
2. **Expected:** `ok: false`, error `code: "file_not_found"`.

### Nonexistent file (zoom)

1. Send: `{"id":"24","command":"zoom","file":"nonexistent/path.ts","symbol":"foo"}`
2. **Expected:** `ok: false`, error `code: "file_not_found"`.

### Context lines clamp at file boundaries

1. Send zoom for the first function in a file with `context_lines: 100`
2. **Expected:** `context_before` contains only lines from the start of the file to the function (no crash, no out-of-bounds). `context_after` contains only lines from the function end to the file end.

## Failure Signals

- Any `ok: false` response where `ok: true` was expected
- Missing `entries` field in outline response
- Methods appearing as top-level outline entries instead of nested under their class
- Empty `calls_out` array when the zoomed function visibly calls other in-file functions
- Crash or hang of the binary process during any command
- `cargo build` producing warnings

## Requirements Proved By This UAT

- R003 — outline returns nested symbol tree with kind, name, range, signature, export status; zoom returns body with context and caller/callee annotations
- R011 (partial) — zoom returns ambiguous_symbol error with candidates when multiple symbols match a name

## Not Proven By This UAT

- R011 full validation — edit_symbol disambiguation is S05 scope
- Cross-file call graph — zoom annotations are file-scoped only (M003)
- Performance under large files — not tested (acceptable for M001)

## Notes for Tester

- The binary stays alive between commands — send all test cases to a single spawned process to verify persistent operation.
- Call annotations are based on text-matching callee names against known file-scoped symbols. Indirect calls (callbacks, dynamic dispatch) won't appear.
- Member access calls like `this.add(a, b)` will show `add` in calls_out, not `this.add`.
