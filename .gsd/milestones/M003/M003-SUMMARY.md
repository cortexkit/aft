---
id: M003
provides:
  - Lazy cross-file call graph engine with per-file caching and import-based edge resolution
  - Forward call tree traversal (call_tree) with depth limiting and cycle detection
  - Reverse caller lookup (callers) with recursive depth expansion and file grouping
  - Backward trace to entry points (trace_to) with BFS multi-path discovery
  - Change impact analysis (impact) with signature/entry-point/parameter annotations
  - Data flow tracking (trace_data) through assignments and cross-file parameter matching
  - Entry point detection heuristics (exported functions, main/init, test patterns per language)
  - File watcher (notify v8) with drain-at-dispatch invalidation pattern
  - Worktree-scoped file walking via ignore crate respecting .gitignore
  - configure command for project root initialization
  - Plugin tools aft_configure, aft_call_tree, aft_callers, aft_trace_to, aft_impact, aft_trace_data
key_decisions:
  - D083: Call extraction extracted to shared calls.rs module
  - D084: Config wrapped in RefCell for runtime mutation via configure
  - D085: ignore crate 0.4.x for worktree-scoped file walking
  - D086: Aliased import resolution via raw text parsing
  - D087: Full callee expression in calls.rs for namespace-aware resolution
  - D088: CallGraph stored as RefCell<Option<CallGraph>> — None until configure
  - D089: Configure-then-use pattern for all graph commands
  - D090: std::sync::mpsc for watcher channel (supersedes D075 crossbeam)
  - D091: Separate RefCells for watcher receiver and callgraph
  - D092: Reverse index cleared entirely on invalidation
  - D093: SymbolMeta in FileCallData for entry point detection
  - D094: Per-path visited sets in trace_to BFS
  - D095: Default trace_to depth 10
  - D096: trace_to continues past intermediate entry points
  - D097: symbol_metadata as third existence check
  - D098: Bracket-depth-aware parameter extraction
  - D099: trace_data re-parses file AST directly
patterns_established:
  - Configure-then-use guard — graph commands return not_configured error before configure is called
  - EdgeResolution enum marks unresolved edges explicitly — never silently dropped
  - Two-phase drain pattern for file watcher — borrow receiver, collect paths, drop borrow, then borrow_mut callgraph to invalidate
  - extract_calls_full returns (full_callee, short_name, line) triples for namespace-aware resolution
  - is_entry_point as standalone pure function for easy extension per-language
  - extract_parameters with bracket-depth-aware comma splitting for cross-language parameter parsing
  - DataFlowHop with approximate flag for agent-consumable uncertainty signals
observability_surfaces:
  - "[aft] project root set: <path>" stderr log on configure
  - "[aft] watcher started: <path>" stderr log on configure
  - "[aft] invalidated N files" stderr log when drain processes changed source files
  - call_tree nodes have resolved:true/false — unresolved edges are leaf nodes with callee name only
  - callers response includes total_callers and scanned_files counts
  - trace_to response includes total_paths, entry_points_found, max_depth_reached, truncated_paths
  - impact response includes total_affected and affected_files counts
  - trace_data hops include approximate flag for individual uncertainty markers
  - Structured error codes across all commands: not_configured, symbol_not_found, invalid_request
requirement_outcomes:
  - id: R020
    from_status: active
    to_status: validated
    proof: S01 proved lazy per-file construction with HashMap<PathBuf, FileCallData> and worktree scoping via ignore crate. S02 proved file watcher invalidation with notify v8 — integration tests exercise modify-then-query and remove-then-query cycles.
  - id: R021
    from_status: active
    to_status: validated
    proof: S01 — 7 integration tests prove cross-file call tree (main→processData→validate across 3 files), depth limiting, aliased import resolution, error paths. Plugin tool round-trip verified.
  - id: R022
    from_status: active
    to_status: validated
    proof: S02 — 4 integration tests prove cross-file callers grouped by file, recursive depth expansion, empty result handling, error guards. 2 watcher cycle tests prove modify-then-query reflects changes.
  - id: R023
    from_status: active
    to_status: validated
    proof: S03 — 5 integration tests prove backward traversal from deeply-nested utility to all entry points, multi-path traces, diagnostic metadata. 5 unit tests prove BFS path collection, cycle detection, depth limiting.
  - id: R024
    from_status: active
    to_status: validated
    proof: S04 — 5 integration tests prove assignment tracking, cross-file parameter matching, approximation marking on destructuring/spread, error paths. Response includes depth_limited flag and per-hop approximate markers.
  - id: R025
    from_status: active
    to_status: validated
    proof: S04 — 3 integration tests prove multi-caller impact with signatures, entry point flags, call expressions, extracted parameters. 15 unit tests prove extract_parameters across all 6 languages.
  - id: R026
    from_status: active
    to_status: validated
    proof: S03 — 8 unit tests prove classification of exported functions, main/init/setup/bootstrap/run patterns, language-specific test patterns (TS/JS/Python/Rust/Go), and negative cases.
  - id: R027
    from_status: active
    to_status: validated
    proof: S01 — walk_project_files uses ignore crate respecting .gitignore with hardcoded exclusions for node_modules, target, venv, .git. Unit tests prove gitignore exclusion and source-file-only filtering.
duration: 4 slices, ~4h
verification_result: passed
completed_at: 2026-03-14
---

# M003: Call Graph Navigation

**Five single-call code navigation primitives — forward call tree, reverse callers, trace-to-entry-points, impact analysis, and data flow tracking — replacing the most token-expensive agent workflow (~5000 tokens for a 4-file trace) with ~400-token operations backed by a lazy cross-file call graph with file watcher invalidation.**

## What Happened

**S01 (Call Graph Infrastructure + Forward Call Tree)** extracted shared call-site helpers from zoom.rs into `calls.rs`, then built the core `CallGraph` engine in `callgraph.rs`. The engine stores per-file call sites and exports in `FileCallData`, builds lazily on demand, and resolves cross-file edges through import chains (direct imports, aliased imports via text parsing, namespace imports, barrel re-exports). `forward_tree()` does depth-limited recursive traversal with HashSet cycle detection. File walking uses the `ignore` crate for worktree-scoped discovery respecting .gitignore. The `configure` command sets project_root and initializes the graph; `call_tree` returns nested cross-file trees through the binary protocol. 22 new tests.

**S02 (Reverse Callers + File Watcher)** added the reverse caller index — `build_reverse_index()` scans all project files and inverts call edges into a `HashMap<(PathBuf, String), Vec<CallerSite>>`. `callers_of()` does recursive depth expansion with cycle detection. `invalidate_file()` clears file data and the reverse index for lazy rebuild. The `notify` v8 file watcher runs on a background thread with `std::sync::mpsc` for event delivery. A two-phase drain pattern in main.rs (borrow receiver → collect paths → drop → borrow_mut callgraph → invalidate) avoids RefCell borrow conflicts. Integration tests prove modify-then-query and remove-then-query cycles work correctly. 10 new tests.

**S03 (Trace to Entry Points)** added `is_entry_point()` — a pure function detecting exported functions, main/init patterns, and language-specific test patterns across 6 languages. `SymbolMeta` on `FileCallData` stores kind/exported/signature per symbol. `trace_to()` uses BFS backward traversal through the reverse index with per-path visited sets, continuing past intermediate entry points to find all reachable paths. Paths render top-down (entry point first). A `lookup_file_data()` helper handles path canonicalization mismatches between walker paths and reverse index keys. 18 new tests.

**S04 (Data Flow Tracking + Impact Analysis)** completed the suite. `impact()` reuses `callers_of()` and enriches each caller with signature, entry point status, call expression, and extracted parameters via `extract_parameters()` — a shared utility handling receiver skipping, generics (bracket-depth-aware comma splitting), defaults, and rest params across all 6 languages. `trace_data()` re-parses the target file's AST to walk variable assignments and cross-file parameter passing, with `approximate: true` markers where static analysis loses confidence (destructuring, spread). 23 new tests.

Total: 74 new callgraph tests (48 unit + 26 integration) bringing the project to 368 Rust tests + 39 plugin tests, all passing.

## Cross-Slice Verification

**Success Criterion 1 — `call_tree` on a function in a multi-file project returns cross-file call tree:**
Verified by `callgraph_cross_file_tree` integration test — calls `call_tree` on `processData` in `main.ts`, receives a tree spanning main.ts → utils.ts → helpers.ts with resolved file paths, function signatures, and depth-limited traversal. `callgraph_depth_limit_truncates` proves depth limiting. `callgraph_aliased_import_resolution` proves aliased import handling. All through binary protocol.

**Success Criterion 2 — `callers` returns all call sites, subsequent query after file modification reflects change:**
Verified by `callgraph_callers_cross_file` (callers grouped by file), `callgraph_callers_recursive` (depth expansion), and the watcher cycle tests: `callgraph_watcher_add_caller` writes a new caller file → sleep → ping (triggers drain) → callers returns the new caller. `callgraph_watcher_remove_caller` rewrites a file removing a call → subsequent callers query shows it gone.

**Success Criterion 3 — `trace_to` on deeply-nested utility returns all paths from entry points:**
Verified by `callgraph_trace_to_single_path` (checkFormat → validate → processData → main, rendered top-down) and `callgraph_trace_to_multi_path` (validate found through 2+ distinct entry points). `trace_to_no_entry_points` proves graceful handling. Response includes total_paths, entry_points_found, max_depth_reached diagnostic fields.

**Success Criterion 4 — `impact` on a function with 5+ callers across 3+ files returns all affected call sites:**
Verified by `callgraph_impact_multi_caller` integration test — calls `impact` on `validate` which has callers across multiple files, receives callers annotated with signatures, entry point status (`is_entry_point` flag), call expressions, and extracted parameter names. `total_affected` and `affected_files` counts confirmed.

**Success Criterion 5 — `trace_data` tracks variable renames and type transformations:**
Verified by `callgraph_trace_data_assignment_tracking` (follows variable through assignment chain within a file), `callgraph_trace_data_cross_file` (follows value through function parameter into another file with parameter name matching), and `callgraph_trace_data_approximation` (marks destructuring/spread with `approximate: true` and stops tracking on that branch).

**Success Criterion 6 — Worktree boundaries respected:**
Verified by `callgraph_walker_excludes_gitignored` and `callgraph_walker_only_source_files` unit tests — .gitignore patterns respected, node_modules/target/venv/.git excluded, only source files (ts/tsx/js/jsx/py/rs/go) included.

**Definition of Done verification:**
- All 5 commands work through binary protocol AND plugin tool registration: confirmed by 26 integration tests + 39 bun tests (including navigation tool registration)
- Cross-file call resolution follows import/export chains: proven by aliased import, namespace import, barrel re-export, and direct import tests
- File watcher detects modified files: proven by 2 watcher cycle integration tests
- Cycle detection and depth limits on all traversals: proven by `callgraph_cycle_detection_stops`, `callgraph_depth_limit_truncates`, `trace_to_cycle_detection`, `trace_to_depth_limit` unit tests
- Worktree scoping: proven by ignore crate tests
- `cargo test`: 368 passed, 0 failed (223 unit + 145 integration)
- `bun test`: 39 passed, 0 failed

All success criteria met. All definition of done items satisfied.

## Requirement Changes

- R020: active → validated — Lazy per-file construction and file watcher invalidation proven by unit tests and watcher cycle integration tests
- R021: active → validated — Forward call tree proven by 7 integration tests through binary protocol
- R022: active → validated — Reverse callers proven by 4 integration tests + 2 watcher cycle tests
- R023: active → validated — Backward trace to entry points proven by 5 integration tests with multi-path support
- R024: active → validated — Data flow tracking proven by 5 integration tests with assignment and cross-file parameter tracking
- R025: active → validated — Impact analysis proven by 3 integration tests with signature/entry-point annotations
- R026: active → validated — Entry point detection proven by 8 unit tests across 6 languages
- R027: active → validated — Worktree scoping proven by ignore crate integration with .gitignore respect

## Forward Intelligence

### What the next milestone should know
- The call graph infrastructure is proven and stable — 74 tests cover the full resolution pipeline. M004's `move_symbol` (R028) can use `callers_of()` to find all consumers of a symbol before rewiring imports.
- `extract_parameters()` is the shared utility for argument-to-parameter matching across all 6 languages. M004's `extract_function` (R029) will need this for parameter inference.
- `symbol_metadata` on `FileCallData` provides kind/exported/signature per symbol — useful for M004's move/extract/inline to understand symbol characteristics without re-parsing.
- The configure-then-use pattern means the plugin must call `aft_configure` before any graph command. All 6 graph commands share this guard.

### What's fragile
- Path canonicalization mismatch — `build_reverse_index` stores data under raw walker paths while `CallerSite.caller_file` uses canonical paths. `lookup_file_data()` papers over it but any new code touching the data cache should be aware.
- Aliased import resolution via raw text parsing (`find_alias_original`) — brittle against edge cases but covers normal imports. If alias handling gets more complex, consider extending `ImportStatement` with alias fields.
- Barrel file re-export resolution is shallow (one level only) — could produce false unresolved edges for deeply re-exported symbols.
- Watcher integration tests use `thread::sleep(500ms)` — FSEvents timing is non-deterministic, could theoretically flake under heavy load.

### Authoritative diagnostics
- `cargo test -- callgraph` runs all 74 callgraph tests — the single command that proves the entire subsystem
- `EdgeResolution::Unresolved` in callgraph.rs — every unresolved cross-file edge is explicitly tagged, never silently dropped
- `TraceToResult` diagnostic fields (total_paths, entry_points_found, max_depth_reached, truncated_paths) let agents assess completeness
- `[aft] invalidated N files` stderr log confirms watcher drain is processing events

### What assumptions changed
- D074 assumed `RefCell<CallGraph>` — implementation uses `RefCell<Option<CallGraph>>` (D088) because the graph can't be initialized without project_root from configure
- D075 assumed crossbeam-channel — D090 switched to std::sync::mpsc because notify v8 implements EventHandler for mpsc::Sender natively
- Assumed `calls_by_symbol` + `exported_symbols` sufficient for symbol existence — leaf functions require `symbol_metadata` as a third source (D097)

## Files Created/Modified

- `Cargo.toml` — added `ignore = "0.4"` and `notify = "8"` dependencies
- `src/calls.rs` — shared call extraction helpers (extracted from zoom.rs) with full-callee variants
- `src/callgraph.rs` — complete call graph engine: FileCallData, CallGraph, forward_tree, callers_of, trace_to, impact, trace_data, is_entry_point, extract_parameters, file watcher invalidation, 48 unit tests
- `src/lib.rs` — added pub mod calls, pub mod callgraph
- `src/context.rs` — Config wrapped in RefCell, callgraph as RefCell<Option<CallGraph>>, watcher fields
- `src/commands/configure.rs` — configure command handler with watcher initialization
- `src/commands/call_tree.rs` — call_tree command handler
- `src/commands/callers.rs` — callers command handler
- `src/commands/trace_to.rs` — trace_to command handler
- `src/commands/impact.rs` — impact command handler
- `src/commands/trace_data.rs` — trace_data command handler
- `src/commands/mod.rs` — added all new command modules
- `src/commands/zoom.rs` — refactored to delegate call extraction to calls.rs
- `src/main.rs` — wired all 6 commands in dispatch, drain_watcher_events before dispatch
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
- `tests/integration/callgraph_test.rs` — 26 integration tests
- `tests/integration/main.rs` — registered callgraph_test module
- `tests/fixtures/callgraph/main.ts` — multi-file test fixture
- `tests/fixtures/callgraph/utils.ts` — multi-file test fixture
- `tests/fixtures/callgraph/helpers.ts` — multi-file test fixture
- `tests/fixtures/callgraph/index.ts` — barrel re-export fixture
- `tests/fixtures/callgraph/aliased.ts` — aliased import fixture
- `tests/fixtures/callgraph/service.ts` — exported handleRequest fixture
- `tests/fixtures/callgraph/test_helpers.ts` — test function fixture
- `tests/fixtures/callgraph/data_flow.ts` — assignment chain + cross-file call fixture
- `tests/fixtures/callgraph/data_processor.ts` — cross-file target function fixture
- `opencode-plugin-aft/src/tools/navigation.ts` — all 6 plugin tools with Zod schemas
- `opencode-plugin-aft/src/index.ts` — wired navigation tools
