# M003: Call Graph Navigation — Research

**Date:** 2026-03-14

## Summary

M003 builds a lazy, incremental, in-memory call graph on top of M001's tree-sitter parsing and symbol extraction. The core data structure is a directed graph (HashMap of symbol → edges) populated on demand when a command first queries a file, cached in the persistent process, and invalidated by a file watcher. Five new commands expose this graph: `call_tree`, `callers`, `trace_to`, `trace_data`, `impact`.

The codebase is well-prepared for this. The zoom command (`src/commands/zoom.rs`) already has the complete call-extraction pipeline: `extract_calls_in_range`, `walk_for_calls`, `extract_callee_name` with member-expression last-segment extraction, and `call_node_kinds` per language. These functions need to be extracted from zoom.rs into a shared `src/callgraph.rs` module and extended for cross-file resolution. The `FileParser` cache, `AppContext` dispatch pattern, and NDJSON protocol are all stable foundations.

The main architectural challenge is threading: the binary is currently single-threaded with `RefCell` interior mutability (D001, D014, D029). The file watcher (`notify` crate) runs on its own OS thread and sends events via a channel. The simplest approach that preserves the existing RefCell architecture is to drain the watcher channel at the start of each command dispatch — no concurrent access, no Mutex migration. This is the recommended path. Cross-file symbol resolution via import/export following is the hardest accuracy problem, but the import engine (`src/imports.rs`) already parses import statements for all 6 languages, providing module paths and names that can be resolved to filesystem paths.

## Recommendation

**Approach: Lazy graph with synchronous drain, risk-first slice ordering.**

Build the call graph as a `HashMap<PathBuf, FileCallData>` inside `AppContext` (behind `RefCell`, like existing stores). Each `FileCallData` contains: parsed call sites (callee name, location, byte range) and exported symbols. Graph edges are resolved lazily when a query traverses an edge to an unparsed file.

The file watcher runs on a background thread and pushes `PathBuf` events into a `crossbeam-channel`. At the top of each `dispatch()` call, drain all pending events and mark those files as stale in the graph. This preserves the single-threaded command execution model.

Cross-file resolution uses a layered strategy:
1. **Import following** — parse imports in the calling file, match callee name to imported name, resolve module path to filesystem path (relative imports are exact, package imports are best-effort).
2. **Export matching** — parse the target file's symbols, match by name and export status.
3. **Fallback** — when import resolution fails, mark the edge as "unresolved" with a callee name only.

Start with the graph infrastructure and forward call tree (S01-S02) since those prove the core data structure without needing the harder reverse-trace or entry-point heuristics. Trace-to and entry points (S03) is the highest-risk slice and should come next. Data flow (S04) and impact (S05) build on the proven graph.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|-------------------|------------|
| File system watching (cross-platform) | `notify` crate v8.x (stable) or v9.0.0-rc.2 | Handles inotify/kqueue/FSEvents/ReadDirectoryChanges. Rolling our own is insane. Mature, well-maintained. |
| .gitignore-aware file walking | `ignore` crate v0.4.25 | Used by ripgrep. Handles .gitignore, .git/info/exclude, global gitignore. Already does exactly what R027 needs. |
| Channel for watcher → main thread | `crossbeam-channel` (notify dependency) or `std::sync::mpsc` | Don't hand-roll thread communication. `crossbeam-channel` is already in notify's dep tree. |
| Debouncing file events | `notify-debouncer-mini` or manual drain | notify fires duplicate events (create+modify, rename pair). Need debouncing or batch drain. Manual drain at dispatch boundary is simpler than the debouncer crate for our sync-drain model. |
| Call extraction from AST | Existing `walk_for_calls` + `extract_callee_name` in zoom.rs | Already handles all 6 languages, member expressions, Rust macros. Extract and reuse, don't rewrite. |
| Import parsing | Existing `parse_ts_imports`, `parse_py_imports`, etc. in imports.rs | Already parses module paths and named imports for all 6 languages. Reuse for cross-file resolution. |

## Existing Code and Patterns

- `src/commands/zoom.rs` — Contains `extract_calls_in_range`, `walk_for_calls`, `extract_callee_name`, `call_node_kinds`, `line_col_to_byte`. These are the call extraction primitives. Currently file-scoped (D022). Must be extracted to a shared module and extended for cross-file edges.
- `src/parser.rs` — `FileParser` with mtime-based cache, `extract_symbols` per language, `grammar_for`, `detect_language`, `node_text`, `node_range`. The parse cache is central to M003 performance — the call graph reuses cached parse trees.
- `src/imports.rs` — Full import statement parsing for all 6 languages. `parse_file_imports()` returns `ImportBlock` with `ImportStatement.module_path` and `names`. This is the foundation for cross-file symbol resolution (resolving `import { foo } from './bar'` to `bar.ts::foo`).
- `src/context.rs` — `AppContext` with `RefCell<BackupStore>`, `RefCell<CheckpointStore>`, `Box<dyn LanguageProvider>`. New call graph store will follow the same RefCell pattern. Config already has `project_root: Option<PathBuf>` which the graph needs for worktree scoping.
- `src/symbols.rs` — `Symbol`, `SymbolKind`, `SymbolMatch`. The graph nodes reference these. `exported` field is critical for cross-file resolution.
- `src/language.rs` — `LanguageProvider` trait with `resolve_symbol` and `list_symbols`. Call graph may need a new trait method or operate independently via `FileParser` directly (like zoom does, per D023).
- `src/main.rs` — Single-threaded `dispatch()` loop reading lines from stdin. The watcher drain hook goes here, before dispatch. 20 existing commands; M003 adds 5 more.
- `src/config.rs` — `Config::project_root` already exists but defaults to `None`. M003 needs this set by the plugin (via a new `init` command or startup param) to scope the file watcher and `.gitignore` resolution.
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge manages process lifecycle. No changes needed for M003 commands (same JSON protocol).
- `opencode-plugin-aft/src/tools/reading.ts` — Pattern for tool registration. M003 tools follow the same pattern in a new `navigation.ts` file.
- `tests/fixtures/calls.ts` — Existing intra-file call fixture. M003 needs multi-file fixtures (e.g., `tests/fixtures/callgraph/` with cross-file imports).
- `tests/integration/helpers.rs` — `AftProcess` test harness. M003 tests use the same pattern.

## Constraints

- **Single-threaded RefCell architecture (D001, D014, D029)** — Binary is single-threaded by design. File watcher must not cause concurrent access to AppContext stores. Drain pattern (check channel at dispatch entry) preserves this invariant.
- **Tree-sitter accuracy ~80% for direct calls (CONTEXT)** — Call graph is approximate. Dynamic dispatch (`obj[method]()`, `getattr(obj, name)()`), higher-order functions (`array.map(fn)`), and computed property access cannot be statically resolved. Must mark these edges as approximate.
- **No runtime dependencies (CONTEXT)** — All analysis is static. No running language servers, no type information, no runtime tracing.
- **Existing dependencies are minimal** — Current Cargo.toml has only serde, serde_json, tree-sitter grammars, similar, streaming-iterator. Adding `notify`, `ignore`, and `crossbeam-channel` is a meaningful expansion. Keep additions justified.
- **Import resolution is path-based, not type-based** — `import { foo } from './bar'` resolves to `./bar.ts` or `./bar/index.ts`. Package imports (`import { foo } from 'react'`) resolve to unresolved external nodes (no node_modules crawling).
- **Config.project_root defaults to None** — Must be set for file watcher and .gitignore scoping. The plugin knows the project root (`input.directory` in index.ts). Need a protocol mechanism to pass this at startup.
- **Binary size matters (release profile optimized to 6.4MB)** — `notify` and `ignore` will add to binary size. Both are well-optimized crates used in production tools (ripgrep, watchexec).
- **NDJSON protocol (D009)** — All new commands follow the existing request/response envelope pattern. No streaming results — each command returns a complete response.

## Common Pitfalls

- **Circular call graphs** — Functions can call each other recursively (`A → B → A`). Without cycle detection, graph traversal will infinite-loop. Use a visited set on every traversal. Apply depth limits as hard safety bounds.
- **Import resolution ambiguity** — `import { foo } from './utils'` could resolve to `utils.ts`, `utils/index.ts`, `utils.js`, etc. Need a prioritized extension resolution list per language, and must handle missing files gracefully (mark as unresolved, don't error).
- **Barrel files / re-exports** — `export { foo } from './internal'` means `foo` isn't defined in the barrel file. Must follow re-export chains. Can limit to 1-2 hops to avoid performance issues.
- **File watcher event storms** — `git checkout`, `npm install`, or IDE auto-save can trigger hundreds of events simultaneously. Drain-and-deduplicate at dispatch boundary handles this naturally — each `PathBuf` is invalidated once regardless of how many events fired.
- **Entry point detection false positives** — Not every exported function is an entry point. Framework-specific heuristics (Express `router.get`, Flask `@app.route`) are fragile across versions. Start with generic patterns (exports, main, test functions), add framework patterns incrementally.
- **Graph memory growth** — Each file node stores call sites and symbol data. For a 10K-file project, this could be 50-100MB. LRU eviction or lazy-only-what's-been-queried keeps the footprint manageable. The lazy approach naturally limits this — only files reachable from queries are parsed.
- **Method call resolution** — `this.foo()` and `self.foo()` in TS/Python resolve to a method on the current class. `obj.foo()` requires knowing `obj`'s type, which tree-sitter can't provide. Last-segment heuristic (already used in zoom) is the practical fallback, but creates false positives when multiple classes have methods with the same name.
- **Rename tracking in trace_data** — Following a value through `const x = foo(); bar(x);` requires tracking that `x` is the return value of `foo` and the first argument to `bar`. This is light data-flow analysis, not just call graph traversal. Keep it simple: track through assignments and function parameters, don't try to handle destructuring or spread patterns initially.

## Open Risks

- **Cross-file resolution accuracy for dynamic languages** — TypeScript with barrel re-exports and Python with `__init__.py` re-exports are the hardest cases. May need iterative improvement after initial delivery. Accept ~70% accuracy for cross-file resolution initially, improving with framework-specific patterns.
- **File watcher reliability across platforms** — `notify` is mature but macOS FSEvents and Linux inotify have different semantics (FSEvents is path-based, inotify is inode-based). Test on both platforms during development.
- **Performance of first query on large codebases** — A `trace_to` query that needs to scan backward through 5 layers of callers could touch 50+ files on first cold query. Depth limits (default 5, max 10) and lazy construction mitigate this, but the first query will still be slower than subsequent ones. Target: <2s for typical projects, <5s for large monorepos.
- **Project root detection** — If the plugin doesn't pass `project_root`, the binary has no way to scope file watching or .gitignore resolution. Need a robust fallback (walk up from first-queried file looking for `.git`).
- **`notify` v8 vs v9** — v8.2.0 is stable, v9.0.0-rc.2 is a release candidate with API changes. Recommend v8.x for stability; v9 can wait until it's released.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | apollographql/skills@rust-best-practices (2.4K installs) | available — general Rust patterns, not M003-specific |
| tree-sitter | plurigrid/asi@tree-sitter (7 installs) | available — very low adoption, not useful |

No skills are directly relevant enough to recommend installing. The codebase already has well-established patterns for tree-sitter parsing and Rust architecture.

## Requirement Analysis

### Table Stakes (requirements that are essential)
- **R020 (call graph construction)** — The entire milestone depends on this. Lazy/incremental is the only viable strategy for large codebases. File watcher is essential for keeping the graph current in the persistent process.
- **R021 (forward call tree)** — The simplest and most immediately useful command. Proves the graph data structure.
- **R022 (reverse caller tree)** — Complement to R021. Together they cover "what does X call" and "what calls X."
- **R027 (worktree-aware scoping)** — Without this, the graph crawls into node_modules and explodes. Must be part of the first slice.

### Expected behaviors that should be validated
- **R023 (trace_to) and R026 (entry point detection)** — These are coupled. trace_to without entry point detection doesn't know where to stop. The heuristics for entry point detection are framework-specific and will need iterative improvement. Core generic patterns (exports, main, test) should ship first.
- **R025 (impact analysis)** — High value, but depends on accurate callers (R022). The "suggestions" aspect (e.g., "add default argument") is a nice-to-have; the core is just "these call sites break."

### Differentiators
- **R024 (data flow tracking)** — The most complex command. Static data flow analysis across function boundaries is genuinely hard. Recommend a minimal viable version: track through direct assignments and function parameters, don't handle destructuring/spread/conditional assignments. Mark approximations.

### Candidate new requirements (from research, not auto-binding)
- **Graph cycle detection** — Not explicitly in requirements but essential for correctness. Every traversal must detect and break cycles. Recommend documenting as an implementation constraint, not a new requirement.
- **Project root initialization** — Config.project_root needs to be set. Recommend an `init` command or extending the protocol to accept `project_root` as a startup parameter (could be a `configure` command). This is prerequisite for R027.
- **Unresolved edge handling** — When cross-file resolution fails, edges should be marked as "unresolved" with the callee name. The response should distinguish between resolved and unresolved edges so agents know when the graph is approximate.
- **Depth limits on all traversals** — Not explicitly stated as a requirement but mentioned in CONTEXT. Should be a documented constraint with sensible defaults (forward: 5, reverse: 5, trace_to: 10).
- **Graph statistics command** — Agents benefit from knowing graph coverage: "X files indexed, Y edges resolved, Z unresolved." Could be part of a `call_graph_status` command. Low priority — consider deferring.

## Sources

- `notify` crate v8.2.0 stable, v9.0.0-rc.2 RC (source: `cargo search notify`)
- `ignore` crate v0.4.25 — gitignore-aware file walking used by ripgrep (source: `cargo search ignore`, Context7 docs)
- `ignore::WalkBuilder` supports .gitignore, .git/info/exclude, global gitignore, configurable depth limits (source: Context7 /websites/rs_ignore docs)
- Existing call extraction pipeline in zoom.rs handles all 6 languages (source: codebase `src/commands/zoom.rs`)
- Import parsing for all 6 languages with module path extraction (source: codebase `src/imports.rs`)
- Binary is single-threaded with RefCell — D001, D014, D029 decisions (source: `.gsd/DECISIONS.md`)
