---
id: T02
parent: S04
milestone: M001
provides:
  - 5 command handlers (undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints) wired into dispatch
  - snapshot test command for populating backup store through protocol
  - Shared AftProcess test helper in helpers.rs (no more duplication)
  - 7 integration tests proving safety system end-to-end through the binary
key_files:
  - src/commands/undo.rs
  - src/commands/edit_history.rs
  - src/commands/checkpoint.rs
  - src/commands/restore_checkpoint.rs
  - src/commands/list_checkpoints.rs
  - src/main.rs
  - src/context.rs
  - tests/integration/helpers.rs
  - tests/integration/safety_test.rs
key_decisions:
  - "D029: AppContext stores wrapped in RefCell for interior mutability — handlers receive &AppContext but mutating commands need &mut store access"
patterns_established:
  - "RefCell borrow pattern: ctx.backup().borrow() for reads, ctx.backup().borrow_mut() for writes — consistent with D014 TreeSitterProvider pattern"
  - "edit_history returns entries most-recent-first (reversed from internal stack order) for agent convenience"
observability_surfaces:
  - "undo command returns { path, backup_id } on success, no_undo_history error with file path on failure"
  - "edit_history returns per-file backup stack with backup_id, timestamp, description — most recent first"
  - "list_checkpoints returns all checkpoint metadata (name, file_count, created_at)"
  - "checkpoint create/restore emit stderr signals: [aft] checkpoint created/restored: {name}"
  - "Error responses propagate structured codes (no_undo_history, checkpoint_not_found) through protocol"
duration: 18m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Command handlers, shared test helper, and integration tests

**Wired 5 safety commands + snapshot into the binary protocol, extracted shared test helper, and proved the full safety system works end-to-end with 7 integration tests.**

## What Happened

Extracted AftProcess struct and fixture_path function from protocol_test.rs into tests/integration/helpers.rs, updated both protocol_test.rs and commands_test.rs to import from the shared module. Built 5 command handlers in src/commands/ following the D021/D026 pattern — each extracts params from the flattened RawRequest, calls the appropriate store method, and returns structured JSON. Added a thin `snapshot` test command in main.rs dispatch for populating the backup store through the protocol. Refactored AppContext to wrap BackupStore and CheckpointStore in RefCell<T> (D029) since handlers receive `&AppContext` but mutating commands need `&mut` access to stores. Wrote 7 integration tests covering the full checkpoint cycle, undo round-trip, edit history stack, list checkpoints, overwrite behavior, and both error paths with process liveness verification after errors.

## Verification

- `cargo build` — 0 warnings
- `cargo test` — 86 unit tests + 19 integration tests pass (7 new)
- `cargo test --test integration` — all 19 integration tests pass
- `cargo test --test integration safety` — 7 safety-specific tests pass in isolation
- `cargo test backup` / `cargo test checkpoint` — unit tests for both stores pass

### Slice-level verification status (T02 is final — all must pass):
- ✅ `cargo test` — all tests pass (86 unit + 19 integration)
- ✅ `cargo test --test integration` — new safety_test.rs passes: checkpoint→modify→restore cycle, undo round-trip, edit_history stack, list_checkpoints, error paths
- ✅ `cargo test backup` / `cargo test checkpoint` — unit tests for both stores pass in isolation

## Diagnostics

- Send `list_checkpoints` or `edit_history` at any time to inspect safety system state through the protocol
- Error paths return structured JSON: `{ "ok": false, "code": "no_undo_history", "message": "..." }` — process stays alive after all errors
- Checkpoint create/restore emit stderr lines: `[aft] checkpoint created: {name} ({n} files)` / `[aft] checkpoint restored: {name}`

## Deviations

- Added RefCell wrapping to AppContext stores (D029) — not in the task plan but necessary because handlers receive `&AppContext` (immutable) while mutating commands need `&mut` access. This is the same pattern used for TreeSitterProvider (D014). Removed the `_mut()` accessors that were never used.

## Known Issues

None.

## Files Created/Modified

- `src/commands/undo.rs` — new: undo command handler
- `src/commands/edit_history.rs` — new: edit_history command handler
- `src/commands/checkpoint.rs` — new: checkpoint command handler
- `src/commands/restore_checkpoint.rs` — new: restore_checkpoint command handler
- `src/commands/list_checkpoints.rs` — new: list_checkpoints command handler
- `src/commands/mod.rs` — added 5 new module declarations
- `src/main.rs` — added 6 dispatch arms (5 commands + snapshot) and handle_snapshot function
- `src/context.rs` — wrapped stores in RefCell, updated accessors to return &RefCell<T>
- `tests/integration/helpers.rs` — new: shared AftProcess + fixture_path
- `tests/integration/safety_test.rs` — new: 7 integration tests
- `tests/integration/main.rs` — added helpers + safety_test module declarations
- `tests/integration/protocol_test.rs` — replaced local AftProcess with import from helpers
- `tests/integration/commands_test.rs` — replaced local AftProcess/fixture_path with imports from helpers
