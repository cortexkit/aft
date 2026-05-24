# M003: Call Graph Navigation

**Vision:** Single-call code navigation primitives that replace the most token-expensive agent workflow — tracing call chains across files — with five commands that return complete results in ~400 tokens instead of ~5000.

## Success Criteria

- Agent calls `aft_call_tree` on a function in a multi-file project and receives a cross-file call tree with resolved file paths, function signatures, and depth-limited traversal — in one call
- Agent calls `aft_callers` on a utility function and receives all call sites grouped by file, then modifies a calling file, and a subsequent `aft_callers` query reflects the change without binary restart
- Agent calls `aft_trace_to` on a deeply-nested utility and receives all paths from entry points (exported functions, main) rendered top-down with data threading
- Agent calls `aft_impact` on a function with 5+ callers across 3+ files and receives all affected call sites with update suggestions
- Agent calls `aft_trace_data` on an expression and sees variable renames and type transformations through the call chain
- All five commands respect worktree boundaries — no results from node_modules, target/, venv/, or .gitignore'd paths

## Key Risks / Unknowns

- **Cross-file symbol resolution accuracy** — following import/export chains across files (barrel re-exports, aliased imports, `__init__.py` re-exports) is non-trivial. Tree-sitter can't resolve types, so resolution is path-based and approximate. Target ~70% for cross-file edges.
- **File watcher + single-threaded architecture** — `notify` crate runs on its own OS thread but AppContext uses RefCell (D001, D014, D029). Concurrent access would panic. The drain-at-dispatch pattern must be proven.
- **Entry point detection heuristics** — framework-specific patterns (Express, Flask, Axum) are fragile. Generic patterns (exports, main, test) may be too broad.
- **Performance on first cold query** — lazy construction helps, but `trace_to` could scan 50+ files backward on first use. Depth limits and caching must keep this under 2s for typical projects.

## Proof Strategy

- Cross-file resolution accuracy → retire in S01 by shipping `call_tree` that resolves calls across files using imports.rs, with multi-file test fixtures proving resolution of direct imports, aliased imports, and re-exports
- File watcher threading → retire in S02 by shipping `callers` with live file watcher, proving modify-file-then-query cycle works without concurrent access panics
- Entry point detection → retire in S03 by shipping `trace_to` with generic heuristics (exports, main, test), proving backward traversal stops at correct entry points in multi-file fixtures
- Cold query performance → retire in S01 with depth limits (default 5) on all traversals, verified by integration tests with depth parameter

## Verification Classes

- Contract verification: Rust integration tests via AftProcess harness (existing pattern), multi-file fixture directories in `tests/fixtures/callgraph/`
- Integration verification: plugin tool round-trips through binary for all 5 new commands (same pattern as M001/M002)
- Operational verification: file watcher invalidation cycle — modify file on disk, query reflects change
- UAT / human verification: none (all verifiable through automated tests)

## Milestone Definition of Done

This milestone is complete only when all are true:

- All 5 new commands (`call_tree`, `callers`, `trace_to`, `trace_data`, `impact`) work through binary protocol AND plugin tool registration
- Cross-file call resolution correctly follows import/export chains in multi-file test fixtures
- File watcher detects modified files and subsequent queries reflect changes
- All graph traversals have cycle detection and depth limits — no infinite loops
- Worktree scoping excludes .gitignore'd paths, node_modules, target/, venv/
- `cargo test` passes with 0 failures (existing 294 + new M003 tests)
- `bun test` passes with 0 failures (existing 39 + new M003 tool tests)
- Final acceptance scenarios from CONTEXT pass: trace_to on deeply-nested utility, callers after file modification, impact on 5+ callers across 3+ files

## Requirement Coverage

- Covers: R020 (call graph construction), R021 (forward call tree), R022 (reverse callers), R023 (trace to entry points), R024 (data flow tracking), R025 (impact analysis), R026 (entry point detection), R027 (worktree-aware scoping)
- Partially covers: none
- Leaves for later: none
- Orphan risks: none — all 8 active requirements for M003 are mapped

## Slices

- [x] **S01: Call Graph Infrastructure + Forward Call Tree** `risk:high` `depends:[]`
  > After this: agent calls `aft_call_tree` on a function in a multi-file TypeScript project and receives a cross-file call tree with resolved paths, signatures, and depth-limited traversal — proven by integration tests with multi-file fixtures and plugin tool round-trip
- [x] **S02: Reverse Callers + File Watcher** `risk:high` `depends:[S01]`
  > After this: agent calls `aft_callers` on a function and sees all call sites grouped by file; after modifying a calling file on disk, a subsequent query reflects the change — proven by integration tests exercising the file watcher invalidation cycle
- [x] **S03: Trace to Entry Points** `risk:medium` `depends:[S02]`
  > After this: agent calls `aft_trace_to` on a deeply-nested utility and receives all paths from entry points (exported functions, main, test functions) rendered top-down — proven by integration tests with multi-layer call chains
- [x] **S04: Data Flow Tracking + Impact Analysis** `risk:medium` `depends:[S03]`
  > After this: agent calls `aft_trace_data` to follow a value through function calls seeing variable renames, and calls `aft_impact` to see all call sites affected by a signature change with update suggestions — proven by integration tests and plugin tool round-trips for both commands

## Boundary Map

### S01 → S02

Produces:
- `src/callgraph.rs` — `CallGraph` struct with `HashMap<PathBuf, FileCallData>` storing call sites and exported symbols per file, `build_file()` for lazy per-file construction, `resolve_cross_file_edge()` for import-based resolution, `forward_tree()` for depth-limited forward traversal with cycle detection
- `src/commands/call_tree.rs` — `handle_call_tree()` command handler returning nested call tree JSON
- `src/commands/configure.rs` — `handle_configure()` that sets `Config.project_root` for worktree scoping
- `CallGraph` stored in `AppContext` as `RefCell<CallGraph>` following D029 pattern
- Worktree-aware file walking via `ignore` crate integrated into `CallGraph`
- `opencode-plugin-aft/src/tools/navigation.ts` — plugin tool registration pattern for navigation commands

Consumes:
- `src/commands/zoom.rs` — `extract_calls_in_range`, `walk_for_calls`, `extract_callee_name`, `call_node_kinds` (extracted to shared module)
- `src/imports.rs` — `parse_file_imports()` for cross-file symbol resolution
- `src/parser.rs` — `FileParser` for parse tree access
- `src/context.rs` — `AppContext` pattern for new store

### S02 → S03

Produces:
- Reverse index in `CallGraph` — `callers_of(symbol, file)` returning all call sites grouped by file
- `src/commands/callers.rs` — `handle_callers()` command handler
- File watcher integration — `notify` crate on background thread, `crossbeam-channel` for event delivery, drain-at-dispatch in `main.rs`
- Graph invalidation — `CallGraph::invalidate_file(path)` marks file nodes as stale

Consumes:
- `src/callgraph.rs` — forward graph data structure and resolution logic from S01
- `src/main.rs` — dispatch loop (watcher drain hook added before dispatch)

### S03 → S04

Produces:
- Entry point detection — `is_entry_point(symbol)` heuristics for exported functions, main/init, test functions, framework handlers
- `src/commands/trace_to.rs` — `handle_trace_to()` with backward traversal from target to entry points, rendered top-down with data threading
- Backward traversal algorithm using reverse index from S02

Consumes:
- `src/callgraph.rs` — forward graph + reverse index from S01/S02
- `src/commands/callers.rs` — reverse lookup proven in S02

### S04 (terminal)

Produces:
- `src/commands/trace_data.rs` — `handle_trace_data()` tracking values through assignments and function parameters
- `src/commands/impact.rs` — `handle_impact()` analyzing affected call sites with update suggestions
- Plugin tool registration for both `aft_trace_data` and `aft_impact`
- All 5 navigation commands complete

Consumes:
- Full call graph infrastructure from S01-S03 (forward tree, reverse index, entry points)
