---
estimated_steps: 7
estimated_files: 5
---

# T01: Build call graph engine with cross-file resolution and forward traversal

**Slice:** S01 — Call Graph Infrastructure + Forward Call Tree
**Milestone:** M003

## Description

Build the core `CallGraph` engine as a new `src/callgraph.rs` module. This includes: extracting shared call-site helpers from zoom.rs into `src/calls.rs`, building the per-file call data model, cross-file edge resolution using the existing import engine, worktree-scoped file walking via the `ignore` crate, and depth-limited forward traversal with cycle detection. All unit-testable without protocol wiring.

## Steps

1. Add `ignore = "0.4"` to `Cargo.toml` dependencies
2. Extract `call_node_kinds()`, `walk_for_calls()`, `extract_callee_name()`, and `extract_last_segment()` from `src/commands/zoom.rs` into a new `src/calls.rs` module. Update zoom.rs to import from `calls.rs`. Add `pub mod calls;` to `src/lib.rs`. Run existing zoom tests to confirm no regression.
3. Create `src/callgraph.rs` with core types: `CallSite` (callee name, line, byte range), `FileCallData` (call sites per symbol, exported symbol names, import info), `CallGraph` struct wrapping `HashMap<PathBuf, FileCallData>` with lazy `build_file()` that uses `calls.rs` helpers for extraction and `parser.rs` for AST access
4. Implement `resolve_cross_file_edge()` — given a callee name and the calling file's import block (from `parse_file_imports`), resolve which file and symbol the call targets. Handle: direct named imports (`import { foo } from './utils'`), aliased imports (`import { foo as bar } from './utils'`), namespace imports (`import * as utils from './utils'` → `utils.foo`), re-exports through barrel files. Return an `EdgeResolution` enum: `Resolved { file, symbol }` or `Unresolved { callee_name }`.
5. Implement worktree-scoped file discovery using `ignore::WalkBuilder` — given a project root, walk files respecting .gitignore, always excluding `node_modules/`, `target/`, `venv/`, `.git/`. Expose as `walk_project_files()` returning an iterator of `PathBuf`.
6. Implement `forward_tree()` — depth-limited recursive traversal starting from a (file, symbol) pair. Uses `HashSet<(PathBuf, String)>` for cycle detection. At each node: look up call sites for the symbol, resolve each callee cross-file, recurse up to depth limit. Returns a tree of `CallTreeNode { name, file, line, signature, resolved, children }`.
7. Write unit tests in `src/callgraph.rs` `#[cfg(test)]` module: single-file call extraction builds correct `FileCallData`, cross-file resolution follows direct/aliased imports, cycle detection stops on A→B→A, depth limit truncates at specified level, worktree walker excludes .gitignore'd paths.

## Must-Haves

- [ ] `calls.rs` extracted from zoom.rs with no test regressions
- [ ] `CallGraph` struct with lazy per-file construction
- [ ] Cross-file edge resolution using `parse_file_imports()` handles direct, aliased, and re-exported imports
- [ ] `ignore` crate integrated for worktree-scoped file walking
- [ ] `forward_tree()` has cycle detection (no infinite loops) and depth limiting
- [ ] Unresolved edges explicitly marked (not silently dropped)
- [ ] Unit tests pass for all core paths

## Verification

- `cargo test -- calls` — extracted module tests (existing zoom call tests should still pass)
- `cargo test -- callgraph` — new unit tests for graph construction, resolution, traversal
- `cargo test` — full suite, all 294+ existing tests pass

## Inputs

- `src/commands/zoom.rs` — call extraction functions to extract (lines 249-348)
- `src/imports.rs` — `parse_file_imports()` for cross-file resolution
- `src/parser.rs` — `LangId`, `detect_language()`, `grammar_for()`, `FileParser`

## Expected Output

- `src/calls.rs` — shared call extraction helpers (extracted from zoom.rs)
- `src/callgraph.rs` — `CallGraph` engine with `FileCallData`, `build_file()`, `resolve_cross_file_edge()`, `forward_tree()`, `walk_project_files()`, and unit tests
- `src/lib.rs` — updated with `pub mod calls; pub mod callgraph;`
- `src/commands/zoom.rs` — refactored to use `calls.rs` (no behavior change)

## Observability Impact

- **Unresolved edges:** Every call that can't be followed cross-file is returned as `EdgeResolution::Unresolved { callee_name }`, which surfaces as `resolved: false` in `CallTreeNode`. A future agent inspecting a call tree sees exactly which edges are approximate.
- **FileCallData cache:** `CallGraph.data` stores per-file results. Cache hits avoid re-parsing. A future agent can check `data.len()` to see how many files have been analyzed.
- **Cycle detection:** Cycles are silently terminated (empty children) rather than erroring. The returned tree is always finite — a future agent can detect cycles by finding leaf nodes with `resolved: true` and empty children at less than max depth.
- **Depth truncation:** Nodes at the depth limit have empty children. A future agent can distinguish depth-limited truncation (resolved + no children at max depth) from unresolved edges (resolved: false).
