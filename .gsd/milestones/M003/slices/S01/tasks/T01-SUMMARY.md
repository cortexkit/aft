---
id: T01
parent: S01
milestone: M003
provides:
  - CallGraph engine with lazy per-file construction
  - Cross-file edge resolution via import chain analysis
  - Depth-limited forward call tree traversal with cycle detection
  - Worktree-scoped file discovery via ignore crate
  - Shared call extraction helpers (calls.rs extracted from zoom.rs)
key_files:
  - src/callgraph.rs
  - src/calls.rs
key_decisions:
  - Used text-based alias parsing from raw import text rather than extending ImportStatement struct — avoids modifying the import engine's data model for a callgraph-specific need
  - Cycle detection uses visited set with remove-after-recurse pattern (allows same node in different call chains, only prevents cycles)
  - FileCallData stores full callee expressions alongside short names to support namespace import resolution (utils.foo → add)
patterns_established:
  - extract_calls_full returns (full_callee, short_name, line) triples for namespace-aware resolution
  - EdgeResolution enum explicitly marks unresolved edges — never silently dropped
  - walk_project_files uses ignore crate with hardcoded exclusions for node_modules/target/venv/.git
observability_surfaces:
  - EdgeResolution::Unresolved surfaces as resolved:false in CallTreeNode
  - CallGraph.data cache tracks which files have been analyzed
  - Cycle-terminated nodes: resolved:true with empty children below max depth
duration: 45min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Build call graph engine with cross-file resolution and forward traversal

**Built `CallGraph` engine with lazy per-file call extraction, cross-file import resolution (direct, aliased, namespace), depth-limited forward traversal with cycle detection, and worktree-scoped file discovery.**

## What Happened

Extracted shared call-site helpers (`call_node_kinds`, `walk_for_calls`, `extract_callee_name`, `extract_last_segment`) from `zoom.rs` into `src/calls.rs`. Added `extract_calls_full` and `extract_full_callee` for namespace-qualified call detection (e.g. `utils.foo`). Zoom.rs now delegates to `calls.rs` — all existing zoom tests pass unchanged.

Built `src/callgraph.rs` with:
- **FileCallData**: per-file call sites grouped by symbol, exported symbol names, and parsed imports
- **CallGraph**: wraps `HashMap<PathBuf, FileCallData>` with lazy `build_file()` that uses TreeSitterProvider for symbol listing and calls.rs for extraction
- **resolve_cross_file_edge()**: resolves callees through direct named imports, aliased imports (parses `{foo as bar}` from raw import text), namespace imports (`import * as utils → utils.foo`), and barrel file re-exports
- **forward_tree()**: depth-limited recursive traversal with `HashSet`-based cycle detection; unresolved edges appear as leaf nodes with `resolved: false`
- **walk_project_files()**: `ignore::WalkBuilder` respecting .gitignore with hardcoded exclusions for node_modules, target, venv, .git

Added `ignore = "0.4"` to Cargo.toml.

## Verification

- `cargo test -- callgraph` — 15 tests pass: single-file extraction, exports detection, direct/namespace/aliased import resolution, unresolved edge marking, cycle detection (A→B→A bounded), depth-0 and depth-1 truncation, cross-file forward tree (main→helper→double across 3 files), gitignore exclusion, source-file-only filtering, alias text parsing
- `cargo test -- zoom` — 12 tests pass (no regression from extraction refactor)
- `cargo test` — 309 total tests pass (190 lib + 119 integration)

**Slice-level verification status:**
- ✅ `cargo test -- callgraph` — unit tests pass
- ⬜ `cargo test -- call_tree` — T02 (integration tests)
- ⬜ `bun test` in opencode-plugin-aft — T02 (plugin tools)
- ✅ All existing tests pass (309 total)

## Diagnostics

- Inspect `CallGraph.data.len()` for cache population
- `EdgeResolution::Unresolved` edges in forward_tree output have `resolved: false` with callee name but no file path
- Cycle-terminated nodes have `resolved: true`, empty `children`, at depth < max_depth
- Depth-truncated nodes have empty `children` at exactly max_depth

## Deviations

- ImportStatement doesn't store alias mappings, so aliased import resolution parses the raw import text with `find_alias_original()`. This avoids modifying the shared import engine's data model.
- Added `extract_calls_full` and `extract_full_callee` to `calls.rs` (not in original plan) — needed for namespace import resolution where the full callee expression matters.

## Known Issues

- Barrel file re-export resolution is shallow — checks if the index file directly exports the symbol, doesn't follow re-export chains through multiple levels.
- Aliased import detection uses text parsing (`" as "` pattern) which could false-match in edge cases with comments or string literals in import statements (extremely unlikely in practice).

## Files Created/Modified

- `Cargo.toml` — added `ignore = "0.4"` dependency
- `src/calls.rs` — new: shared call extraction helpers extracted from zoom.rs, plus full-callee variants
- `src/callgraph.rs` — new: CallGraph engine with FileCallData, build_file, resolve_cross_file_edge, forward_tree, walk_project_files, and 15 unit tests
- `src/lib.rs` — added `pub mod calls; pub mod callgraph;`
- `src/commands/zoom.rs` — refactored to delegate call extraction to `calls.rs`
