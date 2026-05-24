# S04: Safety & Recovery System

**Goal:** Per-file undo and workspace-wide checkpoints work through the binary's JSON protocol — agents can snapshot state, make changes, and roll back.
**Demo:** Send checkpoint → write files → restore_checkpoint → verify files match original content. Send undo/edit_history and see the backup stack.

## Must-Haves

- AppContext struct threads BackupStore, CheckpointStore, LanguageProvider, and Config through dispatch — existing outline/zoom commands unchanged in behavior
- BackupStore: snapshot a file (stores full content), restore latest, list history per file, path canonicalization for key consistency
- CheckpointStore: create named snapshot of explicit file list (or all tracked), restore by name (overwrites files), list checkpoints with metadata, TTL-based cleanup
- All storage under `.aft/` directory (lazily created on first write), paths encoded for safe filesystem use
- Five command handlers: `undo`, `edit_history`, `checkpoint`, `restore_checkpoint`, `list_checkpoints`
- Checkpoint name collision → overwrite (agent-friendly)
- Clear errors for: empty undo stack, nonexistent checkpoint, file not found, disk write failures
- Integration tests prove: checkpoint → modify → restore → files match, undo round-trip, error paths

## Proof Level

- This slice proves: operational (stores persist and round-trip correctly through the binary protocol)
- Real runtime required: yes (integration tests spawn the binary)
- Human/UAT required: no

## Verification

- `cargo test` — all existing tests pass (outline/zoom unit tests updated for AppContext, integration tests unchanged in behavior)
- `cargo test --test integration` — new safety_test.rs passes: checkpoint→modify→restore cycle, undo round-trip, edit_history stack, list_checkpoints, error paths (empty undo, missing checkpoint)
- `cargo test backup checkpoint` — unit tests for both stores pass in isolation

## Observability / Diagnostics

- Runtime signals: `[aft] checkpoint created: {name} ({n} files)` and `[aft] checkpoint restored: {name}` on stderr
- Inspection surfaces: `list_checkpoints` command returns checkpoint names, file counts, and creation timestamps; `edit_history` returns per-file backup stack
- Failure visibility: structured error responses with codes `checkpoint_not_found`, `no_undo_history`, `file_not_found` — each includes the name/path that failed

## Integration Closure

- Upstream surfaces consumed: `src/protocol.rs` (RawRequest, Response), `src/config.rs` (Config.checkpoint_ttl_hours), `src/language.rs` (LanguageProvider trait), `src/error.rs` (AftError enum)
- New wiring introduced in this slice: `AppContext` struct replaces `&dyn LanguageProvider` in dispatch — single change point for all future state needs (S05 uses this to access BackupStore for auto-snapshot)
- What remains before the milestone is truly usable end-to-end: S05 (editing engine that calls BackupStore.snapshot before mutations), S06 (plugin bridge), S07 (distribution)

## Tasks

- [x] **T01: AppContext, BackupStore, and CheckpointStore with unit tests** `est:1h30m`
  - Why: The stores are the core deliverable of this slice — everything else is wiring. AppContext must exist first because stores live in it and dispatch needs to pass it to handlers.
  - Files: `src/context.rs`, `src/backup.rs`, `src/checkpoint.rs`, `src/lib.rs`, `src/main.rs`, `src/error.rs`, `src/commands/outline.rs`, `src/commands/zoom.rs`
  - Do: Create AppContext holding `&dyn LanguageProvider`, `BackupStore`, `CheckpointStore`, `Config`. Refactor dispatch to pass `&AppContext`. Update outline/zoom handler signatures (`&dyn LanguageProvider` → `&AppContext`, extract provider from context). Update their 19 unit tests. Add new AftError variants (CheckpointNotFound, NoUndoHistory). Build BackupStore with snapshot/restore/history using canonicalized paths, monotonic backup IDs, in-memory HashMap + .aft/backups/ disk persistence. Build CheckpointStore with create/restore/list/cleanup using .aft/checkpoints/{name}/ dirs, file content copies, TTL from Config. Path encoding: replace `/` with `__` and `\` with `__` for filesystem-safe directory names. Unit tests for both stores covering: snapshot + restore round-trip, multiple snapshots with ordering, empty history error, checkpoint create + restore + overwrite, TTL cleanup, missing checkpoint error.
  - Verify: `cargo test` — all existing tests pass, new store unit tests pass
  - Done when: BackupStore and CheckpointStore have tested APIs ready for command handlers to call, dispatch passes AppContext, outline/zoom behavior unchanged

- [x] **T02: Command handlers, shared test helper, and integration tests** `est:1h`
  - Why: Wires stores into the protocol and proves the full round-trip — this is where the slice becomes demoable.
  - Files: `src/commands/mod.rs`, `src/commands/undo.rs`, `src/commands/edit_history.rs`, `src/commands/checkpoint.rs`, `src/commands/restore_checkpoint.rs`, `src/commands/list_checkpoints.rs`, `src/main.rs`, `tests/integration/helpers.rs`, `tests/integration/main.rs`, `tests/integration/safety_test.rs`, `tests/integration/protocol_test.rs`, `tests/integration/commands_test.rs`
  - Do: Implement 5 command handlers following D021 pattern (each takes `&RawRequest, &AppContext` → Response). Wire all 5 into dispatch match arms. Extract AftProcess struct from protocol_test.rs and commands_test.rs into tests/integration/helpers.rs — both existing test files import from helpers. Add fixture_path to helpers too. Write safety_test.rs integration tests: (1) checkpoint → write temp files → restore → verify files restored, (2) undo: write file via binary protocol (using a "write" workaround or direct fs + snapshot command) → undo → verify, (3) edit_history returns stack, (4) list_checkpoints, (5) error: undo with no history, (6) error: restore nonexistent checkpoint. Stderr logging for checkpoint create/restore. Note: since S04 doesn't have a `write` command yet (that's S05), integration tests for undo will use checkpoint+restore and the backup store's snapshot method indirectly — the `undo` command is tested by first calling `checkpoint` (which snapshots files), then modifying files externally, then calling `undo` on individual files. Alternatively, add a thin `snapshot` command for testing that just calls BackupStore.snapshot.
  - Verify: `cargo test --test integration` — all tests pass including new safety_test.rs; `cargo build` — 0 warnings
  - Done when: All 5 commands respond correctly through the binary protocol, checkpoint→restore cycle proven by integration test, AftProcess extracted into shared helper with no duplication

## Files Likely Touched

- `src/context.rs` (new — AppContext struct)
- `src/backup.rs` (new — BackupStore)
- `src/checkpoint.rs` (new — CheckpointStore)
- `src/error.rs` (new variants)
- `src/main.rs` (dispatch refactor)
- `src/lib.rs` (module exports)
- `src/commands/mod.rs` (new command modules)
- `src/commands/outline.rs` (signature + test updates)
- `src/commands/zoom.rs` (signature + test updates)
- `src/commands/undo.rs` (new)
- `src/commands/edit_history.rs` (new)
- `src/commands/checkpoint.rs` (new)
- `src/commands/restore_checkpoint.rs` (new)
- `src/commands/list_checkpoints.rs` (new)
- `tests/integration/helpers.rs` (new — shared AftProcess)
- `tests/integration/safety_test.rs` (new)
- `tests/integration/main.rs` (add new modules)
- `tests/integration/protocol_test.rs` (import from helpers)
- `tests/integration/commands_test.rs` (import from helpers)
