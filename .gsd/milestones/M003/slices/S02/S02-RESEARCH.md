# S02: Reverse Callers + File Watcher — Research

**Date:** 2026-03-14

## Summary

S02 adds two capabilities to the call graph: a reverse caller index (`callers` command) and a file watcher for graph invalidation. Both are well-constrained by S01's foundations — the forward graph data structure, the configure-then-use pattern, and the `RefCell<Option<CallGraph>>` storage in `AppContext`.

The reverse index is a straightforward inversion of the existing `calls_by_symbol` data. For each file's call sites, we record `(target_file, target_symbol) → Vec<CallerSite>`. The main design question is when to build it: eagerly (scan all project files on first `callers` query) vs lazily (only files already visited). Eager is correct here — `callers` semantically requires knowing *all* callers, so partial results would be misleading. The cost is acceptable: `walk_project_files` + `build_file` for a 100-1000 file project takes <2s, and subsequent queries hit the cache.

The file watcher uses `notify` v8 with `std::sync::mpsc` (stdlib — no `crossbeam-channel` needed). `notify`'s `EventHandler` trait is implemented for `mpsc::Sender<Result<Event>>` natively. The watcher runs on a background OS thread; the `mpsc::Receiver` is drained at the top of the dispatch loop before each command. This preserves the single-threaded `RefCell` architecture (D001, D014, D029). Events are deduplicated by `PathBuf` — `notify` fires multiple events per file write (Create, Modify(Metadata), Modify(Data)) and the drain deduplicates them into one invalidation per file. Invalidation means removing the file's `FileCallData` from the graph and clearing the reverse index (which must be rebuilt lazily on next `callers` query).

## Recommendation

**Two-phase approach: reverse index in CallGraph, then watcher integration in main.rs.**

### Phase 1: Reverse index + `callers` command

Add to `CallGraph`:
- `reverse_index: Option<HashMap<(PathBuf, String), Vec<CallerSite>>>` — `None` until first built, rebuilt lazily after invalidation.
- `build_reverse_index()` — scans all project files via `walk_project_files()`, calls `build_file()` for each, then inverts `calls_by_symbol` using `resolve_cross_file_edge()` to map callees to their target files.
- `callers_of(file, symbol, depth)` — returns all callers grouped by file, with optional recursive expansion (who calls the callers).
- `invalidate_file(path)` — removes the file from `data` HashMap, sets `reverse_index = None` (forces rebuild on next reverse query), clears `project_files` cache.

The `CallerSite` struct: `{ caller_file: PathBuf, caller_symbol: String, line: u32, resolved: bool }`.

The `callers` command handler follows the same pattern as `call_tree`: check `Option<CallGraph>`, extract params, call `callers_of()`, serialize result.

### Phase 2: File watcher

The watcher lifecycle is tied to `configure`:
1. When `configure` is called, create `mpsc::channel()`, spawn `notify::recommended_watcher(tx)`, watch `project_root` recursively.
2. Store the `Receiver` and `Watcher` handle in `AppContext` (new fields: `RefCell<Option<mpsc::Receiver<...>>>`, `RefCell<Option<RecommendedWatcher>>`).
3. In `main.rs`, add a `drain_watcher_events(&ctx)` call before `dispatch()`. This function borrows the receiver, calls `try_recv()` in a loop, collects changed `PathBuf`s into a `HashSet`, then calls `graph.invalidate_file()` for each.

The watcher should only watch source files. Since `notify` watches directories recursively (can't filter by extension at the OS level), the drain function filters events to only process paths with supported extensions (`.ts`, `.tsx`, `.js`, `.jsx`, `.py`, `.rs`, `.go`).

### Watcher storage decision

Two options for where the watcher lives:
- **In `AppContext`**: Clean — everything related to state is in AppContext. But `RecommendedWatcher` is a platform-specific type. On macOS it's `FsEventWatcher`.
- **Separate from `AppContext`**: Watcher + receiver held in `main.rs` locals. Passed to `configure` as a callback. Simpler types but splits state.

**Recommendation: Store in AppContext.** The watcher is logically part of the application state. `RefCell<Option<_>>` wrapping is consistent with the existing pattern. Verified that `RecommendedWatcher` works in `RefCell` on macOS (compiled and ran test code).

The receiver *could* be stored alongside the watcher in AppContext, but it's cleaner to store it separately because the drain function needs to borrow the receiver without also borrowing the graph. Using two separate `RefCell`s avoids borrow conflicts:
- `RefCell<Option<mpsc::Receiver<notify::Result<Event>>>>` — for the drain function
- `RefCell<Option<RecommendedWatcher>>` — just to keep the watcher alive (dropped = stops watching)

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Cross-platform file watching | `notify` v8.2.0 (stable) | Handles FSEvents (macOS), inotify (Linux), ReadDirectoryChanges (Windows). Rolling our own is insane. |
| Channel for watcher → main thread | `std::sync::mpsc` (stdlib) | `notify` implements `EventHandler` for `mpsc::Sender` natively. No need for `crossbeam-channel`. Zero added deps. |
| Call extraction from AST | Existing `extract_calls_full` in `src/calls.rs` | Already handles all 6 languages, extracted in S01. Reverse index reuses this. |
| Import resolution | Existing `resolve_cross_file_edge` in `src/callgraph.rs` | Already resolves direct, aliased, namespace, barrel imports. Reverse index uses this to map callees to targets. |
| Worktree-scoped file walking | Existing `walk_project_files` in `src/callgraph.rs` | Already uses `ignore` crate, respects .gitignore, excludes node_modules/target/venv. Reverse index uses this for full scan. |

## Existing Code and Patterns

- `src/callgraph.rs` — `CallGraph` struct with `HashMap<PathBuf, FileCallData>`, `build_file()` for lazy per-file construction, `resolve_cross_file_edge()` for import-based resolution, `forward_tree()` for depth-limited traversal, `walk_project_files()` for scoped file discovery. S02 extends this with reverse index and invalidation. Do not modify the forward traversal logic.
- `src/callgraph.rs::FileCallData` — Stores `calls_by_symbol: HashMap<String, Vec<CallSite>>` and `exported_symbols: Vec<String>`. The reverse index inverts `calls_by_symbol` using cross-file resolution to determine target (file, symbol) pairs.
- `src/callgraph.rs::CallSite` — Has `callee_name`, `full_callee`, `line`, `byte_start`, `byte_end`. The `callee_name` and `full_callee` are inputs to `resolve_cross_file_edge()`.
- `src/callgraph.rs::EdgeResolution` — `Resolved { file, symbol }` or `Unresolved { callee_name }`. The reverse index only includes `Resolved` edges — unresolved callees can't be mapped to a target.
- `src/context.rs` — `AppContext` with `RefCell<Option<CallGraph>>`, `RefCell<Config>`, etc. New watcher fields follow the same `RefCell<Option<_>>` pattern.
- `src/main.rs` — `for line in reader.lines()` loop with `dispatch(req, &ctx)`. The watcher drain goes right before `dispatch()` inside the loop body. The `ctx` is constructed before the loop.
- `src/commands/configure.rs` — `handle_configure()` creates `CallGraph::new(root_path)` and stores it. S02 extends this to also create the watcher and store it.
- `src/commands/call_tree.rs` — Pattern for graph-dependent command: borrow `ctx.callgraph()`, check `Option`, extract params, call graph method. `callers` command follows the identical pattern.
- `tests/integration/callgraph_test.rs` — 7 integration tests for configure + call_tree. S02 adds callers tests and file-modification-then-query tests following the same `AftProcess` pattern.
- `opencode-plugin-aft/src/tools/navigation.ts` — `aft_configure` and `aft_call_tree` tool definitions. S02 adds `aft_callers` following the same Zod schema + bridge.send pattern.

## Constraints

- **Single-threaded RefCell architecture (D001, D014, D029)** — Cannot borrow `callgraph` mutably and the watcher receiver at the same time if they're in the same `RefCell`. Drain function must complete (releasing receiver borrow) before dispatch borrows the graph. Separate `RefCell`s for watcher components solve this.
- **`notify` fires multiple events per file operation** — A single `fs::write()` on macOS produces Create + Modify(Metadata) + Modify(Data) events. The drain must deduplicate by PathBuf to avoid redundant invalidation work.
- **Watcher must be initialized during `configure`** — The watcher needs `project_root` to know what to watch. Before `configure`, there's no watcher. The receiver `RefCell` starts as `None`.
- **Reverse index requires full project scan** — Unlike forward traversal (starts from one file, follows outward), reverse lookup ("who calls this?") inherently needs to know about all callers. First `callers` query triggers a full scan. This is O(N files) but acceptable for typical projects (<2s for 1000 files).
- **Graph invalidation clears reverse index entirely** — A file change could add or remove callers for any symbol. Rather than computing delta updates (complex, error-prone), clearing the `Option<HashMap>` reverse index and rebuilding on next query is simpler and correct.
- **Watcher should not watch node_modules/target/venv** — `notify` watches recursively. We can't prevent it from receiving events from excluded directories at the OS level. The drain function must filter events: only invalidate files with supported source extensions and that pass the same scoping rules as `walk_project_files()`.
- **`CallGraph::project_files` cache must be invalidated on Create/Remove events** — When a new file is added or removed, `project_files: Option<Vec<PathBuf>>` must be set to `None` so the next scan picks it up.

## Common Pitfalls

- **Borrow conflict between receiver drain and graph mutation** — If the receiver and graph are in the same `RefCell`, draining events (borrow receiver) and then invalidating (borrow_mut graph) would panic. Solution: store watcher receiver in a separate `RefCell` from the callgraph.
- **File watcher event storms during git operations** — `git checkout` or `npm install` can fire hundreds of events. The drain loop collects all paths into a `HashSet` before invalidating, so each file is invalidated at most once per dispatch cycle. No performance issue.
- **Canonicalization differences between watcher paths and graph paths** — `notify` may report paths differently from what the graph stores (e.g., `/private/var/...` vs `/var/...` on macOS with tmpdir). The `invalidate_file()` method must canonicalize the incoming path before lookup. `CallGraph` already has `canonicalize()` for this.
- **Reverse index stale after invalidation** — After invalidating a file, the reverse index is cleared but callers from *other* files pointing at the invalidated file are still valid. However, re-importing a changed file's exports could change resolution. Clearing the whole reverse index is the safe choice.
- **`callers` depth expansion creates N+1 query pattern** — If `callers` supports recursive expansion (who calls the callers), each level requires another round of reverse lookups. Depth limits (default 5) bound this. Each level reuses the cached reverse index so it's fast after the initial build.
- **Watcher must be kept alive** — Dropping the `RecommendedWatcher` stops file watching. The `RefCell<Option<RecommendedWatcher>>` in AppContext keeps it alive for the process lifetime. Don't accidentally drop it during `configure` re-initialization (if called twice, drop old watcher first).

## Open Risks

- **macOS FSEvents coalescing** — FSEvents can coalesce multiple changes into one event with a generic `Any` kind. The drain function should treat any event on a source file as invalidation, regardless of `EventKind`. Don't filter by event kind.
- **Large monorepo first-scan performance** — A project with 5000+ source files could take 3-5s on first `callers` query. This is within the 5s target from CONTEXT but worth monitoring. The scan is one-time per process lifecycle (subsequent queries hit cache).
- **Re-configure behavior** — If the plugin calls `configure` twice (e.g., workspace switch), the old watcher must be dropped and a new one created. The old graph data is stale and should be cleared entirely.
- **Integration test for file watcher** — Testing the watcher cycle (modify file on disk → query reflects change) requires timing. The test must: configure, query callers (builds cache), modify a fixture file on disk, send another command (which triggers drain), query callers again, verify the change is reflected. A small `sleep` or a flush command may be needed between file modification and query to ensure the OS delivers the event.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | actionbook/rust-skills@rust-code-navigator (44 installs) | available — general Rust navigation, not M003-specific |
| Rust | sammcj/agentic-coding@rust-engineer (36 installs) | available — general Rust engineering, low relevance |

No skills are directly relevant. The `notify` crate is well-documented and the integration pattern is straightforward.

## Sources

- `notify` v8.2.0 — `EventHandler` implemented for `std::sync::mpsc::Sender<Result<Event>>` natively (verified: compiled and ran test code)
- `notify` v8.2.0 — fires Create + Modify(Metadata) + Modify(Data) for a single `fs::write()` on macOS (verified: ran test code, observed 3 events for 1 write)
- `notify` v8.2.0 transitive deps: walkdir, bitflags, libc, log, fsevent-sys, notify-types (verified: `cargo tree`)
- `RecommendedWatcher` works inside `RefCell` — single-threaded usage compiles and runs (verified: test code)
- `std::sync::mpsc::Receiver::try_recv()` — non-blocking drain, returns `Err(TryRecvError::Empty)` when no events pending (stdlib docs)
- No `crossbeam-channel` dependency needed — D075 specified it but `std::sync::mpsc` is sufficient since `notify` v8 supports it natively, saving a dependency
