# S03: Trace to Entry Points — Research

**Date:** 2026-03-14

## Summary

S03 adds `trace_to` — backward traversal from a target symbol to all entry points, rendered top-down. The core algorithm is a reverse BFS/DFS over the existing reverse index from S02, stopping at symbols classified as entry points by a new `is_entry_point()` heuristic. All paths are then reversed for top-down rendering.

The infrastructure is well-prepared. S02's `callers_of()` already does recursive backward traversal with cycle detection, and `build_reverse_index()` scans all project files. `trace_to` needs a different traversal shape though — it doesn't just collect all callers recursively, it needs to collect *complete paths* from entry points to the target. This means building a path-tracking backward traversal rather than reusing `collect_callers_recursive` directly.

Entry point detection is the design crux. D081 says start generic: exported functions, `main`/`init`, test functions. The current `FileCallData` stores `exported_symbols` (names only) and `calls_by_symbol` (symbol → call sites). It does *not* store `SymbolKind` — needed to distinguish functions from methods, classes, interfaces, etc. `SymbolInfo` in callgraph.rs must be extended with a `kind` field, and `FileCallData` must store per-symbol metadata so `is_entry_point` can check both the name pattern and kind without re-parsing.

## Recommendation

**Approach: path-tracking backward BFS with entry point heuristics, top-down rendering.**

### Entry Point Detection

Add `is_entry_point(name, kind, exported, lang, signature)` as a pure function in callgraph.rs:

1. **Exported functions** — `exported == true && kind == Function`. Not methods (those are class members, not standalone entry points). This is the broadest heuristic and covers TS/JS module exports, Rust `pub fn`, Go exported functions.
2. **Main/init patterns** — name is `main`, `init`, `setup`, `bootstrap`, `run` (case-insensitive for the exact match). These are entry points regardless of export status.
3. **Test patterns** — language-specific:
   - TS/JS: name is `describe`, `it`, `test`, or matches `test*`/`spec*` (only top-level functions)
   - Python: name starts with `test_` or is `setUp`/`tearDown`
   - Rust: signature contains `#[test]` or `#[tokio::test]` (parsed from the first line) — but this is fragile. More reliably, name starts with `test_` and kind is Function.
   - Go: name starts with `Test` and kind is Function
4. **No-caller functions** — a function with no callers in the reverse index is de facto an entry point. This catches framework handlers and other patterns without needing framework-specific heuristics. However, this should be a fallback, not primary — it could include dead code.

Recommend implementing categories 1-3 for the initial delivery. Category 4 (no-caller fallback) is interesting but risks false positives.

### Backward Traversal Algorithm

```
trace_to(file, symbol, max_depth=10):
  1. Ensure reverse index is built
  2. BFS backward from (file, symbol):
     - For each (file, symbol), look up callers in reverse_index
     - Track the path: [(file, symbol, line), ...]
     - If a caller is an entry point, record the complete path
     - If a caller is not an entry point, continue backward
     - Cycle detection: visited set of (file, symbol) per path
     - Depth limit: stop expanding paths beyond max_depth hops
  3. Collect all complete paths
  4. Reverse each path so it reads top-down: entry_point → ... → target
  5. Deduplicate paths that share common prefixes (optional — could be S04)
```

Key difference from `callers_of`: `trace_to` collects *paths*, not just *nodes*. Each path is a sequence of hops from entry point to target.

### Data Model

```rust
pub struct TraceHop {
    pub symbol: String,
    pub file: String,       // relative to project root
    pub line: u32,
    pub signature: Option<String>,
    pub is_entry_point: bool,
}

pub struct TracePath {
    pub hops: Vec<TraceHop>,   // top-down: entry_point first, target last
}

pub struct TraceToResult {
    pub target_symbol: String,
    pub target_file: String,
    pub paths: Vec<TracePath>,
    pub total_paths: usize,
    pub entry_points_found: usize,
    pub max_depth_reached: bool,   // true if some paths were truncated
}
```

### SymbolInfo Extension

Add `kind: SymbolKind` to the existing `SymbolInfo` struct in callgraph.rs. The mapping is trivial — `list_symbols_from_tree` already gets `s.kind` from the full `Symbol` struct.

Store a per-file map of `symbol_name → (kind, exported)` in `FileCallData` so entry point detection doesn't require re-parsing. Adding a new field like `symbol_metadata: HashMap<String, SymbolMeta>` where `SymbolMeta { kind, exported, signature }` is cleanest. Alternatively, extend `exported_symbols` to be a richer structure — but that's a bigger refactor. A separate metadata map is simpler.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Reverse index lookup | `CallGraph::build_reverse_index()` from S02 | Already scans all project files and builds `(file, symbol) → Vec<CallerSite>` |
| Cycle detection in traversal | Visited `HashSet<(PathBuf, String)>` pattern from `callers_of` and `forward_tree` | Proven pattern used in both forward and reverse traversal already |
| Symbol metadata (kind, exported) | `list_symbols_from_tree()` returning from `TreeSitterProvider::list_symbols()` | Already extracts kind, name, exported, signature for all 6 languages |
| Path canonicalization | `CallGraph::canonicalize()` + `relative_path()` | Required for consistent reverse index keys (S02 forward intelligence) |

## Existing Code and Patterns

- `src/callgraph.rs:callers_of()` — Entry point for reverse queries. Lazily builds the reverse index. `trace_to()` should follow the same pattern: check `reverse_index.is_none()` → build if needed → traverse.
- `src/callgraph.rs:collect_callers_recursive()` — Recursive backward traversal with visited set and depth limit. `trace_to` needs a similar shape but collecting *paths* instead of flat `Vec<CallerSite>`. Cannot reuse directly.
- `src/callgraph.rs:build_file_data()` — Builds `FileCallData` per file. Currently stores `calls_by_symbol`, `exported_symbols`, `import_block`, `lang`. Needs extension to store symbol metadata for entry point detection.
- `src/callgraph.rs:SymbolInfo` — Currently `{name, start_line, start_col, end_line, end_col, exported, signature}`. Missing `kind: SymbolKind`. Extension is mechanical — add the field and map `s.kind` in `list_symbols_from_tree`.
- `src/callgraph.rs:FileCallData.exported_symbols` — `Vec<String>` of exported symbol names. `is_entry_point` needs both name and kind, so either cross-reference with a new metadata field or extend this.
- `src/commands/callers.rs` — Command handler pattern: extract params, check configure guard, call graph method, serialize result. `trace_to.rs` follows the same structure.
- `src/commands/call_tree.rs` — Same pattern. Good reference for the handler skeleton.
- `opencode-plugin-aft/src/tools/navigation.ts` — Plugin tool registration. Add `aft_trace_to` following the same shape as `aft_callers`.
- `tests/integration/callgraph_test.rs` — Integration test patterns. `trace_to` tests go here.
- `tests/fixtures/callgraph/` — Multi-file TS fixtures. Current chain is `main → processData → validate → checkFormat` (3 layers). Good for basic `trace_to` testing but may need extension for multi-path scenarios.

## Constraints

- **Reverse index is `HashMap<(PathBuf, String), Vec<CallerSite>>`** — keyed by canonical paths. `trace_to` must canonicalize all lookups, same as `callers_of`. The S02 forward intelligence warns: "if any new code path adds caller entries without canonicalizing, recursive lookups will silently miss results."
- **Reverse index cleared on any file invalidation (D092)** — `trace_to` triggers a full project scan on first call after invalidation. Acceptable per D092 rationale, but means the first `trace_to` after a file change is the most expensive.
- **`SymbolInfo.kind` doesn't exist yet** — Must be added. The `#[allow(dead_code)]` annotation on `SymbolInfo` suggests fields were deliberately kept minimal. Adding `kind` is justified because entry point detection requires it.
- **Single-threaded RefCell architecture** — `trace_to` borrows `callgraph` mutably (for lazy index build). Same constraints as all other graph commands.
- **Method calls are not entry points** — A method on a class is accessed via `obj.method()`, which is tracked as a call site in the graph. Methods shouldn't be classified as entry points even if exported. The `kind == Function` check handles this.
- **No framework-specific heuristics in this slice (D081)** — Express `router.get`, Flask `@app.route`, etc. are deferred. Generic patterns only: exports, main/init, test functions.
- **`FileCallData` stores `exported_symbols` as `Vec<String>`** — Entry point detection needs more than just names. Adding symbol metadata is the right extension point rather than making `exported_symbols` more complex.

## Common Pitfalls

- **Path inconsistency in reverse index lookups** — S02 forward intelligence explicitly warns about this. All path lookups must go through `canonicalize()`. The `CallerSite.caller_file` is already canonical, but when following a path backward (looking up the *caller* as the new target), the path is already canonical — no double-canonicalization needed.
- **Exported methods treated as entry points** — A method like `class Foo { export method() {} }` (in TS/JS, exported via class export) should not be an entry point. The heuristic must check `kind == Function`, not just `exported == true`.
- **Test fixture coverage gaps** — Current fixtures have a single chain (main → processData → validate → checkFormat). `trace_to` needs multi-path fixtures: a utility function called from multiple entry points (two different exported functions, a test function, etc.). Need new fixture files.
- **Empty paths when no entry points are found** — If backward traversal reaches a dead end (no callers) without finding an entry point, the path is incomplete. The response should distinguish between "found paths to entry points" and "no entry points reached." Include these as `truncated_paths` in the response.
- **Depth limit masking real entry points** — If max_depth is too low, `trace_to` might not reach the actual entry points. Default of 10 is generous. The response includes `max_depth_reached` flag so agents know when results might be incomplete.
- **`build_reverse_index` scans all files** — On first `trace_to` call, this scans every file in the project. For typical projects this is fast (< 1s). For large monorepos, could be 2-5s. The lazy construction model handles this — subsequent calls are cached.

## Open Risks

- **Entry point heuristic accuracy** — Generic patterns (exports, main, test) will miss framework handlers. An Express route like `router.get('/users', getUsers)` won't be detected as an entry point — `getUsers` will just show as "no callers" (dead end). The response should handle this gracefully rather than failing. The `max_depth_reached` flag and incomplete path handling address this.
- **Exported-but-internal functions** — In many TS projects, functions are exported for testing but aren't true entry points. This creates noise in `trace_to` results. Acceptable for v1 — the agent can filter.
- **Performance on first cold query** — `trace_to` requires the full reverse index (all files scanned). For a 500-file project, this is ~1s. For 5000+ files, could approach the 2s target. Depth limit helps — the traversal stops at `max_depth` regardless of project size.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | (checked `<available_skills>`) | No directly relevant skill — existing patterns are well-established |
| tree-sitter | (checked `<available_skills>`) | No relevant skill — codebase has mature tree-sitter patterns |

No external skills recommended. The implementation is a graph algorithm on top of existing infrastructure.

## Sources

- S02 forward intelligence in `S02-SUMMARY.md` — reverse index data structure, path canonicalization requirement, drain-at-dispatch pattern
- `src/callgraph.rs` — existing graph types, `build_reverse_index()`, `callers_of()`, `forward_tree()` patterns
- D081 — entry point detection starts generic (exports, main/init, test functions)
- D092 — reverse index cleared entirely on invalidation
- M003-ROADMAP boundary map — S03 produces `is_entry_point()`, `handle_trace_to()`, backward traversal algorithm; consumes forward graph + reverse index from S01/S02
