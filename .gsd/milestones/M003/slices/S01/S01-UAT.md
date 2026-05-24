# S01: Call Graph Infrastructure + Forward Call Tree — UAT

**Milestone:** M003
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All capabilities are verifiable through binary protocol round-trips and automated tests. No UI or human-experience component.

## Preconditions

- `cargo build` succeeds (binary at `target/debug/aft`)
- `tests/fixtures/callgraph/` directory exists with 5 TypeScript files (main.ts, utils.ts, helpers.ts, index.ts, aliased.ts)
- Working directory is the project root

## Smoke Test

Send `configure` + `call_tree` through binary and receive a multi-node cross-file tree:
```bash
PROJ="$(pwd)/tests/fixtures/callgraph"
printf '{"id":"1","command":"configure","project_root":"%s"}\n{"id":"2","command":"call_tree","file":"%s/main.ts","symbol":"main"}\n' "$PROJ" "$PROJ" | ./target/debug/aft 2>/dev/null
```
**Expected:** Two JSON responses. Second has `ok: true` with nested `children` containing at least one node with a different `file` than `main.ts`.

## Test Cases

### 1. Configure sets project root

1. Send `{"id":"1","command":"configure","project_root":"<abs_path_to_fixtures>"}` to binary
2. Check stderr output
3. **Expected:** Response `ok: true` with `project_root` field matching the path. Stderr contains `[aft] project root set: <path>`.

### 2. Configure rejects missing directory

1. Send `{"id":"1","command":"configure","project_root":"/nonexistent/path/that/does/not/exist"}`
2. **Expected:** Response `ok: false`, code `invalid_request`, message mentions "not a directory".

### 3. Configure rejects missing param

1. Send `{"id":"1","command":"configure"}` (no project_root field)
2. **Expected:** Response `ok: false`, code `invalid_request`, message mentions "missing required param 'project_root'".

### 4. Cross-file forward call tree

1. Configure with `tests/fixtures/callgraph/` as project root
2. Send `call_tree` with `file: main.ts`, `symbol: main`
3. **Expected:** Root node is `main` in `main.ts` with `resolved: true`. Children include `processData` in `utils.ts` (`resolved: true`). processData's children include `validate` in `helpers.ts` (`resolved: true`). Each resolved node has a `signature` field.

### 5. Depth limiting

1. Configure with fixtures
2. Send `call_tree` with `file: main.ts`, `symbol: main`, `depth: 1`
3. **Expected:** Root node is `main`, children include `processData`, but processData's children are empty (depth truncated at 1 level below root).

### 6. Unknown symbol error

1. Configure with fixtures
2. Send `call_tree` with `file: main.ts`, `symbol: nonExistentFunction`
3. **Expected:** Response `ok: false`, code `symbol_not_found`.

### 7. call_tree without configure

1. Start fresh binary (no configure)
2. Send `call_tree` with any file and symbol
3. **Expected:** Response `ok: false`, code `not_configured`.

### 8. Aliased import resolution

1. Configure with fixtures
2. Send `call_tree` with `file: aliased.ts`, `symbol: useAliased`
3. **Expected:** Tree resolves through the aliased import — child node references the original function (e.g., `processData` in `utils.ts`) with `resolved: true`.

### 9. Unresolved edges marked

1. Configure with fixtures
2. Send `call_tree` with `file: helpers.ts`, `symbol: validate`
3. **Expected:** `checkFormat` appears as a child with `resolved: false` (it's a local call to a function that calls library functions which can't be resolved cross-file).

## Edge Cases

### Cycle detection (A → B → A)

1. Create two fixture files where function A calls function B and function B calls function A
2. Configure and call `call_tree` on A with depth 10
3. **Expected:** Tree terminates — no infinite recursion. Each function appears at most once per branch path. The cycle-terminating node has empty `children`.

### Depth 0 returns root only

1. Configure with fixtures
2. Send `call_tree` with `file: main.ts`, `symbol: main`, `depth: 0`
3. **Expected:** Response contains root node with `name: "main"` and empty `children` array.

### Gitignore exclusion

1. Create a `.gitignore` in a temp project root that ignores a subdirectory
2. Place TypeScript files in both the root and the ignored subdirectory
3. Configure with the temp root, query a function that appears to call something in the ignored dir
4. **Expected:** The call to the ignored file appears as `resolved: false` — the ignored file is not walked.

### node_modules exclusion

1. Create a `node_modules/` directory inside a temp project root with TypeScript files
2. Configure and query
3. **Expected:** Files in `node_modules/` are never included in resolution. Calls to node_modules functions appear as unresolved leaves.

## Failure Signals

- `call_tree` response with `ok: false` and code other than `symbol_not_found` or `not_configured` — indicates engine error
- All children showing `resolved: false` when cross-file resolution should work — likely broken import parsing
- Infinite hang on `call_tree` — cycle detection failure
- `cargo test -- callgraph` failures — regression in core engine
- Plugin `bun test` failures on navigation tool registration — schema or wiring issue
- Missing `signature` field on resolved nodes — symbol extraction not populating signatures

## Requirements Proved By This UAT

- R020 (Call graph construction) — Lazy per-file construction proven via configure + call_tree round-trips; worktree scoping proven via gitignore/node_modules exclusion tests. File watcher not proven (S02).
- R021 (Forward call tree) — Cross-file depth-limited forward tree with resolved paths, signatures, cycle detection proven through test cases 4, 5, 8, 9, and edge cases.
- R027 (Worktree-aware scoping) — .gitignore respect and node_modules exclusion proven through edge case tests.

## Not Proven By This UAT

- File watcher invalidation (R020 partial — deferred to S02)
- Reverse callers (R022 — S02)
- Trace to entry points (R023 — S03)
- Data flow tracking (R024 — S04)
- Impact analysis (R025 — S04)
- Entry point detection heuristics (R026 — S03)
- Plugin tool round-trip for call_tree through the full plugin→binary→response stack (proven by bun tests, not repeated here)

## Notes for Tester

- All test cases use flattened JSON format (params at top level, not nested under `params` key) — e.g., `{"id":"1","command":"call_tree","file":"...","symbol":"..."}` not `{"id":"1","command":"call_tree","params":{"file":"..."}}`
- File paths in `call_tree` responses are relative to the project root set by `configure`
- The `signature` field contains the function signature as it appears in source (e.g., `function processData(input: string): string`)
- Binary logs to stderr — redirect with `2>/dev/null` or `2>/tmp/aft.log` for clean JSON output
