# S02: Reverse Callers + File Watcher

**Goal:** `callers` command returns all call sites for a symbol grouped by file with recursive depth expansion; file watcher invalidates the call graph on file changes so subsequent queries reflect modifications.

**Demo:** Agent calls `aft_callers` on a utility function and sees all call sites grouped by file. Agent modifies a calling file on disk, sends another command (triggering watcher drain), and a subsequent `aft_callers` query reflects the change.

## Must-Haves

- Reverse index in `CallGraph` that scans all project files and inverts `calls_by_symbol` via cross-file resolution
- `callers` command handler returning call sites grouped by file with recursive depth expansion
- `invalidate_file(path)` that removes a file's `FileCallData` and clears the reverse index
- File watcher using `notify` v8 initialized during `configure`, stored in `AppContext`
- Drain-at-dispatch pattern in `main.rs` — `try_recv()` loop before `dispatch()`, deduplicated by `PathBuf`
- Watcher event filtering to only invalidate supported source file extensions
- `aft_callers` plugin tool with Zod schema
- Integration tests for `callers` (static) and file-modification-then-query cycle (watcher)

## Proof Level

- This slice proves: operational (file watcher invalidation cycle is a real-time OS integration)
- Real runtime required: yes (watcher requires actual filesystem events from the OS)
- Human/UAT required: no

## Verification

- `cargo test -- callgraph` — all existing 22 tests pass + new callers + watcher invalidation tests
- `cargo test` — all tests pass (316 existing + new)
- `bun test` — all pass (39 existing + new callers tool test)
- Integration test: configure → `callers` → modify fixture file → send command (triggers drain) → `callers` again → verify changed result
- Integration test: `callers` without configure returns `not_configured` error; `callers` for symbol with no callers returns empty result with `total_callers: 0`

## Observability / Diagnostics

- Runtime signals: `[aft] watcher started: <path>` stderr log on configure; `[aft] invalidated N files` stderr log when drain processes events
- Inspection surfaces: `callers` response includes `total_callers` count and `scanned_files` count for visibility into reverse index coverage
- Failure visibility: `not_configured` error if callers called before configure; watcher initialization failure logged to stderr but non-fatal (callers still works, just no live invalidation)

## Integration Closure

- Upstream surfaces consumed: `CallGraph` struct, `build_file()`, `resolve_cross_file_edge()`, `walk_project_files()`, `AppContext` RefCell pattern, `configure` command handler
- New wiring introduced in this slice: `notify::RecommendedWatcher` + `mpsc::Receiver` stored in `AppContext`; drain hook in `main.rs` dispatch loop; `callers` command in dispatch table; `aft_callers` in plugin tool registration
- What remains before the milestone is truly usable end-to-end: S03 (trace_to), S04 (trace_data + impact)

## Tasks

- [x] **T01: Reverse index + `callers` command + plugin tool** `est:1h30m`
  - Why: Delivers R022 (reverse caller tree) — the core feature of S02. The reverse index and command handler are self-contained and fully testable with static fixtures.
  - Files: `src/callgraph.rs`, `src/commands/callers.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/integration/callgraph_test.rs`, `opencode-plugin-aft/src/tools/navigation.ts`
  - Do: Add `CallerSite` struct + `reverse_index: Option<HashMap<(PathBuf, String), Vec<CallerSite>>>` to `CallGraph`. Implement `build_reverse_index()` that scans all project files and inverts calls_by_symbol using `resolve_cross_file_edge()`. Implement `callers_of(file, symbol, depth)` returning callers grouped by file with recursive expansion. Add `invalidate_file(path)` that removes file from `data` HashMap, sets `reverse_index = None`, clears `project_files` cache. Build `callers` command handler following `call_tree` pattern. Register in dispatch + plugin.
  - Verify: `cargo test -- callgraph` passes all existing + new tests; `bun test` passes
  - Done when: `callers` returns correct cross-file callers grouped by file in integration test, recursive depth expansion works, `invalidate_file` clears the reverse index

- [x] **T02: File watcher integration + invalidation cycle tests** `est:1h`
  - Why: Completes R020 (file watcher invalidation) and proves the milestone's key risk — that the drain-at-dispatch pattern preserves single-threaded RefCell safety. Also proves the modify-then-query cycle that the roadmap demands.
  - Files: `Cargo.toml`, `src/context.rs`, `src/commands/configure.rs`, `src/main.rs`, `tests/integration/callgraph_test.rs`
  - Do: Add `notify = "0.8"` dep to Cargo.toml. Add `RefCell<Option<mpsc::Receiver<...>>>` and `RefCell<Option<RecommendedWatcher>>` to `AppContext`. Extend `configure` to create watcher + channel, watch project_root recursively, store in context. Add `drain_watcher_events(ctx)` in `main.rs` before `dispatch()` — try_recv loop, dedup by PathBuf HashSet, filter by source extensions, call `invalidate_file()` for each, stderr log count. Integration test: configure → callers → fs::write modified fixture → small delay → send ping (triggers drain) → callers again → assert changed results.
  - Verify: `cargo test -- callgraph` passes all tests including watcher cycle; `cargo test` all green; no borrow panics
  - Done when: integration test proves modify-file-then-query cycle reflects changes; watcher drain doesn't panic from RefCell borrow conflicts

## Files Likely Touched

- `Cargo.toml`
- `src/callgraph.rs`
- `src/commands/callers.rs` (new)
- `src/commands/mod.rs`
- `src/commands/configure.rs`
- `src/context.rs`
- `src/main.rs`
- `tests/integration/callgraph_test.rs`
- `opencode-plugin-aft/src/tools/navigation.ts`
