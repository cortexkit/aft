---
estimated_steps: 3
estimated_files: 3
---

# T03: Plugin tool aft_move_symbol with Zod schema and bun test

**Slice:** S01 — Move Symbol with Import Rewiring
**Milestone:** M004

## Description

Register `aft_move_symbol` as an OpenCode plugin tool with a Zod schema, creating a new `refactoring.ts` tool group that S02 will extend with `aft_extract_function` and `aft_inline_symbol`. Add a bun test that exercises the full plugin → binary → response round-trip for a basic move_symbol operation.

## Steps

1. **Create `opencode-plugin-aft/src/tools/refactoring.ts`** following the pattern in `navigation.ts`. Export `refactoringTools(bridge)` returning `{ aft_move_symbol }`. Define the Zod schema: `file` (string, required — source file), `symbol` (string, required — symbol name), `destination` (string, required — target file path), `scope` (string, optional — disambiguation), `dry_run` (boolean, optional). Use `const z = tool.schema;` per D034. Execute function sends `move_symbol` command via `bridge.send()`.

2. **Register in `opencode-plugin-aft/src/index.ts`.** Import `refactoringTools` from `./tools/refactoring.js`. Spread into the tool object alongside existing tool groups. Update the JSDoc tool categories comment.

3. **Add bun test in `opencode-plugin-aft/src/__tests__/tools.test.ts`.** Test: create a temp directory with a source file (containing one exported function) and a consumer file (importing that function). Create bridge, call `aft_configure` with the temp dir, then call `aft_move_symbol` to move the function to a new destination file. Parse the JSON response and assert `ok: true`, `files_modified >= 2`. Verify the response structure includes expected fields.

## Must-Haves

- [ ] `aft_move_symbol` tool definition with Zod schema matching the binary's expected params
- [ ] Tool registered in plugin index and appears in the tool list
- [ ] Bun test proves round-trip: plugin → binary → response with `ok: true`
- [ ] Uses `const z = tool.schema;` (D034), not direct zod import

## Verification

- `cd opencode-plugin-aft && bun test` — new test passes alongside existing tests
- `bun build` or TypeScript compilation succeeds without type errors

## Inputs

- `opencode-plugin-aft/src/tools/navigation.ts` — reference for tool definition pattern
- `opencode-plugin-aft/src/index.ts` — registration pattern
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — existing test patterns
- `src/commands/move_symbol.rs` — command params to match in Zod schema

## Expected Output

- `opencode-plugin-aft/src/tools/refactoring.ts` — new tool group with `aft_move_symbol`
- `opencode-plugin-aft/src/index.ts` — updated with refactoring tools import
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — updated with move_symbol round-trip test

## Observability Impact

- **Tool discovery:** `aft_move_symbol` appears in the plugin's tool registry. A future agent can verify by inspecting the return value of `refactoringTools(bridge)` — it should contain an `aft_move_symbol` key with `description`, `args`, and `execute`.
- **Round-trip signal:** The bun test proves the full plugin → binary → JSON response pipeline. If the binary protocol changes (param names, response shape), this test fails with a parseable assertion error showing expected vs actual.
- **Response diagnostics:** The test asserts on `ok`, `files_modified`, `consumers_updated`, and `checkpoint_name` — the same fields a future agent would use to verify a move operation succeeded.
- **Failure visibility:** If the Zod schema drifts from the binary's expected params, the binary returns `ok: false` with an `invalid_request` code and a message naming the missing/invalid param.
