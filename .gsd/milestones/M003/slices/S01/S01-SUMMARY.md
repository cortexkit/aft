---
id: S01
parent: M003
milestone: M003
provides:
  - CallGraph engine with lazy per-file construction and cross-file edge resolution
  - Depth-limited forward call tree traversal with cycle detection
  - Worktree-scoped file discovery via ignore crate respecting .gitignore
  - Shared call extraction helpers (calls.rs extracted from zoom.rs)
  - configure command setting project_root and initializing CallGraph
  - call_tree protocol command returning nested cross-file call trees
  - Plugin tools aft_configure and aft_call_tree with Zod schemas
  - Multi-file TypeScript test fixtures for call graph integration testing
requires:
  - slice: none
    provides: foundation (M001/M002 complete — AppContext, protocol, parser, imports.rs)
affects:
  - S02 (consumes CallGraph struct, forward graph data, resolution logic)
  - S03 (consumes forward graph + reverse index)
  - S04 (consumes full call graph infrastructure)
key_files:
  - src/callgraph.rs
  - src/calls.rs
  - src/commands/configure.rs
  - src/commands/call_tree.rs
  - tests/integration/callgraph_test.rs
  - tests/fixtures/callgraph/
  - opencode-plugin-aft/src/tools/navigation.ts
key_decisions:
  - D083: Call extraction extracted to shared calls.rs module
  - D084: Config wrapped in RefCell for runtime mutation via configure command
  - D085: ignore crate 0.4.x for worktree-scoped file walking
  - D086: Aliased import resolution via raw text parsing (avoids modifying ImportStatement)
  - D087: Full callee expression in calls.rs for namespace-aware resolution
  - D088: CallGraph stored as RefCell<Option<CallGraph>> — None until configure, then Some
  - D089: configure-then-use pattern — commands check Option, return not_configured error
patterns_established:
  - extract_calls_full returns (full_callee, short_name, line) triples for namespace-aware resolution
  - EdgeResolution enum marks unresolved edges explicitly — never silently dropped
  - walk_project_files uses ignore crate with hardcoded exclusions for node_modules/target/venv/.git
  - configure-then-use pattern — CallGraph commands return not_configured error before configure is called
  - Navigation tool naming uses aft_ prefix (aft_configure, aft_call_tree)
observability_surfaces:
  - "[aft] project root set: <path>" stderr log on configure
  - call_tree response nodes have resolved:true/false — unresolved edges are leaf nodes with callee name only
  - not_configured error code if call_tree called before configure
  - symbol_not_found error code with file context if symbol doesn't exist
drill_down_paths:
  - .gsd/milestones/M003/slices/S01/tasks/T01-SUMMARY.md
  - .gsd/milestones/M003/slices/S01/tasks/T02-SUMMARY.md
duration: 2 tasks (~2h)
verification_result: passed
completed_at: 2026-03-14
---

# S01: Call Graph Infrastructure + Forward Call Tree

**Lazy cross-file call graph engine with depth-limited forward traversal, worktree scoping, and `call_tree`/`configure` protocol commands proven through 22 new tests and plugin tool round-trips.**

## What Happened

Extracted shared call-site helpers from `zoom.rs` into `src/calls.rs` (T01) — `call_node_kinds`, `walk_for_calls`, `extract_callee_name`, `extract_last_segment`, plus new `extract_calls_full` and `extract_full_callee` for namespace-qualified call detection (`utils.foo`). All existing zoom tests pass unchanged.

Built `src/callgraph.rs` (T01) with the core engine: `FileCallData` stores per-file call sites grouped by symbol, exported symbol names, and parsed imports. `CallGraph` wraps `HashMap<PathBuf, FileCallData>` with lazy `build_file()` using tree-sitter for symbol listing and calls.rs for extraction. Cross-file resolution follows direct named imports, aliased imports (parses `{foo as bar}` from raw import text), namespace imports (`import * as utils → utils.foo`), and barrel file re-exports. `forward_tree()` does depth-limited recursive traversal with `HashSet`-based cycle detection. `walk_project_files()` uses `ignore::WalkBuilder` respecting .gitignore with hardcoded exclusions.

Wired the engine into runtime (T02): wrapped `Config` in `RefCell` in `AppContext` (updating all 13 existing call sites), added `CallGraph` as `RefCell<Option<CallGraph>>`. Built `configure` command (sets project_root, initializes graph) and `call_tree` command (validates symbol exists, runs `forward_tree()`, returns nested JSON with name/file/line/signature/resolved/children). Created multi-file TypeScript fixtures (5 files: direct import, transitive, local calls, barrel re-export, aliased import). Plugin `navigation.ts` registers `aft_configure` and `aft_call_tree` with Zod schemas.

## Verification

- `cargo test -- callgraph`: 15 unit tests (single-file extraction, exports, direct/namespace/aliased resolution, unresolved marking, cycle detection, depth limiting, cross-file tree, gitignore exclusion, alias parsing) + 7 integration tests (configure success/error, call_tree cross-file/depth-limit/unknown-symbol/aliased/not-configured) — all pass
- `cargo test`: 316 total (190 lib + 126 integration), all pass (was 309 before S01)
- `bun test`: 39/39 pass (unchanged count, navigation tools registered)
- Observability confirmed: stderr log `[aft] project root set: <path>`, `resolved: true/false` per node in call_tree response, `not_configured` and `symbol_not_found` error codes

## Requirements Advanced

- R020 (Call graph construction) — Lazy per-file construction proven with `HashMap<PathBuf, FileCallData>`, worktree scoping via `ignore` crate. File watcher (incremental invalidation) deferred to S02.
- R021 (Forward call tree) — `call_tree` command returns cross-file depth-limited trees with resolved paths, signatures, and cycle detection. Proven through 7 integration tests.
- R027 (Worktree-aware scoping) — `walk_project_files()` respects .gitignore, excludes node_modules/target/venv/.git. Proven by unit tests with gitignore fixtures.

## Requirements Validated

- R021 (Forward call tree) — Full forward call tree with cross-file resolution, depth limiting, cycle detection, and error paths proven through binary protocol integration tests and plugin tool registration
- R027 (Worktree-aware scoping) — Worktree-scoped file walking proven via ignore crate integration with .gitignore respect and hardcoded exclusions

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- Added `extract_calls_full` and `extract_full_callee` to `calls.rs` (not in original plan) — needed for namespace import resolution where the full callee expression matters
- Aliased import detection uses text parsing rather than extending ImportStatement struct — avoids modifying the shared import engine's data model (D086)

## Known Limitations

- Barrel file re-export resolution is shallow — checks if the index file directly exports the symbol, doesn't follow re-export chains through multiple levels
- Aliased import detection uses `" as "` text pattern which could false-match in edge cases with comments or string literals in import statements (extremely unlikely in practice)
- `call_tree` requires absolute paths for file and project_root parameters
- No file watcher yet — graph reflects state at query time but doesn't auto-invalidate on file changes (S02)
- No reverse callers, trace_to, or impact analysis yet (S02–S04)

## Follow-ups

- none — all planned work completed, remaining capabilities ship in S02–S04

## Files Created/Modified

- `Cargo.toml` — added `ignore = "0.4"` dependency
- `src/calls.rs` — new: shared call extraction helpers extracted from zoom.rs, plus full-callee variants
- `src/callgraph.rs` — new: CallGraph engine with FileCallData, build_file, resolve_cross_file_edge, forward_tree, walk_project_files, 15 unit tests
- `src/lib.rs` — added `pub mod calls; pub mod callgraph;`
- `src/context.rs` — Config wrapped in RefCell, added config_mut(), callgraph field + accessor
- `src/commands/zoom.rs` — refactored to delegate call extraction to calls.rs
- `src/commands/configure.rs` — new: configure command handler
- `src/commands/call_tree.rs` — new: call_tree command handler
- `src/commands/mod.rs` — added call_tree and configure module declarations
- `src/main.rs` — wired configure and call_tree in dispatch
- `src/commands/add_decorator.rs` — updated ctx.config() borrow pattern
- `src/commands/add_derive.rs` — updated ctx.config() borrow pattern
- `src/commands/add_import.rs` — updated ctx.config() borrow pattern
- `src/commands/add_member.rs` — updated ctx.config() borrow pattern
- `src/commands/add_struct_tags.rs` — updated ctx.config() borrow pattern
- `src/commands/batch.rs` — updated ctx.config() borrow pattern
- `src/commands/edit_match.rs` — updated ctx.config() borrow pattern
- `src/commands/edit_symbol.rs` — updated ctx.config() borrow pattern
- `src/commands/organize_imports.rs` — updated ctx.config() borrow pattern
- `src/commands/remove_import.rs` — updated ctx.config() borrow pattern
- `src/commands/transaction.rs` — updated ctx.config() borrow pattern
- `src/commands/wrap_try_catch.rs` — updated ctx.config() borrow pattern
- `src/commands/write.rs` — updated ctx.config() borrow pattern
- `tests/fixtures/callgraph/main.ts` — fixture: imports processData, calls it
- `tests/fixtures/callgraph/utils.ts` — fixture: imports validate, exports processData
- `tests/fixtures/callgraph/helpers.ts` — fixture: exports validate, local checkFormat
- `tests/fixtures/callgraph/index.ts` — fixture: barrel re-export
- `tests/fixtures/callgraph/aliased.ts` — fixture: aliased import
- `tests/integration/callgraph_test.rs` — 7 integration tests
- `tests/integration/main.rs` — registered callgraph_test module
- `opencode-plugin-aft/src/tools/navigation.ts` — aft_configure and aft_call_tree tools
- `opencode-plugin-aft/src/index.ts` — wired navigation tools

## Forward Intelligence

### What the next slice should know
- `CallGraph` is stored as `RefCell<Option<CallGraph>>` in AppContext — None until `configure` is called. S02's `callers` command needs the same configure-then-use guard.
- Cross-file resolution works through `resolve_cross_file_edge()` which returns `EdgeResolution::Resolved` or `EdgeResolution::Unresolved`. S02's reverse index should track both resolved and unresolved edges.
- `forward_tree()` builds lazily — files are only parsed when reached during traversal. The reverse index for `callers` will likely need a different strategy (scan all files upfront or build reverse index incrementally during forward traversals).
- The `ignore` crate walker (`walk_project_files`) returns only source files (TS/JS/TSX/PY/RS/GO). S02's file watcher should use the same extension filter.

### What's fragile
- Aliased import resolution via raw text parsing (`find_alias_original`) — brittle against edge cases but covers normal imports well. If alias handling gets more complex, consider extending `ImportStatement` with alias fields.
- Barrel file re-export resolution is shallow (one level only) — could produce false unresolved edges for deeply re-exported symbols.

### Authoritative diagnostics
- `EdgeResolution::Unresolved` in `callgraph.rs` — every unresolved cross-file edge is explicitly tagged, never silently dropped. If call trees show unexpected `resolved: false` leaves, start here.
- `cargo test -- callgraph` — 22 tests cover the full resolution pipeline. Run this first after any callgraph.rs changes.

### What assumptions changed
- Plan assumed `RefCell<CallGraph>` — implementation uses `RefCell<Option<CallGraph>>` because the graph can't be initialized without project_root from configure. Minor but affects accessor patterns.
