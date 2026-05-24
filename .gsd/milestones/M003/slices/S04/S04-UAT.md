# S04: Data Flow Tracking + Impact Analysis — UAT

**Milestone:** M003
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: Both commands are testable via binary protocol JSON requests. Integration tests prove the full round-trip. No UI, no external services, no runtime environment needed.

## Preconditions

- `cargo build` succeeds (binary at `target/debug/aft`)
- A project directory with the callgraph fixtures exists at `tests/fixtures/callgraph/` containing: `main.ts`, `data.ts`, `utils.ts`, `data_flow.ts`, `data_processor.ts`
- Binary spawned and configured with `{"id":1,"command":"configure","project_root":"<abs_path_to_fixtures_parent>"}` returning `ok: true`

## Smoke Test

Send `{"id":2,"command":"impact","file":"<fixtures>/utils.ts","symbol":"validate","depth":5}` to the binary. Expect a JSON response with `ok: true`, non-empty `callers` array, and `total_affected >= 1`.

## Test Cases

### 1. Impact on multi-caller symbol

1. Configure binary with project root pointing to the callgraph fixtures parent
2. Send `{"id":2,"command":"impact","file":"<fixtures>/utils.ts","symbol":"validate","depth":5}`
3. **Expected:** Response includes `ok: true`, `total_affected >= 2`, `affected_files >= 1`. Each caller in `callers[]` has: `file` (absolute path), `symbol` (string), `line` (number), `signature` (string containing parameter list), `is_entry_point` (boolean), `call_expression` (string containing `validate`), `parameters` (array of strings). At least one caller should have `is_entry_point: true` (the `main` function or exported function).

### 2. Impact — not_configured guard

1. Spawn a fresh binary (no `configure` sent)
2. Send `{"id":1,"command":"impact","file":"any.ts","symbol":"foo","depth":5}`
3. **Expected:** Response has `ok: false`, `code: "not_configured"`

### 3. Impact — symbol_not_found

1. Configure binary with valid project root
2. Send `{"id":2,"command":"impact","file":"<fixtures>/utils.ts","symbol":"nonexistent_function","depth":5}`
3. **Expected:** Response has `ok: false`, `code: "symbol_not_found"`

### 4. Data flow tracking through local assignments

1. Configure binary with project root
2. Send `{"id":2,"command":"trace_data","file":"<fixtures>/data_flow.ts","symbol":"handleRequest","expression":"rawInput","depth":5}`
3. **Expected:** Response has `ok: true`, `hops` array with at least one entry of `flow_type: "assignment"`. The hop shows the variable rename (e.g., `rawInput` → `cleaned`). All hops in the same file have `approximate: false`.

### 5. Data flow tracking across file boundary

1. Configure binary with project root
2. Send `{"id":2,"command":"trace_data","file":"<fixtures>/data_flow.ts","symbol":"handleRequest","expression":"rawInput","depth":5}`
3. **Expected:** Response `hops` array includes at least one entry with `flow_type: "parameter"` where `file` points to `data_processor.ts`. This hop shows the value flowing from a call argument into the callee's parameter name.

### 6. Data flow tracking — approximation on destructuring

1. Configure binary with project root
2. Send `{"id":2,"command":"trace_data","file":"<fixtures>/data_flow.ts","symbol":"handleRequest","expression":"rawInput","depth":5}` (fixture must include a destructuring assignment in the tracking path)
3. **Expected:** Response `hops` array includes a hop with `approximate: true`, indicating static analysis lost confidence at a destructuring/spread point. Tracking does not continue past this hop on that branch.

### 7. Data flow tracking — not_configured guard

1. Spawn a fresh binary (no `configure` sent)
2. Send `{"id":1,"command":"trace_data","file":"any.ts","symbol":"foo","expression":"x","depth":5}`
3. **Expected:** Response has `ok: false`, `code: "not_configured"`

### 8. Data flow tracking — symbol_not_found

1. Configure binary with valid project root
2. Send `{"id":2,"command":"trace_data","file":"<fixtures>/data_flow.ts","symbol":"nonexistent","expression":"x","depth":5}`
3. **Expected:** Response has `ok: false`, `code: "symbol_not_found"`

### 9. Plugin tool registration — aft_impact

1. Run `bun test` in `opencode-plugin-aft/`
2. **Expected:** Tests pass confirming `aft_impact` tool is registered with correct Zod schema (file, symbol, depth parameters)

### 10. Plugin tool registration — aft_trace_data

1. Run `bun test` in `opencode-plugin-aft/`
2. **Expected:** Tests pass confirming `aft_trace_data` tool is registered with correct Zod schema (file, symbol, expression, depth parameters)

## Edge Cases

### Impact on symbol with no callers

1. Configure binary, send impact for a leaf function that nothing calls (e.g., a deeply nested utility)
2. **Expected:** Response has `ok: true`, `total_affected: 0`, `callers: []`

### trace_data with expression not found in symbol body

1. Configure binary, send trace_data with an `expression` value that doesn't appear in the symbol's body
2. **Expected:** Response has `ok: true`, `hops: []` (empty — nothing to track)

### Impact and trace_data depth limiting

1. Send impact/trace_data with `depth: 1` on a symbol with deep callers
2. **Expected:** Response only includes direct callers/hops (depth 1), not transitive ones

## Failure Signals

- Any `cargo test` failure in `impact` or `trace_data` test names
- `bun test` failures in tool registration tests
- Binary panics or hangs when processing impact/trace_data commands
- Missing fields in response JSON (no `callers`, no `hops`, no `total_affected`)
- `approximate` flag missing on hops where tracking should have marked uncertainty

## Requirements Proved By This UAT

- R024 (Data flow tracking) — test cases 4, 5, 6, 7, 8 prove expression tracking through assignments and parameters with approximation
- R025 (Change impact analysis) — test cases 1, 2, 3 prove impact returns callers with signatures and entry point annotations

## Not Proven By This UAT

- Framework-specific entry point detection (Express routes, Flask decorators) — deferred per D081
- Data flow through destructuring/spread (only approximation marking is tested, not actual tracking through)
- Performance under large codebases (>100 files) — tested with small fixtures only

## Notes for Tester

- All test cases can be automated via the existing `AftProcess` test harness pattern (see `tests/integration/callgraph_test.rs`)
- The fixture files in `tests/fixtures/callgraph/` are the canonical test data — verify they contain the expected structures before testing
- `extract_parameters` has its own 15 unit tests covering edge cases — run `cargo test -- extract_parameters` separately if parameter extraction seems wrong
