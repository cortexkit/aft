---
id: S02
parent: M003
milestone: M003
provides:
  - Reverse caller index in CallGraph with lazy full-project scan and recursive depth expansion
  - callers protocol command returning call sites grouped by file
  - invalidate_file() clearing file data and reverse index for lazy rebuild
  - notify v8 file watcher initialized during configure, drain-at-dispatch pattern in main.rs
  - aft_callers plugin tool with Zod schema
requires:
  - slice: S01
    provides: CallGraph struct, build_file(), resolve_cross_file_edge(), walk_project_files(), AppContext RefCell pattern, configure command
affects:
  - S03
key_files:
  - src/callgraph.rs
  - src/commands/callers.rs
  - src/context.rs
  - src/commands/configure.rs
  - src/main.rs
  - tests/integration/callgraph_test.rs
  - opencode-plugin-aft/src/tools/navigation.ts
key_decisions:
  - D090 (std::sync::mpsc for watcher channel — supersedes D075 crossbeam)
  - D091 (separate RefCells for watcher receiver and callgraph — avoids borrow conflicts during drain)
  - D092 (reverse index cleared entirely on invalidation — full rebuild on next query)
patterns_established:
  - Two-phase drain pattern: phase 1 borrows receiver to collect paths into HashSet, phase 2 borrows callgraph mutably to invalidate — no overlapping RefCell borrows
  - Caller file paths must be canonicalized for consistent lookup (walk_project_files returns non-canonical, EdgeResolution targets are canonical)
  - Source extension filtering (ts/tsx/js/jsx/py/rs/go) applied during drain, not at OS watcher level
observability_surfaces:
  - "[aft] watcher started: <path>" stderr log on configure
  - "[aft] invalidated N files" stderr log when drain processes changed source files
  - "[aft] watcher watch error: <err>" and "[aft] watcher init failed: <err>" — non-fatal warnings
  - callers response includes total_callers and scanned_files counts
  - not_configured and symbol_not_found structured error codes
drill_down_paths:
  - .gsd/milestones/M003/slices/S02/tasks/T01-SUMMARY.md
  - .gsd/milestones/M003/slices/S02/tasks/T02-SUMMARY.md
duration: 45m
verification_result: passed
completed_at: 2026-03-14
---

# S02: Reverse Callers + File Watcher

**Reverse caller index with recursive depth expansion, `callers` command, and `notify` v8 file watcher with drain-at-dispatch invalidation — proving the modify-then-query cycle works without RefCell borrow panics.**

## What Happened

T01 built the reverse caller index and command. Added `CallerSite`, `CallerGroup`, `CallerEntry`, and `CallersResult` types to `callgraph.rs`. `build_reverse_index()` scans all project files, builds file data, and inverts `calls_by_symbol` through `resolve_cross_file_edge()` into a `HashMap<(PathBuf, String), Vec<CallerSite>>`. `callers_of(file, symbol, depth)` lazily triggers the index build and uses `collect_callers_recursive()` with visited-set cycle detection. `invalidate_file(path)` removes file data, clears the reverse index (for lazy rebuild), and clears the project_files cache. Command handler follows the configure-then-use guard pattern from `call_tree.rs`.

Key implementation detail: caller paths must be canonicalized because `walk_project_files()` returns non-canonical paths while `EdgeResolution::Resolved` targets are canonical (from `resolve_module_path → std::fs::canonicalize`). Without this, recursive lookups fail to match reverse index keys.

T02 integrated the file watcher. Added `notify = "8"` to Cargo.toml. Extended `AppContext` with separate `RefCell<Option<_>>` fields for the `mpsc::Receiver` and `RecommendedWatcher` handle (D091 — separate RefCells avoid borrow conflicts). `handle_configure` creates the watcher channel, watches project_root recursively, stores both in context. `drain_watcher_events()` in `main.rs` runs before every `dispatch()` using a two-phase pattern: phase 1 borrows the receiver to collect all pending events into a `HashSet<PathBuf>` (deduplication) with source extension filtering; phase 2 borrows the callgraph and calls `invalidate_file()` for each path. The receiver borrow is dropped before the callgraph borrow — no RefCell conflicts.

## Verification

- `cargo test -- callgraph`: 19 unit + 13 integration = 32 tests pass
- `cargo test`: 194 unit + 132 integration = 326 total, 0 failures
- `bun test`: 39 pass, 0 failures
- `callgraph_watcher_add_caller` integration test: configure → callers → write new caller file → sleep(500ms) → ping (triggers drain) → callers → new caller appears
- `callgraph_watcher_remove_caller` integration test: configure → callers → rewrite file removing call → sleep(500ms) → ping (triggers drain) → callers → removed caller gone
- `callers` without configure returns `not_configured` error
- `callers` for symbol with no callers returns empty result with `total_callers: 0`
- No RefCell borrow panics in any test run
- Stderr observability confirmed: `[aft] watcher started:` and `[aft] invalidated N files` logs present

## Requirements Advanced

- R020 (Call graph construction) — file watcher invalidation now operational; lazy construction + watcher completes the requirement
- R022 (Reverse caller tree) — fully delivered with recursive depth expansion

## Requirements Validated

- R022 (Reverse caller tree) — integration tests prove cross-file callers grouped by file, recursive depth expansion, empty result handling, not_configured guard, symbol_not_found error
- R020 (Call graph construction) — lazy construction proven in S01, file watcher invalidation proven in S02 with modify-then-query and remove-then-query cycle tests

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

None — implemented as planned.

## Known Limitations

- Watcher integration tests use `thread::sleep(500ms)` for OS event delivery. FSEvents on macOS has non-deterministic timing — could theoretically be flaky on heavily loaded CI.
- Reverse index is fully cleared on any file invalidation (D092). For large codebases with frequent file changes, this could cause repeated full scans. Acceptable for now — optimize to delta updates if rebuild proves too slow.

## Follow-ups

- none

## Files Created/Modified

- `Cargo.toml` — added `notify = "8"` dependency
- `src/callgraph.rs` — CallerSite/CallerGroup/CallerEntry/CallersResult types, reverse_index field, build_reverse_index(), callers_of(), collect_callers_recursive(), invalidate_file(), 4 unit tests
- `src/commands/callers.rs` — new command handler with configure-then-use guard and symbol_not_found check
- `src/commands/mod.rs` — added `pub mod callers`
- `src/context.rs` — added watcher and watcher_rx RefCell fields + accessor methods
- `src/commands/configure.rs` — watcher creation, recursive watch, storage in AppContext on configure
- `src/main.rs` — `drain_watcher_events()` function + call before dispatch; `"callers"` dispatch entry; source extension filter constant
- `tests/integration/callgraph_test.rs` — 6 new tests (4 callers + 2 watcher cycle) + setup_watcher_fixture() helper
- `opencode-plugin-aft/src/tools/navigation.ts` — added `aft_callers` tool definition with Zod schema

## Forward Intelligence

### What the next slice should know
- Reverse index is a `HashMap<(PathBuf, String), Vec<CallerSite>>` keyed by canonical `(file, symbol_name)` tuples. `callers_of()` is the public API — it handles lazy build and recursive expansion.
- `invalidate_file()` clears the reverse index entirely (D092). S03's `trace_to` can rely on `callers_of()` returning fresh data after any file mutation.
- The drain-at-dispatch pattern (`drain_watcher_events()` before `dispatch()`) means any command sent after a file change will see updated results. No explicit "refresh" needed.

### What's fragile
- Path canonicalization in reverse index — if any new code path adds caller entries without canonicalizing, recursive lookups will silently miss results. The pattern is in `build_reverse_index()` where `CallerSite.file` gets `fs::canonicalize()`.
- Watcher test timing — 500ms sleep is generous but not guaranteed. If CI sees flaky callgraph_watcher tests, increase the sleep or add a retry.

### Authoritative diagnostics
- `cargo test -- callgraph` runs all 32 callgraph tests — the single command that proves the entire subsystem
- `[aft] invalidated N files` stderr log confirms drain is processing events — absence means events aren't arriving or extension filter is too strict

### What assumptions changed
- D075 assumed crossbeam-channel — D090 switched to std::sync::mpsc because notify v8 implements EventHandler for mpsc::Sender natively, avoiding an extra dependency
