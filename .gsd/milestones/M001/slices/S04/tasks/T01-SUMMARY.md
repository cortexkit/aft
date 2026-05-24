---
id: T01
parent: S04
milestone: M001
provides:
  - AppContext struct threading shared state through dispatch
  - BackupStore with per-file undo (snapshot/restore/history)
  - CheckpointStore with workspace snapshots (create/restore/list/cleanup)
  - AftError variants CheckpointNotFound and NoUndoHistory
key_files:
  - src/context.rs
  - src/backup.rs
  - src/checkpoint.rs
  - src/error.rs
  - src/main.rs
  - src/commands/outline.rs
  - src/commands/zoom.rs
  - src/lib.rs
key_decisions:
  - In-memory stores only for now — disk persistence under .aft/ deferred per plan (D001 persistent process assumption)
  - BackupStore uses std::fs::canonicalize with fallback to raw path for key consistency
  - CheckpointStore.create takes &BackupStore reference to resolve "all tracked files" when file list is empty
  - Monotonic counter (AtomicU64) for backup IDs — simple, deterministic ordering
patterns_established:
  - AppContext constructed once in main, passed as &AppContext to dispatch and all handlers
  - Handlers extract provider via ctx.provider() — clean accessor pattern for adding future state
  - Store mutability: stores are fields on AppContext with _mut() accessors for write operations
observability_surfaces:
  - AftError::CheckpointNotFound — structured code "checkpoint_not_found" with checkpoint name in message
  - AftError::NoUndoHistory — structured code "no_undo_history" with file path in message
  - stderr logging on checkpoint create/restore via eprintln! in CheckpointStore
duration: 25m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: AppContext, BackupStore, and CheckpointStore with unit tests

**Built the foundation stores and refactored dispatch to thread AppContext through all handlers.**

## What Happened

Added two new AftError variants (CheckpointNotFound, NoUndoHistory) with Display impls and error codes. Built BackupStore in src/backup.rs — in-memory HashMap keyed by canonicalized path, each value a Vec<BackupEntry> stack. Methods: snapshot (reads file, pushes entry with monotonic ID), restore_latest (pops and writes file), history (returns ordered stack), tracked_files (lists all paths with entries). Built CheckpointStore in src/checkpoint.rs — in-memory HashMap keyed by checkpoint name. Methods: create (reads listed files or all tracked from BackupStore), restore (overwrites files), list (returns metadata), cleanup (TTL-based removal). Created AppContext in src/context.rs holding provider, both stores, and config with accessor methods. Refactored main.rs dispatch from `&dyn LanguageProvider` to `&AppContext`. Updated outline.rs and zoom.rs handler signatures — outline tests didn't need changes (they test internal tree building), zoom tests all updated to construct AppContext via make_ctx() helper.

## Verification

- `cargo test` — 86 unit tests pass, 0 warnings (up from ~73, +13 new tests)
- `cargo test --test integration` — all 12 existing integration tests pass (dispatch signature change is internal, protocol unchanged)
- `cargo test "backup::tests"` — 6 backup store tests pass in isolation
- `cargo test "checkpoint::tests"` — 6 checkpoint store tests pass in isolation
- New error variant tests confirm Display output and error codes

### Slice-level verification status (T01 is intermediate — partial expected):
- ✅ `cargo test` — all existing tests pass, outline/zoom updated for AppContext
- ⬜ `cargo test --test integration` — safety_test.rs not yet created (T02 scope)
- ✅ `cargo test backup/checkpoint` — unit tests for both stores pass in isolation

## Diagnostics

- BackupStore.history(path) returns ordered stack with timestamps and descriptions for any file
- CheckpointStore.list() returns all checkpoints with names, file counts, and creation timestamps
- Error responses include structured codes: `checkpoint_not_found` and `no_undo_history` with the failing name/path
- Checkpoint create/restore emit `[aft] checkpoint created: {name} ({n} files)` and `[aft] checkpoint restored: {name}` on stderr

## Deviations

- Outline tests didn't need updating — they test build_outline_tree/symbol_to_entry directly, not handle_outline. Plan estimated 7 outline test updates but 0 were needed.
- Disk persistence for both stores deferred as planned (in-memory primary, D001 persistent process).

## Known Issues

None.

## Files Created/Modified

- `src/context.rs` — new: AppContext struct with provider/backup/checkpoint/config accessors
- `src/backup.rs` — new: BackupStore with 6 unit tests
- `src/checkpoint.rs` — new: CheckpointStore with 6 unit tests
- `src/error.rs` — added CheckpointNotFound and NoUndoHistory variants
- `src/main.rs` — refactored dispatch to use AppContext instead of &dyn LanguageProvider
- `src/commands/outline.rs` — updated handle_outline signature to &AppContext
- `src/commands/zoom.rs` — updated handle_zoom signature and all 12 unit tests to use AppContext
- `src/lib.rs` — added pub mod backup, checkpoint, context; added 2 error variant tests
