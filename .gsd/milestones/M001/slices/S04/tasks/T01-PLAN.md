---
estimated_steps: 8
estimated_files: 8
---

# T01: AppContext, BackupStore, and CheckpointStore with unit tests

**Slice:** S04 — Safety & Recovery System
**Milestone:** M001

## Description

Create the AppContext struct that threads all shared state through dispatch, build BackupStore (per-file undo) and CheckpointStore (workspace snapshots) with full unit test coverage, and refactor existing handler signatures to use AppContext. This is the foundation task — T02 builds command handlers on top of these stores.

## Steps

1. **Add new AftError variants** in `src/error.rs`: `CheckpointNotFound { name }`, `NoUndoHistory { path }`. Add Display impls and codes (`checkpoint_not_found`, `no_undo_history`). Add unit tests for the new variants in `src/lib.rs`.

2. **Create `src/backup.rs` — BackupStore.** In-memory HashMap keyed by canonical path, each value a `Vec<BackupEntry>` (backup_id, content, timestamp, description). Methods: `snapshot(path, description) -> Result<String>` (reads file, pushes entry, returns backup_id), `restore_latest(path) -> Result<BackupEntry>` (pops and writes file), `history(path) -> Vec<BackupEntry>`, `tracked_files() -> Vec<PathBuf>`. Backup IDs use a monotonic counter (`backup-{n}`). Path canonicalization via `std::fs::canonicalize` with fallback to cleaned relative path for new files. Disk persistence under `.aft/backups/` deferred — in-memory is primary (D001: persistent process). Add unit tests: snapshot+restore round-trip, multiple snapshots preserve order, restore pops from stack, empty history returns empty vec, snapshot of nonexistent file returns FileNotFound.

3. **Create `src/checkpoint.rs` — CheckpointStore.** In-memory HashMap of checkpoints keyed by name. Each checkpoint: name, file_contents (HashMap<PathBuf, String>), created_at timestamp, file_count. Methods: `create(name, files: Vec<PathBuf>) -> Result<CheckpointInfo>` (reads each file, stores content — if `files` is empty, snapshots all tracked files from BackupStore), `restore(name) -> Result<CheckpointInfo>` (overwrites files with stored content), `list() -> Vec<CheckpointInfo>`, `cleanup(ttl_hours)` (removes expired checkpoints). Disk persistence under `.aft/checkpoints/{name}/` — write on create, read on startup. Overwrite on name collision. Add unit tests: create+restore round-trip, overwrite existing name, list returns metadata, cleanup removes expired, restore nonexistent returns CheckpointNotFound.

4. **Create `src/context.rs` — AppContext.** Struct holding: `provider: Box<dyn LanguageProvider>`, `backup: BackupStore`, `checkpoint: CheckpointStore`, `config: Config`. Constructor `AppContext::new(provider, config)` initializes empty stores.

5. **Refactor `src/main.rs` dispatch.** Replace `let provider = TreeSitterProvider::new()` with `let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())`. Change `dispatch(req, &provider)` to `dispatch(req, &ctx)`. Update dispatch signature: `fn dispatch(req: RawRequest, ctx: &AppContext) -> Response`. Outline/zoom arms pass `ctx` instead of `provider`.

6. **Update `src/commands/outline.rs`.** Change `handle_outline(req: &RawRequest, provider: &dyn LanguageProvider)` to `handle_outline(req: &RawRequest, ctx: &AppContext)`. Extract provider as `ctx.provider()` (add accessor to AppContext). Update all 7 unit tests — construct a test AppContext with StubProvider or TreeSitterProvider as needed, pass it instead of provider directly.

7. **Update `src/commands/zoom.rs`.** Same signature change. Update all 12 unit tests — same pattern as outline.

8. **Register new modules in `src/lib.rs`.** Add `pub mod backup;`, `pub mod checkpoint;`, `pub mod context;`.

## Must-Haves

- [ ] AppContext struct holds provider, backup store, checkpoint store, and config
- [ ] dispatch and all existing handlers accept `&AppContext` instead of `&dyn LanguageProvider`
- [ ] All 19 existing unit tests in outline.rs and zoom.rs pass with updated signatures
- [ ] BackupStore: snapshot, restore_latest, history, tracked_files — all with unit tests
- [ ] CheckpointStore: create, restore, list, cleanup — all with unit tests
- [ ] New AftError variants: CheckpointNotFound, NoUndoHistory
- [ ] Path canonicalization in BackupStore for key consistency

## Verification

- `cargo test` — all tests pass (existing + new), 0 warnings
- `cargo test --test integration` — existing integration tests still pass (dispatch signature change is internal, protocol unchanged)
- Unit test count increases by ~10-15 (store tests + error variant tests)

## Observability Impact

- Signals added: AftError::CheckpointNotFound and AftError::NoUndoHistory — structured error codes for diagnostic clarity
- How a future agent inspects this: error responses include `code` and `message` fields distinguishing "no undo history for path X" from "checkpoint Y not found"
- Failure state exposed: BackupStore.history returns ordered stack with timestamps; CheckpointStore.list returns all checkpoints with creation times and file counts

## Inputs

- `src/main.rs` — current dispatch pattern with `&dyn LanguageProvider`
- `src/commands/outline.rs` — handler + 7 unit tests to update
- `src/commands/zoom.rs` — handler + 12 unit tests to update
- `src/config.rs` — Config struct with checkpoint_ttl_hours consumed by CheckpointStore
- `src/error.rs` — AftError enum to extend
- S01 summary — RawRequest.params extraction pattern, Response constructors

## Expected Output

- `src/context.rs` — AppContext struct with provider accessor, store accessors
- `src/backup.rs` — BackupStore with ~5 unit tests
- `src/checkpoint.rs` — CheckpointStore with ~5 unit tests
- `src/error.rs` — two new variants with Display/code impls
- `src/main.rs` — refactored dispatch using AppContext
- `src/commands/outline.rs` — updated signature, 7 unit tests still pass
- `src/commands/zoom.rs` — updated signature, 12 unit tests still pass
- `src/lib.rs` — three new module exports
