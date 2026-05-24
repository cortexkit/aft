---
estimated_steps: 8
estimated_files: 6
---

# T01: Reverse index + `callers` command + plugin tool

**Slice:** S02 — Reverse Callers + File Watcher
**Milestone:** M003

## Description

Build the reverse caller index in `CallGraph` and the `callers` protocol command. The reverse index inverts the existing per-file `calls_by_symbol` data using `resolve_cross_file_edge()` to map `(target_file, target_symbol) → Vec<CallerSite>`. First `callers` query triggers a full project scan (all files via `walk_project_files()`), then caches the index. `invalidate_file()` clears the index for lazy rebuild.

The `callers` command follows the exact pattern of `call_tree`: check `Option<CallGraph>`, extract params, call `callers_of()`, serialize result.

## Steps

1. Add `CallerSite` struct to `callgraph.rs`: `{ caller_file: PathBuf, caller_symbol: String, line: u32, col: u32, resolved: bool }`
2. Add `reverse_index: Option<HashMap<(PathBuf, String), Vec<CallerSite>>>` field to `CallGraph`
3. Implement `build_reverse_index(&mut self)` — iterate all project files via `walk_project_files()`, `build_file()` each, then for each `(symbol, call_sites)` pair resolve the cross-file edge and insert into the reverse map
4. Implement `callers_of(&mut self, file: &Path, symbol: &str, depth: usize)` — returns callers grouped by file, with optional recursive expansion (callers-of-callers up to depth). Return type: a struct serializable to JSON with `callers: Vec<CallerGroup>` where `CallerGroup { file, callers: Vec<{symbol, line}> }`
5. Implement `invalidate_file(&mut self, path: &Path)` — remove file from `data` HashMap, set `reverse_index = None`, set `project_files = None` (for Create/Remove events)
6. Create `src/commands/callers.rs` — `handle_callers()` extracting `file`, `symbol`, `depth` params, following configure-then-use guard pattern from `call_tree.rs`
7. Wire `callers` in `src/commands/mod.rs`, `src/main.rs` dispatch, and add `aft_callers` tool in `navigation.ts`
8. Add integration tests: callers with known multi-file fixture (main.ts calls helpers), callers without configure (not_configured error), callers for symbol with no callers (empty result), callers recursive depth

## Must-Haves

- [ ] `CallerSite` struct with caller file, symbol, line, resolved flag
- [ ] Reverse index built by scanning all project files and resolving cross-file edges
- [ ] `callers_of()` returns callers grouped by file
- [ ] `callers_of()` supports recursive depth expansion (callers of callers)
- [ ] `invalidate_file()` removes file data and clears reverse index
- [ ] `callers` command handler with configure-then-use guard
- [ ] `aft_callers` plugin tool with Zod schema
- [ ] Integration tests proving cross-file callers, not-configured error, empty callers

## Verification

- `cargo test -- callgraph` — all 22 existing + new callers unit/integration tests pass
- `cargo test` — all pass, 0 failures
- `bun test` — all pass, `aft_callers` tool registered
- Integration test: configure → callers for `validate` in helpers.ts → response contains caller from utils.ts

## Observability Impact

- `callers` response includes `total_callers` and `scanned_files` counts — lets agents gauge reverse index coverage
- `symbol_not_found` error code if target symbol doesn't exist in the file
- `not_configured` error code if callers called before configure

## Inputs

- `src/callgraph.rs` — `CallGraph`, `FileCallData`, `CallSite`, `EdgeResolution`, `walk_project_files()`, `resolve_cross_file_edge()`
- `src/commands/call_tree.rs` — handler pattern to follow (configure-then-use guard, param extraction)
- `opencode-plugin-aft/src/tools/navigation.ts` — tool registration pattern
- `tests/fixtures/callgraph/` — existing multi-file TypeScript fixtures

## Expected Output

- `src/callgraph.rs` — extended with `CallerSite`, reverse index, `build_reverse_index()`, `callers_of()`, `invalidate_file()`, + unit tests
- `src/commands/callers.rs` — new command handler
- `src/commands/mod.rs` — `pub mod callers;`
- `src/main.rs` — `"callers"` in dispatch match
- `tests/integration/callgraph_test.rs` — 4+ new integration tests
- `opencode-plugin-aft/src/tools/navigation.ts` — `aft_callers` tool definition
