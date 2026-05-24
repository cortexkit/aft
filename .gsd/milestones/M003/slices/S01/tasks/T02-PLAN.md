---
estimated_steps: 8
estimated_files: 11
---

# T02: Wire protocol commands, integration tests, and plugin tools

**Slice:** S01 — Call Graph Infrastructure + Forward Call Tree
**Milestone:** M003

## Description

Wire the `CallGraph` engine from T01 into the runtime: add `CallGraph` and mutable `Config` to `AppContext`, implement `configure` and `call_tree` protocol commands, create multi-file test fixtures, write integration tests proving cross-file call trees through the binary, and register plugin tools.

## Steps

1. Update `src/config.rs` — no structural changes needed, but verify `project_root` field exists. Update `src/context.rs` — wrap `Config` in `RefCell<Config>` (D029 pattern), add `config_mut()` returning `RefMut<Config>`, update `config()` to return `Ref<Config>`. Add `RefCell<CallGraph>` field. Update all existing `ctx.config()` call sites to handle the `Ref<Config>` return (`.project_root` etc. accessed through the `Ref`). Verify `cargo test` still passes after this refactor.
2. Create `src/commands/configure.rs` — `handle_configure(req, ctx)` that reads `project_root` from params, sets it on `Config` via `ctx.config_mut()`, initializes `CallGraph` worktree state, returns success with the configured root path. Log to stderr: `[aft] project root set: <path>`.
3. Create `src/commands/call_tree.rs` — `handle_call_tree(req, ctx)` that reads `file`, `symbol`, optional `depth` (default 5) from params. Borrows `CallGraph` from context, calls `forward_tree()`, serializes the result tree to JSON with fields: `name`, `file`, `line`, `signature`, `resolved`, `children`. Returns error if symbol not found.
4. Update `src/commands/mod.rs` to add `pub mod configure;` and `pub mod call_tree;`. Update `src/main.rs` dispatch to wire `"configure"` and `"call_tree"` commands.
5. Create multi-file TypeScript test fixtures in `tests/fixtures/callgraph/`:
   - `main.ts` — imports `processData` from `./utils`, calls it
   - `utils.ts` — imports `validate` from `./helpers`, exports `processData` which calls `validate`
   - `helpers.ts` — exports `validate` which calls a local `checkFormat` function
   - `index.ts` — barrel re-export: `export { processData } from './utils'`
   - `aliased.ts` — `import { validate as checker } from './helpers'`, calls `checker()`
6. Write `tests/integration/callgraph_test.rs`: test `configure` sets project root and responds ok; test `call_tree` on `main.ts:processData` returns tree with cross-file children (utils.ts → helpers.ts); test depth limit truncates; test unknown symbol returns error; test aliased import resolution works.
7. Register the integration test module in `tests/integration/main.rs`.
8. Create `opencode-plugin-aft/src/tools/navigation.ts` — define `aft_configure` tool (args: `project_root`) and `aft_call_tree` tool (args: `file`, `symbol`, optional `depth`). Register in `opencode-plugin-aft/src/index.ts`. Run `bun test` to verify schema validation.

## Must-Haves

- [ ] `Config` wrapped in `RefCell` in AppContext — all existing call sites updated
- [ ] `configure` command sets `project_root` and returns success
- [ ] `call_tree` command returns nested cross-file call tree JSON
- [ ] Multi-file test fixtures exercise direct imports, aliased imports, and re-exports
- [ ] Integration tests prove cross-file tree, depth limiting, and error paths through binary protocol
- [ ] Plugin tools registered with Zod schemas for `aft_call_tree` and `aft_configure`
- [ ] All existing tests (cargo 294+ and bun 39+) still pass

## Verification

- `cargo test -- callgraph_test` — new integration tests pass
- `cargo test` — all tests pass (existing + new)
- `bun test` in opencode-plugin-aft — all tests pass (existing + new navigation tool tests)

## Observability Impact

- Signals added/changed: `[aft] project root set: <path>` stderr log on configure
- How a future agent inspects this: `call_tree` response includes `resolved: true/false` per edge and unresolved callees listed separately
- Failure state exposed: unresolved cross-file edges appear as leaf nodes with `resolved: false` and `callee_name` only

## Inputs

- `src/callgraph.rs` — `CallGraph` engine from T01
- `src/calls.rs` — shared call helpers from T01
- `src/context.rs` — existing AppContext pattern (D025, D029)
- `opencode-plugin-aft/src/tools/reading.ts` — pattern for tool registration

## Expected Output

- `src/context.rs` — updated with `RefCell<Config>`, `RefCell<CallGraph>`, new accessors
- `src/commands/configure.rs` — new command handler
- `src/commands/call_tree.rs` — new command handler
- `src/main.rs` — dispatch entries for `configure` and `call_tree`
- `tests/fixtures/callgraph/` — 5 TypeScript fixture files
- `tests/integration/callgraph_test.rs` — 5+ integration tests
- `opencode-plugin-aft/src/tools/navigation.ts` — plugin tool definitions
- `opencode-plugin-aft/src/index.ts` — navigation tools wired in
