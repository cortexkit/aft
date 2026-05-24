---
id: S04
parent: M001
milestone: M001
provides:
  - AppContext struct threading BackupStore, CheckpointStore, LanguageProvider, and Config through dispatch
  - BackupStore with per-file undo — snapshot/restore_latest/history with canonicalized path keys and monotonic backup IDs
  - CheckpointStore with named workspace snapshots — create/restore/list/cleanup with TTL-based expiry
  - 5 command handlers wired into dispatch — undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints
  - snapshot test command for populating backup store through protocol (used by integration tests, available for S05)
  - Shared AftProcess test helper eliminating duplication across integration test files
  - AftError variants CheckpointNotFound and NoUndoHistory with structured error codes
requires:
  - slice: S01
    provides: Protocol types (RawRequest, Response), dispatch loop, Config, AftError enum, LanguageProvider trait
affects:
  - S05
key_files:
  - src/context.rs
  - src/backup.rs
  - src/checkpoint.rs
  - src/commands/undo.rs
  - src/commands/edit_history.rs
  - src/commands/checkpoint.rs
  - src/commands/restore_checkpoint.rs
  - src/commands/list_checkpoints.rs
  - src/error.rs
  - src/main.rs
  - tests/integration/helpers.rs
  - tests/integration/safety_test.rs
key_decisions:
  - "D025: AppContext struct as single dispatch parameter — replaces growing parameter list, handlers extract what they need"
  - "D026: Handler signature (&RawRequest, &AppContext) → Response — supersedes D021's &dyn LanguageProvider"
  - "D027: snapshot test command exposed in dispatch for integration testing — S04 has no mutation commands"
  - "D028: Checkpoint name collision → overwrite (agent-friendly, no pre-check needed)"
  - "D029: AppContext stores wrapped in RefCell for interior mutability — handlers receive &AppContext but mutating commands need &mut store access"
patterns_established:
  - "AppContext constructed once in main, passed as &AppContext to dispatch and all handlers"
  - "RefCell borrow pattern: ctx.backup().borrow() for reads, ctx.backup().borrow_mut() for writes"
  - "edit_history returns entries most-recent-first for agent convenience"
  - "Shared AftProcess test helper in tests/integration/helpers.rs — all integration test files import from here"
observability_surfaces:
  - "list_checkpoints command returns checkpoint names, file counts, and creation timestamps"
  - "edit_history command returns per-file backup stack with backup_id, timestamp, description — most recent first"
  - "Structured error codes: checkpoint_not_found, no_undo_history, file_not_found — each includes the failing name/path"
  - "Stderr signals: [aft] checkpoint created/restored: {name} ({n} files)"
drill_down_paths:
  - .gsd/milestones/M001/slices/S04/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S04/tasks/T02-SUMMARY.md
duration: 43m
verification_result: passed
completed_at: 2026-03-14
---

# S04: Safety & Recovery System

**Per-file undo and workspace-wide checkpoints wired through the binary's JSON protocol — agents can snapshot, modify, and roll back files.**

## What Happened

**T01** built the foundation: AppContext struct threading shared state (LanguageProvider, BackupStore, CheckpointStore, Config) through dispatch, replacing the previous `&dyn LanguageProvider` parameter. BackupStore stores file contents in-memory keyed by canonicalized paths with monotonic backup IDs. CheckpointStore creates named workspace snapshots by reading file contents at checkpoint time, and restores by overwriting files. Both stores have complete unit test suites (6 tests each). Refactored outline and zoom handler signatures to accept `&AppContext` — all existing tests updated and passing.

**T02** wired five command handlers (undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints) plus a thin `snapshot` test command into dispatch. AppContext stores were wrapped in `RefCell<T>` (D029) since handlers receive `&AppContext` but mutating commands need `&mut` access. Extracted shared `AftProcess` test helper into `tests/integration/helpers.rs`, eliminating duplication across test files. Wrote 7 integration tests proving: checkpoint→modify→restore cycle, undo round-trip, edit_history stack ordering, list_checkpoints metadata, checkpoint overwrite, and both error paths (empty undo stack, missing checkpoint) with process liveness after errors.

## Verification

- `cargo build` — 0 warnings
- `cargo test` — 86 unit + 19 integration = 105 tests, 0 failures
- `cargo test --test integration` — all 19 pass, including 7 safety-specific tests
- `cargo test backup` / `cargo test checkpoint` — store unit tests pass in isolation
- All 5 commands respond correctly through binary protocol
- Checkpoint→modify→restore cycle proven: files match original content after restore
- Undo round-trip proven: file restored to pre-snapshot state
- Error paths return structured JSON with codes, process stays alive after errors

## Requirements Advanced

- R007 (Per-file auto-backup and undo) — BackupStore and undo command implemented; auto-snapshot on mutation deferred to S05 where edit commands will call `BackupStore.snapshot()` before every write
- R008 (Workspace-wide checkpoints) — CheckpointStore with create/restore/list/cleanup and all 5 commands fully wired through protocol

## Requirements Validated

None — R007 depends on S05 wiring auto-snapshot into edit commands. R008 is functionally complete but full validation deferred to end-to-end proof with S05's mutation commands.

## New Requirements Surfaced

None.

## Requirements Invalidated or Re-scoped

None.

## Deviations

- AppContext stores wrapped in RefCell (D029) — not in the original plan but necessary because handlers receive `&AppContext` (immutable) while mutating commands need `&mut` access to stores. Same pattern as D014 (TreeSitterProvider).
- Outline tests didn't need updating — plan estimated 7 outline test updates but they test internal functions (build_outline_tree/symbol_to_entry), not handle_outline, so 0 changes needed.
- Disk persistence deferred — both stores are in-memory only per D001 (persistent process assumption). The `.aft/` directory structure is designed but not yet used.

## Known Limitations

- Stores are in-memory only — data lost on binary restart (intentional per D001, persistent process stays alive)
- BackupStore.snapshot requires the file to exist on disk (no virtual file support)
- CheckpointStore.create with empty file list uses BackupStore's tracked files — only files previously snapshotted are included
- No automatic TTL cleanup trigger — cleanup must be called explicitly (no background timer)
- R007 auto-snapshot on mutation is S05 scope — undo only works for files explicitly snapshotted via the `snapshot` command until then

## Follow-ups

- S05 must call `BackupStore.snapshot()` before every mutation in edit_symbol/edit_match/write/batch
- `snapshot` test command (D027) may be removable after S05 adds implicit snapshots — evaluate then

## Files Created/Modified

- `src/context.rs` — new: AppContext struct with provider/backup/checkpoint/config accessors
- `src/backup.rs` — new: BackupStore with snapshot/restore_latest/history/tracked_files + 6 unit tests
- `src/checkpoint.rs` — new: CheckpointStore with create/restore/list/cleanup + 6 unit tests
- `src/error.rs` — added CheckpointNotFound and NoUndoHistory variants with Display + error codes
- `src/main.rs` — refactored dispatch to AppContext, added 6 dispatch arms (5 commands + snapshot)
- `src/lib.rs` — added pub mod backup, checkpoint, context + 2 error variant tests
- `src/commands/outline.rs` — handle_outline signature: &dyn LanguageProvider → &AppContext
- `src/commands/zoom.rs` — handle_zoom signature + all 12 unit tests updated
- `src/commands/mod.rs` — added 5 new module declarations
- `src/commands/undo.rs` — new: undo command handler
- `src/commands/edit_history.rs` — new: edit_history command handler
- `src/commands/checkpoint.rs` — new: checkpoint command handler
- `src/commands/restore_checkpoint.rs` — new: restore_checkpoint command handler
- `src/commands/list_checkpoints.rs` — new: list_checkpoints command handler
- `tests/integration/helpers.rs` — new: shared AftProcess + fixture_path
- `tests/integration/safety_test.rs` — new: 7 integration tests
- `tests/integration/main.rs` — added helpers + safety_test module declarations
- `tests/integration/protocol_test.rs` — replaced local AftProcess with import from helpers
- `tests/integration/commands_test.rs` — replaced local AftProcess/fixture_path with imports

## Forward Intelligence

### What the next slice should know
- AppContext is the single state container — add new stores/state as fields with RefCell wrapping and accessor methods
- Handler signature is `(&RawRequest, &AppContext) -> Response` — follow D026 pattern
- Call `ctx.backup().borrow_mut().snapshot(&path, "description")` before any file mutation to enable undo
- The `snapshot` test command exists in dispatch for testing — can be used by S05 integration tests too

### What's fragile
- RefCell borrows must not overlap — `borrow()` and `borrow_mut()` on the same store in the same scope will panic at runtime. Each handler should do one borrow, use it, drop it before doing another.
- BackupStore uses `std::fs::canonicalize` which follows symlinks — symlinked files may have unexpected canonical paths

### Authoritative diagnostics
- `list_checkpoints` and `edit_history` through the binary protocol — these are the definitive views of safety system state
- Structured error codes (`checkpoint_not_found`, `no_undo_history`) in JSON responses — parse the `code` field for programmatic handling

### What assumptions changed
- Plan assumed outline tests needed updating (7 estimated) — they test internal functions directly, not handler signatures, so 0 changes needed
- Plan assumed `_mut()` accessor pattern for stores — RefCell wrapping (D029) was cleaner and more consistent with existing TreeSitterProvider pattern (D014)
