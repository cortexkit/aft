# S01: Call Graph Infrastructure + Forward Call Tree

**Goal:** Build a lazy, worktree-scoped call graph engine that resolves calls across files using import chains, and expose a `call_tree` command returning depth-limited forward call trees through the binary protocol and plugin.

**Demo:** Agent calls `aft_call_tree` on a function in a multi-file TypeScript project and receives a cross-file call tree with resolved file paths, function signatures, and depth-limited traversal — proven by integration tests with multi-file fixtures and plugin tool round-trip.

## Must-Haves

- `CallGraph` struct with lazy per-file construction and `HashMap<PathBuf, FileCallData>` storage
- Cross-file edge resolution using `parse_file_imports()` from imports.rs — handles direct imports, aliased imports, and re-exports
- Worktree-scoped file walking via `ignore` crate respecting .gitignore, excluding node_modules/target/venv
- `forward_tree()` traversal with cycle detection and configurable depth limit (default 5)
- `call_tree` protocol command returning nested JSON with callee name, file path, line, signature, and children
- `configure` protocol command setting `Config.project_root` from plugin at startup
- `CallGraph` stored in `AppContext` as `RefCell<CallGraph>` (D029 pattern)
- Call extraction logic shared between zoom.rs and callgraph.rs (extracted to common location)
- Plugin tool registration for `aft_call_tree` and `aft_configure`
- Multi-file test fixtures proving cross-file resolution (direct import, aliased import, re-export)

## Proof Level

- This slice proves: contract + integration
- Real runtime required: yes (binary protocol round-trips)
- Human/UAT required: no

## Verification

- `cargo test -- callgraph` — unit tests for graph construction, cross-file resolution, worktree scoping, forward traversal with cycle detection and depth limits
- `cargo test -- call_tree` — integration tests through binary protocol with multi-file fixtures proving cross-file call tree, depth limiting, and error paths
- `bun test` in opencode-plugin-aft — plugin tool round-trip for `call_tree` and `configure`
- All existing tests pass: `cargo test` (294+), `bun test` (39+)

## Observability / Diagnostics

- Runtime signals: `[aft]` stderr logs for configure (project_root set), callgraph cache hits/misses
- Inspection surfaces: `call_tree` response includes `resolved: true/false` per edge, `unresolved_calls` list for edges that couldn't be followed across files
- Failure visibility: cross-file resolution failures appear as unresolved leaf nodes with callee name but no file path — agent sees exactly where the graph is approximate

## Integration Closure

- Upstream surfaces consumed: `src/imports.rs` (`parse_file_imports`), `src/commands/zoom.rs` (call extraction helpers), `src/parser.rs` (`FileParser`, `LangId`, `detect_language`, `grammar_for`), `src/context.rs` (`AppContext` pattern)
- New wiring introduced in this slice: `callgraph.rs` module, `configure` + `call_tree` command dispatch, `RefCell<CallGraph>` in AppContext, `RefCell<Config>` replacing bare Config, `ignore` crate dependency, plugin `navigation.ts` tool file
- What remains before the milestone is truly usable end-to-end: reverse callers (S02), trace_to entry points (S03), data flow + impact (S04)

## Tasks

- [x] **T01: Build call graph engine with cross-file resolution and forward traversal** `est:3h`
  - Why: The core data structure, worktree scoping, cross-file resolution, and traversal algorithm are the foundation everything else builds on. Unit-testable in isolation.
  - Files: `Cargo.toml`, `src/callgraph.rs` (new), `src/calls.rs` (new, extracted from zoom.rs), `src/commands/zoom.rs`, `src/lib.rs`
  - Do: Add `ignore` crate dependency. Extract `call_node_kinds`, `walk_for_calls`, `extract_callee_name`, `extract_last_segment` from zoom.rs into a new `src/calls.rs` shared module (zoom.rs re-imports from there). Build `src/callgraph.rs` with: `FileCallData` (call sites + exported symbols per file), `CallGraph` struct wrapping `HashMap<PathBuf, FileCallData>`, `build_file()` for lazy per-file AST parsing and call extraction, `resolve_cross_file_edge()` using `parse_file_imports()` to follow import chains, `WalkerConfig` using `ignore::WalkBuilder` for worktree-scoped file discovery respecting .gitignore, `forward_tree()` depth-limited recursive traversal with `HashSet`-based cycle detection. Unit tests covering: single-file call extraction, cross-file resolution (direct import, aliased import, re-export), cycle detection (A→B→A stops), depth limiting, worktree boundary exclusion (node_modules, .gitignore'd paths).
  - Verify: `cargo test -- callgraph` passes all unit tests; `cargo test` existing 294+ tests still pass (zoom.rs refactor doesn't break anything)
  - Done when: `CallGraph::forward_tree()` returns correct cross-file trees in unit tests with depth limits and cycle detection, and zoom.rs tests still pass after call extraction refactor

- [x] **T02: Wire protocol commands, integration tests, and plugin tools** `est:2h`
  - Why: The engine from T01 needs protocol exposure, AppContext integration, and end-to-end proof through the binary and plugin.
  - Files: `src/context.rs`, `src/config.rs`, `src/commands/configure.rs` (new), `src/commands/call_tree.rs` (new), `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/callgraph/` (new directory with multi-file fixtures), `tests/integration/callgraph_test.rs` (new), `tests/integration/main.rs`, `opencode-plugin-aft/src/tools/navigation.ts` (new), `opencode-plugin-aft/src/index.ts`
  - Do: Wrap `Config` in `RefCell<Config>` in AppContext (D029 pattern) — update `config()` accessor to return `Ref<Config>`, add `config_mut()` returning `RefMut<Config>`. Build `configure` command handler that sets `project_root` and initializes `CallGraph` worktree walker. Build `call_tree` command handler that takes `file`, `symbol`, optional `depth` (default 5), calls `forward_tree()`, returns nested JSON. Store `CallGraph` as `RefCell<CallGraph>` in AppContext. Wire both commands in `main.rs` dispatch. Create multi-file TypeScript test fixtures in `tests/fixtures/callgraph/` (main.ts importing from utils.ts, utils.ts importing from helpers.ts, barrel re-export through index.ts). Write integration tests: call_tree returns cross-file tree, depth limit truncates, unknown symbol returns error, configure sets project_root. Write plugin `navigation.ts` with `aft_call_tree` and `aft_configure` tool definitions and Zod schemas. Register in index.ts.
  - Verify: `cargo test -- callgraph` integration tests pass; `bun test` plugin tests pass; `cargo test` all 294+ existing tests pass; `bun test` all 39+ existing tests pass
  - Done when: Integration test sends `call_tree` through binary and receives a cross-file tree with resolved file paths, signatures, and depth-limited traversal; plugin tool schema validates and round-trips through the bridge

## Files Likely Touched

- `Cargo.toml`
- `src/callgraph.rs` (new)
- `src/calls.rs` (new — extracted call helpers)
- `src/context.rs`
- `src/config.rs`
- `src/commands/zoom.rs` (refactor to use shared calls.rs)
- `src/commands/configure.rs` (new)
- `src/commands/call_tree.rs` (new)
- `src/commands/mod.rs`
- `src/main.rs`
- `src/lib.rs`
- `tests/fixtures/callgraph/` (new multi-file fixtures)
- `tests/integration/callgraph_test.rs` (new)
- `tests/integration/main.rs`
- `opencode-plugin-aft/src/tools/navigation.ts` (new)
- `opencode-plugin-aft/src/index.ts`
