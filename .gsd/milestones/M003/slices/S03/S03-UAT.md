# S03: Trace to Entry Points — UAT

**Milestone:** M003
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All verification is through binary protocol integration tests — no UI, no human experience to validate. Entry point detection and backward traversal are fully testable through automated fixtures.

## Preconditions

- `cargo build` succeeds (debug binary at `target/debug/aft`)
- Test fixtures exist in `tests/fixtures/callgraph/` (main.ts, utils.ts, data.ts, service.ts, test_helpers.ts)
- Fixture call chain: `main.ts:main` → `data.ts:processData` → `utils.ts:validate` → `utils.ts:checkFormat`
- Additional paths: `service.ts:handleRequest` (exported) → `data.ts:processData`, `test_helpers.ts:testValidation` → `utils.ts:validate`

## Smoke Test

Send `configure` then `trace_to` for `checkFormat` in `utils.ts` through binary protocol. Response should contain at least one path starting at `main` and ending at `checkFormat` with `total_paths >= 1`.

## Test Cases

### 1. Single-path trace through 4-level call chain

1. Send `configure` with project root pointing to `tests/fixtures/callgraph/`
2. Send `trace_to` with `file: "utils.ts"`, `symbol: "checkFormat"`
3. **Expected:** Response contains a path with hops: `main` (main.ts) → `processData` (data.ts) → `validate` (utils.ts) → `checkFormat` (utils.ts), rendered top-down (entry point first, target last). `total_paths >= 1`, `entry_points_found >= 1`.

### 2. Multi-path trace to multiple entry points

1. Send `configure` with project root
2. Send `trace_to` with `file: "utils.ts"`, `symbol: "validate"`
3. **Expected:** Response contains 2+ distinct paths from 2+ distinct entry points. Paths include at least: one through `main` and one through either `handleRequest` (service.ts) or `testValidation` (test_helpers.ts). Each path ends at `validate`.

### 3. Not-configured guard

1. Start fresh binary (no `configure` sent)
2. Send `trace_to` with `file: "utils.ts"`, `symbol: "validate"`
3. **Expected:** Error response with `code: "not_configured"` and message instructing to call `configure` first.

### 4. Symbol not found error

1. Send `configure` with project root
2. Send `trace_to` with `file: "utils.ts"`, `symbol: "nonExistentFunction"`
3. **Expected:** Error response with `code: "symbol_not_found"`.

### 5. Entry point as target (no backward paths)

1. Send `configure` with project root
2. Send `trace_to` with `file: "main.ts"`, `symbol: "main"`
3. **Expected:** `main` is itself an entry point — response should handle gracefully. `total_paths` is 0 or 1 (self-path). No crash, no infinite loop.

### 6. Depth limiting

1. Send `configure` with project root
2. Send `trace_to` with `file: "utils.ts"`, `symbol: "checkFormat"`, `max_depth: 2`
3. **Expected:** Paths are truncated at 2 hops. `max_depth_reached: true` if the full path (4 hops) would exceed the limit. Fewer paths returned than with default depth.

### 7. Plugin tool registration

1. Run `bun test` in `opencode-plugin-aft/`
2. **Expected:** `aft_trace_to` tool is registered with correct Zod schema (file: string required, symbol: string required, depth: number optional). All plugin tests pass.

## Edge Cases

### Cycle detection

1. If a fixture had A → B → C → A, trace_to on C should not infinite-loop
2. **Expected:** Per-path visited sets prevent revisiting the same (file, symbol) pair within a single path. Response returns in bounded time with `max_depth_reached` or valid paths.

### Leaf function with no outgoing calls and not exported

1. `checkFormat` has no outgoing calls and is not exported — it's a pure leaf
2. **Expected:** `trace_to` finds it via `symbol_metadata` (third existence check), doesn't return `symbol_not_found`

### Disconnected symbol (no callers at all)

1. If a function exists but nothing calls it and it's not an entry point
2. **Expected:** `total_paths: 0`, `entry_points_found: 0`, `truncated_paths: 0` or `1`. Graceful empty response.

## Failure Signals

- `trace_to` returns `symbol_not_found` for a symbol that exists in the file — likely missing `symbol_metadata` check
- `trace_to` returns 0 paths for `checkFormat` — likely path canonicalization bug (lookup_file_data not finding data)
- `trace_to` hangs or panics — likely missing cycle detection (per-path visited sets)
- `total_paths` counts don't match actual paths array length — response serialization bug
- `bun test` fails on `aft_trace_to` — Zod schema mismatch or tool not registered

## Requirements Proved By This UAT

- R023 — Reverse trace to entry points: test cases 1, 2 prove backward traversal with multi-path support through binary protocol
- R026 — Entry point detection heuristics: test cases 1, 2, 5 prove exported functions, main patterns, and test patterns are correctly classified as entry points

## Not Proven By This UAT

- Framework-specific entry point patterns (Express routes, Flask decorators) — deferred per D081
- Data threading through call chains — R024, deferred to S04
- Impact analysis on traced paths — R025, deferred to S04
- File watcher interaction with trace_to — trace_to reuses the same reverse index that S02 proved is invalidated correctly

## Notes for Tester

- All test cases are already automated as integration tests in `tests/integration/callgraph_test.rs` — run `cargo test -- trace_to` to execute them
- The fixture files in `tests/fixtures/callgraph/` form a deliberate call graph; modifying them will break the integration tests
- `trace_to` paths are deterministic (sorted) — the test assertions depend on stable ordering
