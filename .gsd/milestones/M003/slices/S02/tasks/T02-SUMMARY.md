---
id: T02
parent: S02
milestone: M003
provides:
  - notify v8 file watcher integrated into binary, initialized during configure
  - drain-at-dispatch pattern in main.rs — deduplicates and filters events before invalidating callgraph
  - Two integration tests proving modify-then-query and remove-then-query cycles
key_files:
  - Cargo.toml
  - src/context.rs
  - src/commands/configure.rs
  - src/main.rs
  - tests/integration/callgraph_test.rs
key_decisions:
  - D090 (std::sync::mpsc for watcher channel — already recorded)
  - D091 (separate RefCells for watcher receiver and callgraph — already recorded)
patterns_established:
  - Watcher receiver and handle stored as separate RefCells in AppContext to avoid borrow conflicts during drain → invalidate
  - drain_watcher_events() uses two-phase pattern: phase 1 borrows receiver to collect paths, phase 2 borrows callgraph to invalidate — no overlapping borrows
  - Source extension filtering (ts/tsx/js/jsx/py/rs/go) applied during drain, not at OS watcher level
observability_surfaces:
  - "[aft] watcher started: <path>" stderr log on successful configure
  - "[aft] watcher watch error: <err>" stderr log if watch fails (non-fatal)
  - "[aft] watcher init failed: <err>" stderr log if watcher creation fails (non-fatal)
  - "[aft] invalidated N files" stderr log when drain processes >0 changed source files
duration: 20m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: File watcher integration + invalidation cycle tests

**Integrated `notify` v8 file watcher into the binary with drain-at-dispatch invalidation and two integration tests proving the modify-then-query cycle.**

## What Happened

Added `notify = "8"` to Cargo.toml. Extended `AppContext` with two new `RefCell<Option<_>>` fields: one for the `mpsc::Receiver` and one for the `RecommendedWatcher` handle. Both follow the existing RefCell pattern and start as `None`.

Extended `handle_configure` to create an `mpsc::channel()`, spawn `notify::recommended_watcher(tx)`, watch `project_root` recursively, and store both receiver and watcher in AppContext. Re-configure drops old watcher/receiver before creating new ones. Watcher initialization failure is logged but non-fatal.

Added `drain_watcher_events()` in `main.rs`, called before `dispatch()` on every request. Uses a two-phase pattern: phase 1 borrows the receiver to `try_recv()` all pending events into a `HashSet<PathBuf>` (deduplication), filtering to supported source extensions; phase 2 borrows the callgraph mutably and calls `invalidate_file()` for each path. The receiver borrow is dropped before the callgraph borrow — no RefCell conflicts.

Two integration tests added:
- `callgraph_watcher_add_caller`: configure → callers → write new file with additional caller → sleep(500ms) → ping (triggers drain) → callers → assert new caller appears
- `callgraph_watcher_remove_caller`: configure → callers → rewrite file to remove call → sleep(500ms) → ping (triggers drain) → callers → assert removed caller gone

## Verification

- `cargo test -- callgraph`: 19 unit + 13 integration = 32 tests pass (including 2 new watcher cycle tests)
- `cargo test`: 194 unit + 132 integration = 326 tests pass, 0 failures
- `bun test`: 39 pass, 0 failures
- Stderr observability: `[aft] watcher started: /tmp` confirmed via manual binary run
- No RefCell borrow panics in any test run

### Slice-level verification status

- ✅ `cargo test -- callgraph` — all 32 tests pass (was 22 at slice start, now 32)
- ✅ `cargo test` — 326 tests pass, 0 failures
- ✅ `bun test` — 39 pass
- ✅ Integration test: modify-then-query cycle reflects changes
- ✅ Integration test: remove-then-query cycle reflects changes
- ✅ `callers` without configure returns `not_configured` error
- ✅ `callers` for symbol with no callers returns empty result with `total_callers: 0`

All slice verification checks pass. S02 is complete.

## Diagnostics

- `[aft] watcher started: <path>` — confirms watcher is active after configure
- `[aft] invalidated N files` — confirms drain processed file change events (only when N > 0)
- Watcher init/watch failures logged to stderr but non-fatal — callers still works with stale data
- The two-phase drain pattern (collect paths, then invalidate) avoids RefCell borrow conflicts — if a panic occurs during drain, it would indicate the phases are accidentally overlapping

## Deviations

None.

## Known Issues

- Integration tests use `thread::sleep(500ms)` for OS event delivery. This is inherent to file watcher testing — FSEvents on macOS has non-deterministic delivery timing. The 500ms value is generous but could theoretically be flaky on heavily loaded CI.

## Files Created/Modified

- `Cargo.toml` — added `notify = "8"` dependency
- `src/context.rs` — added `watcher` and `watcher_rx` RefCell fields + accessor methods
- `src/commands/configure.rs` — watcher creation, recursive watch, storage in AppContext on configure
- `src/main.rs` — `drain_watcher_events()` function + call before dispatch; source extension filter constant
- `tests/integration/callgraph_test.rs` — 2 new watcher invalidation cycle tests + `setup_watcher_fixture()` helper
