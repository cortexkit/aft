---
id: T02
parent: S01
milestone: M003
provides:
  - configure command sets project_root and initializes CallGraph
  - call_tree command returns nested cross-file call tree via binary protocol
  - Plugin tools aft_configure and aft_call_tree with Zod schemas
  - Multi-file TypeScript test fixtures for call graph integration testing
key_files:
  - src/context.rs
  - src/commands/configure.rs
  - src/commands/call_tree.rs
  - tests/integration/callgraph_test.rs
  - tests/fixtures/callgraph/
  - opencode-plugin-aft/src/tools/navigation.ts
key_decisions:
  - Config wrapped in RefCell<Config> in AppContext; all 13 call sites updated from ctx.config() (returning &Config) to &ctx.config() (borrowing through Ref<Config>)
  - CallGraph stored as RefCell<Option<CallGraph>> — None until configure is called, then Some(graph)
  - call_tree command validates symbol exists in file before traversal — returns symbol_not_found error early
patterns_established:
  - configure-then-use pattern — commands requiring CallGraph check Option and return not_configured error
  - Navigation tool naming uses aft_ prefix (aft_configure, aft_call_tree) to namespace agent-facing tools
observability_surfaces:
  - "[aft] project root set: <path>" stderr log on configure
  - call_tree response includes resolved: true/false per node; unresolved edges are leaf nodes with callee name but no file path
duration: 1 task
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Wire protocol commands, integration tests, and plugin tools

**Wired CallGraph engine into runtime with configure/call_tree commands, 7 integration tests proving cross-file traversal through binary protocol, and aft_configure/aft_call_tree plugin tools.**

## What Happened

1. **RefCell<Config> refactor** — Wrapped `Config` in `RefCell` in `AppContext`, added `config_mut()` returning `RefMut<Config>`. Updated all 13 existing call sites (across 13 command files) to borrow through the `Ref` wrapper. All 309 existing tests pass after refactor.

2. **configure command** (`src/commands/configure.rs`) — Accepts `project_root`, validates it's a directory, sets it on Config via `config_mut()`, initializes `CallGraph` in context, logs to stderr.

3. **call_tree command** (`src/commands/call_tree.rs`) — Accepts `file`, `symbol`, optional `depth` (default 5). Validates CallGraph is initialized, checks symbol exists in file, runs `forward_tree()`, returns nested JSON with name/file/line/signature/resolved/children.

4. **Multi-file fixtures** — 5 TypeScript files in `tests/fixtures/callgraph/`: main.ts (direct import), utils.ts (transitive), helpers.ts (local calls), index.ts (barrel re-export), aliased.ts (aliased import).

5. **Integration tests** — 7 tests in `callgraph_test.rs`: configure success, configure missing param, call_tree without configure, cross-file tree (main→processData→validate→checkFormat), depth limiting, unknown symbol error, aliased import resolution.

6. **Plugin tools** — `navigation.ts` defines `aft_configure` and `aft_call_tree` with Zod schemas. Wired into `index.ts`.

## Verification

- `cargo test -- callgraph_test` — 7/7 pass
- `cargo test` — 316 total (190 integration + 126 lib), all pass
- `bun test` in opencode-plugin-aft — 39/39 pass
- Stderr log `[aft] project root set: /tmp` confirmed via manual binary run

### Slice-level verification status
- ✅ `cargo test -- callgraph` — unit tests pass (from T01)
- ✅ `cargo test -- call_tree` — integration tests pass (7 new)
- ✅ `bun test` in opencode-plugin-aft — 39 pass
- ✅ All existing tests pass: cargo 316 (was 309), bun 39

## Diagnostics

- `call_tree` response nodes have `resolved: true/false` — unresolved edges are leaf nodes with callee name only
- `not_configured` error code if `call_tree` called before `configure`
- `symbol_not_found` error code with file context if symbol doesn't exist

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/context.rs` — Config wrapped in RefCell, added config_mut(), callgraph field + accessor
- `src/commands/configure.rs` — new configure command handler
- `src/commands/call_tree.rs` — new call_tree command handler
- `src/commands/mod.rs` — added call_tree and configure module declarations
- `src/main.rs` — wired configure and call_tree in dispatch
- `src/commands/add_decorator.rs` — updated ctx.config() → &ctx.config()
- `src/commands/add_derive.rs` — updated ctx.config() → &ctx.config()
- `src/commands/add_import.rs` — updated ctx.config() → &ctx.config()
- `src/commands/add_member.rs` — updated ctx.config() → &ctx.config()
- `src/commands/add_struct_tags.rs` — updated ctx.config() → &ctx.config()
- `src/commands/batch.rs` — updated ctx.config() → &ctx.config()
- `src/commands/edit_match.rs` — updated ctx.config() → &ctx.config()
- `src/commands/edit_symbol.rs` — updated ctx.config() → &ctx.config()
- `src/commands/organize_imports.rs` — updated ctx.config() → &ctx.config()
- `src/commands/remove_import.rs` — updated ctx.config() → &ctx.config()
- `src/commands/transaction.rs` — updated ctx.config() → &ctx.config()
- `src/commands/wrap_try_catch.rs` — updated ctx.config() → &ctx.config()
- `src/commands/write.rs` — updated ctx.config() → &ctx.config()
- `tests/fixtures/callgraph/main.ts` — fixture: imports processData, calls it
- `tests/fixtures/callgraph/utils.ts` — fixture: imports validate, exports processData
- `tests/fixtures/callgraph/helpers.ts` — fixture: exports validate, local checkFormat
- `tests/fixtures/callgraph/index.ts` — fixture: barrel re-export
- `tests/fixtures/callgraph/aliased.ts` — fixture: aliased import
- `tests/integration/callgraph_test.rs` — 7 integration tests
- `tests/integration/main.rs` — registered callgraph_test module
- `opencode-plugin-aft/src/tools/navigation.ts` — aft_configure and aft_call_tree tools
- `opencode-plugin-aft/src/index.ts` — wired navigation tools
