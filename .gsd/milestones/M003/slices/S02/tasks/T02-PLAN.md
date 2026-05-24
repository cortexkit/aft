---
estimated_steps: 6
estimated_files: 5
---

# T02: File watcher integration + invalidation cycle tests

**Slice:** S02 — Reverse Callers + File Watcher
**Milestone:** M003

## Description

Integrate `notify` v8 file watcher into the binary to automatically invalidate call graph data when source files change on disk. The watcher runs on a background OS thread (notify's default), delivers events via `std::sync::mpsc`, and the main thread drains events before each `dispatch()` call. This preserves the single-threaded RefCell architecture (D001, D014, D029) — no concurrent access to AppContext stores.

Key constraint: the receiver and the callgraph must be in separate `RefCell`s to avoid borrow conflicts during drain → invalidate.

## Steps

1. Add `notify = "0.8"` to `Cargo.toml` dependencies
2. Add two new fields to `AppContext` in `context.rs`: `RefCell<Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>>` for the event receiver, and `RefCell<Option<notify::RecommendedWatcher>>` to keep the watcher alive. Add accessor methods `watcher_receiver()` and `set_watcher()`.
3. Extend `handle_configure()` in `configure.rs`: after creating the CallGraph, create `mpsc::channel()`, spawn `notify::recommended_watcher(tx)`, add `project_root` as a recursive watch path, store receiver and watcher in AppContext. If configure is called again (re-configure), drop old watcher/receiver first.
4. Add `drain_watcher_events(ctx: &AppContext)` function in `main.rs` before the `dispatch()` call: borrow the receiver, `try_recv()` in a loop collecting changed `PathBuf`s into a `HashSet`, filter to only supported source extensions (.ts/.tsx/.js/.jsx/.py/.rs/.go), then borrow_mut the callgraph and call `invalidate_file()` for each path. Log `[aft] invalidated N files` to stderr when N > 0.
5. Add integration test: configure with temp dir fixture → send `callers` for a symbol → use `std::fs::write` to modify a calling file (add a new function that calls the target) → send `ping` (triggers drain) → send `callers` again → assert the new caller appears in the response
6. Add integration test: configure with temp dir fixture → send `callers` → use `std::fs::write` to remove a call from a file → drain cycle → send `callers` → assert the removed caller is gone

## Must-Haves

- [ ] `notify` v8 dependency added
- [ ] Watcher receiver and handle stored in AppContext as separate RefCells
- [ ] Watcher created and started during `configure`, watching project_root recursively
- [ ] Drain function in main.rs before dispatch — deduplicates events by PathBuf, filters by source extension
- [ ] `invalidate_file` called for each changed source file during drain
- [ ] No RefCell borrow panics during drain → invalidate → dispatch cycle
- [ ] Integration test proving modify-file-then-query reflects changes
- [ ] Re-configure drops old watcher and creates new one

## Verification

- `cargo test -- callgraph` — all tests pass including new watcher cycle tests
- `cargo test` — all pass, 0 failures
- `bun test` — all pass (unchanged)
- Integration test: the modify-then-query cycle test passes reliably (with small sleep for OS event delivery)

## Observability Impact

- `[aft] watcher started: <path>` stderr log on configure
- `[aft] invalidated N files` stderr log when drain processes events (only when N > 0)
- Watcher initialization failure is logged but non-fatal — callers still works with stale data

## Inputs

- `src/callgraph.rs` — `CallGraph::invalidate_file()` from T01
- `src/context.rs` — `AppContext` struct with existing RefCell pattern
- `src/commands/configure.rs` — configure handler to extend
- `src/main.rs` — dispatch loop to add drain hook
- S02-RESEARCH.md — watcher design decisions (separate RefCells, mpsc not crossbeam, drain-at-dispatch)

## Expected Output

- `Cargo.toml` — `notify = "0.8"` added
- `src/context.rs` — two new RefCell fields for watcher components, accessor methods
- `src/commands/configure.rs` — watcher creation and storage during configure
- `src/main.rs` — `drain_watcher_events()` function, called before dispatch in loop
- `tests/integration/callgraph_test.rs` — 2+ new integration tests proving watcher invalidation cycle
