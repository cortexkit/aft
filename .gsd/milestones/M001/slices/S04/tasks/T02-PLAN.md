---
estimated_steps: 5
estimated_files: 11
---

# T02: Command handlers, shared test helper, and integration tests

**Slice:** S04 — Safety & Recovery System
**Milestone:** M001

## Description

Wire the BackupStore and CheckpointStore into the binary's JSON protocol through 5 command handlers, extract the duplicated AftProcess test helper into a shared module, and write integration tests proving the full safety system works end-to-end through the binary.

## Steps

1. **Extract AftProcess into shared helper.** Create `tests/integration/helpers.rs` with `AftProcess` struct (spawn, send, send_silent, shutdown, stderr_output) and `fixture_path` function — copied from protocol_test.rs. Update `tests/integration/main.rs` to declare `mod helpers;`. Replace duplicated AftProcess in `protocol_test.rs` and `commands_test.rs` with `use super::helpers::{AftProcess, fixture_path};`. Verify existing tests still pass.

2. **Implement 5 command handlers.** Each in its own file under `src/commands/`, each following D021 pattern: `handle_*(req: &RawRequest, ctx: &AppContext) -> Response`. Register modules in `src/commands/mod.rs`.
   - `undo.rs`: extract `file` from params, call `ctx.backup().restore_latest(path)`, return restored content path + backup_id. Error: NoUndoHistory.
   - `edit_history.rs`: extract `file` from params, call `ctx.backup().history(path)`, return entries array with backup_id, timestamp, description.
   - `checkpoint.rs`: extract `name` and optional `files` array from params. If files provided, pass them. If not, use backup store's tracked_files. Call `ctx.checkpoint().create(name, files, &backup_store)`. Return name, file_count, created_at. Log to stderr.
   - `restore_checkpoint.rs`: extract `name`, call `ctx.checkpoint().restore(name)`, return restored file list. Log to stderr. Error: CheckpointNotFound.
   - `list_checkpoints.rs`: call `ctx.checkpoint().list()`, return array of checkpoint metadata.

3. **Wire command handlers into dispatch.** Add 5 match arms to `dispatch()` in main.rs: `"undo"`, `"edit_history"`, `"checkpoint"`, `"restore_checkpoint"`, `"list_checkpoints"`. Each calls the corresponding handler with `(&req, &ctx)`.

4. **Add a `snapshot` test command.** A thin command handler that calls `ctx.backup().snapshot(path, "manual snapshot")` and returns the backup_id. This is needed because S04 has no `write` or `edit_symbol` command — integration tests need a way to populate the backup store through the protocol. Mark it as test-only in the dispatch comment. This command also serves S05 verification needs.

5. **Write integration tests in `tests/integration/safety_test.rs`.** Add `mod safety_test;` to main.rs. Tests:
   - `test_checkpoint_create_restore_cycle`: create temp files, send `snapshot` for each (populates backup store + tracked files), send `checkpoint` with name, modify files externally (`std::fs::write`), send `restore_checkpoint`, verify files match original content via `std::fs::read_to_string`.
   - `test_undo_restores_previous_version`: write a temp file, send `snapshot`, overwrite file externally, send `undo`, verify file has original content.
   - `test_edit_history_returns_stack`: send multiple `snapshot` commands for the same file (modifying it between each), send `edit_history`, verify entries count and ordering (most recent first).
   - `test_list_checkpoints`: create 2 checkpoints with different names, send `list_checkpoints`, verify both appear with correct file counts.
   - `test_undo_no_history_error`: send `undo` for a file with no snapshots, verify error response with code `no_undo_history`.
   - `test_restore_nonexistent_checkpoint`: send `restore_checkpoint` with unknown name, verify error with code `checkpoint_not_found`.
   - `test_checkpoint_overwrite`: create checkpoint, create again with same name but different files, restore, verify second set of files.
   - Process should stay alive after every error — verify with a ping after each error case.

## Must-Haves

- [ ] 5 command handlers wired into dispatch and responding correctly
- [ ] AftProcess extracted to helpers.rs — no duplication across test files
- [ ] Integration test: checkpoint → modify → restore → files match original
- [ ] Integration test: undo restores previous file version
- [ ] Integration test: error paths return structured errors and don't crash binary
- [ ] Stderr logging for checkpoint create and restore operations
- [ ] `snapshot` command available for testing (populates backup store through protocol)

## Verification

- `cargo build` — 0 warnings
- `cargo test` — all tests pass
- `cargo test --test integration` — all integration tests pass, including new safety_test.rs
- `cargo test --test integration safety` — safety-specific tests pass in isolation

## Inputs

- `src/context.rs` — AppContext with backup() and checkpoint() accessors (from T01)
- `src/backup.rs` — BackupStore API (from T01)
- `src/checkpoint.rs` — CheckpointStore API (from T01)
- `src/error.rs` — CheckpointNotFound, NoUndoHistory variants (from T01)
- `src/main.rs` — dispatch function accepting `&AppContext` (from T01)
- `tests/integration/protocol_test.rs` — AftProcess to extract
- `tests/integration/commands_test.rs` — AftProcess duplicate to replace

## Observability Impact

- **New protocol commands**: `undo`, `edit_history`, `checkpoint`, `restore_checkpoint`, `list_checkpoints`, `snapshot` — all return structured JSON responses with `ok: true/false`
- **Stderr signals**: checkpoint create emits `[aft] checkpoint created: {name} ({n} files)`, checkpoint restore emits `[aft] checkpoint restored: {name}` — both already in CheckpointStore from T01, now reachable through the protocol
- **Error diagnostics**: `no_undo_history` (code + file path) and `checkpoint_not_found` (code + checkpoint name) propagated as structured error responses through protocol
- **Inspection**: `edit_history` returns per-file backup stack (backup_id, timestamp, description); `list_checkpoints` returns all checkpoint metadata (name, file_count, created_at)
- **Future agent verification**: send `list_checkpoints` or `edit_history` at any time to inspect safety system state without side effects

## Expected Output

- `src/commands/undo.rs` — undo command handler
- `src/commands/edit_history.rs` — edit_history command handler
- `src/commands/checkpoint.rs` — checkpoint command handler
- `src/commands/restore_checkpoint.rs` — restore_checkpoint command handler
- `src/commands/list_checkpoints.rs` — list_checkpoints command handler
- `src/commands/mod.rs` — 5 new module declarations
- `src/main.rs` — 6 new dispatch arms (5 commands + snapshot)
- `tests/integration/helpers.rs` — shared AftProcess + fixture_path
- `tests/integration/safety_test.rs` — 7 integration tests
- `tests/integration/main.rs` — helpers + safety_test modules added
- `tests/integration/protocol_test.rs` — imports from helpers (no more local AftProcess)
- `tests/integration/commands_test.rs` — imports from helpers (no more local AftProcess)
