# S02: Reverse Callers + File Watcher — UAT

**Milestone:** M003
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All verification is through automated tests exercising the binary protocol. The file watcher invalidation cycle is tested with real filesystem events — no mock layer. No human-visible UI involved.

## Preconditions

- `cargo build` succeeds (binary compiles)
- `cargo test` passes all 326 tests
- `bun test` passes all 39 plugin tests
- Multi-file call graph fixture exists at `tests/fixtures/callgraph/multifile/` with helpers.ts, utils.ts, index.ts, aliased.ts, types.ts

## Smoke Test

Run `cargo test -- callgraph_callers_cross_file` — confirms the callers command returns cross-file caller sites for a known function. If this passes, the reverse index, cross-file resolution, and command handler all work.

## Test Cases

### 1. Cross-file callers grouped by file

1. Start the binary, send `configure` with `project_root` pointing to `tests/fixtures/callgraph/multifile/`
2. Send `callers` with `file: "helpers.ts"`, `symbol: "validate"`, `depth: 1`
3. **Expected:** Response includes `total_callers: 2`, `scanned_files: 5`, callers array with entries from utils.ts (`processData` calls validate) and aliased.ts (`runCheck` calls validate via aliased import)

### 2. Recursive depth expansion

1. Configure with multifile fixture
2. Send `callers` with `file: "helpers.ts"`, `symbol: "validate"`, `depth: 2`
3. **Expected:** Response shows depth-1 callers (processData in utils.ts, runCheck in aliased.ts) AND depth-2 callers of those functions (e.g., index.ts main calling processData)

### 3. Empty callers result

1. Configure with multifile fixture
2. Send `callers` with `file: "index.ts"`, `symbol: "main"`, `depth: 1`
3. **Expected:** Response has `total_callers: 0`, empty `callers` array. No error — valid result for a top-level function nobody calls.

### 4. Not-configured error

1. Start binary without sending `configure`
2. Send `callers` with any file/symbol
3. **Expected:** Error response with `code: "not_configured"`, message instructing to call configure first

### 5. Symbol not found error

1. Configure with multifile fixture
2. Send `callers` with `file: "helpers.ts"`, `symbol: "nonExistentFunction"`, `depth: 1`
3. **Expected:** Error response with `code: "symbol_not_found"` including the symbol name and file path

### 6. File watcher — add caller

1. Configure with a temporary copy of the multifile fixture (watcher starts)
2. Send `callers` for validate in helpers.ts — note initial caller count
3. Write a new file `extra_caller.ts` into the fixture directory that imports and calls validate
4. Wait 500ms for OS event delivery
5. Send a `ping` command (triggers drain_watcher_events)
6. Send `callers` for validate again
7. **Expected:** Caller count increased by 1, new caller from extra_caller.ts appears in results

### 7. File watcher — remove caller

1. Configure with a temporary copy of the multifile fixture (watcher starts)
2. Send `callers` for validate — confirm callers exist
3. Rewrite utils.ts to remove the call to validate (replace function body)
4. Wait 500ms for OS event delivery
5. Send `ping` (triggers drain)
6. Send `callers` for validate again
7. **Expected:** Caller from utils.ts no longer appears, total_callers decreased

### 8. Plugin tool registration

1. Run `bun test` in opencode-plugin-aft
2. **Expected:** `aft_callers` tool is registered with correct Zod schema (file: string, symbol: string, depth: optional number), tool appears in the tool list

## Edge Cases

### Callers with cycle detection

1. Configure with a fixture containing mutual recursion (A calls B, B calls A)
2. Send `callers` for A with depth 5
3. **Expected:** Results include B as a caller but do not loop infinitely. Response terminates with finite results.

### Re-configure replaces watcher

1. Configure with directory A (watcher starts on A)
2. Configure again with directory B
3. Modify a file in directory A
4. Send ping + callers
5. **Expected:** Changes in directory A are NOT reflected — watcher now watches directory B only

### Non-source file changes ignored

1. Configure with fixture directory
2. Create or modify a `.json` or `.md` file in the directory
3. Wait 500ms, send ping
4. **Expected:** No invalidation occurs (extension filter excludes non-source files). `[aft] invalidated N files` log does NOT appear.

## Failure Signals

- `cargo test -- callgraph` has any failure — reverse index or watcher integration broken
- RefCell borrow panic in stderr during any test — two-phase drain pattern has overlapping borrows (D091 violation)
- `callers` returns `total_callers: 0` for a function with known callers — path canonicalization issue (caller paths not matching reverse index keys)
- Watcher cycle tests pass locally but fail in CI — FSEvents timing issue, increase sleep duration
- `[aft] watcher started:` log missing after configure — watcher initialization failed silently

## Requirements Proved By This UAT

- R022 (Reverse caller tree) — test cases 1-3, 5 prove callers grouped by file with recursive depth, empty results, and error handling
- R020 (Call graph construction, file watcher component) — test cases 6-7 prove the modify-then-query invalidation cycle with real filesystem events

## Not Proven By This UAT

- R023 (trace_to) — S03 scope, backward traversal to entry points
- R024/R025 (trace_data/impact) — S04 scope
- Performance under load — no benchmark for large codebases with frequent file changes
- Cross-platform watcher behavior — tests run on macOS (FSEvents); Linux (inotify) and Windows (ReadDirectoryChangesW) not verified here

## Notes for Tester

- Watcher tests require actual filesystem events from the OS — they cannot be mocked. The 500ms sleep is a pragmatic choice for FSEvents latency on macOS.
- The `ping` command acts as a trigger for `drain_watcher_events()` because the drain runs before every dispatch. Any command would work; ping is cheapest.
- If watcher tests are flaky, check `stderr` output for `[aft] invalidated N files` — if the log never appears, events aren't arriving from the OS within the sleep window.
