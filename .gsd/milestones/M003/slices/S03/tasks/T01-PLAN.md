---
estimated_steps: 8
estimated_files: 4
---

# T01: Implement entry point detection and backward path traversal

**Slice:** S03 — Trace to Entry Points
**Milestone:** M003

## Description

Extend the callgraph data model to carry symbol kind metadata, implement `is_entry_point()` heuristics for detecting entry points (exported functions, main/init patterns, test patterns per language), and build the `trace_to()` backward path traversal algorithm. This task produces the core algorithm verified by unit tests — protocol wiring is T02.

## Steps

1. Add `kind: SymbolKind` field to the private `SymbolInfo` struct in `callgraph.rs`. Import `SymbolKind` from `symbols.rs`. Map `s.kind` in `list_symbols_from_tree()` — it already has access to the full `Symbol` struct.

2. Add `SymbolMeta` struct to `callgraph.rs` with fields `kind: SymbolKind`, `exported: bool`, `signature: Option<String>`. Add `symbol_metadata: HashMap<String, SymbolMeta>` field to `FileCallData`. Populate it in `build_file_data()` from the symbols list.

3. Implement `is_entry_point(name: &str, kind: &SymbolKind, exported: bool, lang: LangId) -> bool` as a standalone function in `callgraph.rs`:
   - Exported functions: `exported && kind == Function` (not methods — methods are class members, not standalone entry points)
   - Main/init patterns: name matches `main`, `init`, `setup`, `bootstrap`, `run` (case-insensitive exact)
   - Test patterns by language: TS/JS/TSX — `describe`, `it`, `test`, or starts with `test`/`spec`; Python — starts with `test_` or matches `setUp`/`tearDown`; Rust — starts with `test_`; Go — starts with `Test`

4. Add `TraceHop`, `TracePath`, `TraceToResult` types to `callgraph.rs` with `#[derive(Serialize)]`:
   - `TraceHop`: `symbol`, `file` (relative string), `line`, `signature` (Option), `is_entry_point` (bool)
   - `TracePath`: `hops: Vec<TraceHop>` (top-down: entry point first, target last)
   - `TraceToResult`: `target_symbol`, `target_file`, `paths: Vec<TracePath>`, `total_paths`, `entry_points_found`, `max_depth_reached`, `truncated_paths` (count of paths that hit dead ends)

5. Implement `pub fn trace_to(&mut self, file: &Path, symbol: &str, max_depth: usize) -> Result<TraceToResult, AftError>` on `CallGraph`:
   - Ensure reverse index is built (same lazy pattern as `callers_of`)
   - BFS backward from `(file, symbol)`: for each node, look up callers in reverse index
   - Track complete paths as `Vec<(PathBuf, String, u32)>` during traversal
   - If a caller is an entry point (check `symbol_metadata` from its file's `FileCallData`), record the complete path
   - If not an entry point and depth < max_depth, continue backward
   - Per-path visited set `HashSet<(PathBuf, String)>` for cycle detection
   - After collection, reverse each path so it reads top-down (entry point → ... → target)
   - All path lookups must use canonicalized paths (S02 forward intelligence warning)

6. Create multi-path test fixture files in `tests/fixtures/callgraph/`:
   - `service.ts`: exported function `handleRequest` that calls `processData` from utils.ts — creates a second path to `checkFormat`
   - `test_helpers.ts`: test function `testValidation` that calls `validate` from helpers.ts — creates a third path via test entry point
   - This gives `checkFormat` three paths: `main→processData→validate→checkFormat`, `handleRequest→processData→validate→checkFormat`, `testValidation→validate→checkFormat`

7. Write unit tests in `callgraph.rs::tests`:
   - `is_entry_point` tests: exported function → true, exported method → false, main/init → true, test patterns per language, non-exported non-main → false
   - `trace_to` test: setup multi-path project, verify paths from entry points to leaf function
   - `trace_to` cycle test: verify cycle detection terminates
   - `trace_to` depth limit test: verify max_depth truncation with `max_depth_reached` flag

## Must-Haves

- [ ] `SymbolInfo.kind` field populated from `Symbol.kind` in `list_symbols_from_tree`
- [ ] `FileCallData.symbol_metadata` populated in `build_file_data`
- [ ] `is_entry_point()` handles exported functions, main/init, test patterns
- [ ] `trace_to()` returns correct top-down paths using reverse index
- [ ] Cycle detection prevents infinite loops
- [ ] Depth limiting works with `max_depth_reached` flag
- [ ] Paths that reach dead ends (no callers, not entry point) counted as `truncated_paths`
- [ ] All reverse index lookups use canonicalized paths
- [ ] Multi-path fixture files created

## Verification

- `cargo test -- callgraph` passes with new unit tests
- `cargo test -- is_entry_point` tests pass
- `cargo test -- trace_to` unit tests pass
- No existing tests broken

## Observability Impact

- **New runtime signals:** `TraceToResult` struct includes `total_paths`, `entry_points_found`, `max_depth_reached`, and `truncated_paths` — these let callers (and future agents) assess completeness of trace results without guessing.
- **Inspection surface:** `symbol_metadata` on `FileCallData` makes entry point classification inspectable per-file without re-parsing. A future agent can check `graph.build_file(path).symbol_metadata` to see which symbols are classified as what kind.
- **Failure visibility:** `trace_to()` returns `AftError` for file-not-found / parse-failure cases. Dead-end paths (callers with no further callers that aren't entry points) are counted in `truncated_paths` rather than silently dropped.
- **Diagnostic path:** When `trace_to` returns 0 paths, the agent can check `entry_points_found == 0` (no entry points at all) vs `truncated_paths > 0` (paths existed but didn't reach entry points) to distinguish between disconnected code and insufficient depth.

## Inputs

- `src/callgraph.rs` — existing `CallGraph`, `FileCallData`, `SymbolInfo`, `build_file_data()`, `build_reverse_index()`, `callers_of()` from S01/S02
- `src/symbols.rs` — `SymbolKind` enum with 7 variants
- S02 forward intelligence: reverse index is `HashMap<(PathBuf, String), Vec<CallerSite>>`, path canonicalization is critical, `callers_of()` is the public API for reverse lookups

## Expected Output

- `src/callgraph.rs` — extended with `SymbolMeta`, `is_entry_point()`, `TraceHop`/`TracePath`/`TraceToResult`, `trace_to()` method, unit tests
- `tests/fixtures/callgraph/service.ts` — new fixture for multi-path testing
- `tests/fixtures/callgraph/test_helpers.ts` — new fixture for test-entry-point path
