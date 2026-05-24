# S04: Safety & Recovery System — Research

**Date:** 2026-03-14

## Summary

S04 introduces the safety infrastructure: per-file backup/undo (R007) and workspace-wide checkpoints (R008). These are two separate stores — BackupStore for per-file undo stacks, CheckpointStore for named workspace snapshots — plus five command handlers (undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints).

The main design challenge is threading stateful stores through dispatch. Current command handlers take `(&RawRequest, &dyn LanguageProvider)`. S04 commands don't need LanguageProvider at all — they need mutable access to BackupStore and CheckpointStore. S05 needs _both_ (provider for symbol resolution + backup store for auto-snapshot before edits). The cleanest solution is an `AppContext` struct that holds all shared state and gets passed to dispatch. This avoids growing the parameter list with each slice and gives S05 a natural place to grab both deps.

The stores themselves are straightforward: in-memory HashMap-based structures with file I/O for persistence. The binary is single-threaded (D014), so RefCell works. Checkpoints use simple file copies (context says "simple file copies sufficient for M001"). Backup entries store full file content — no delta compression needed at this scale.

## Recommendation

**Create an `AppContext` struct** in `src/context.rs` that holds `LanguageProvider`, `BackupStore`, `CheckpointStore`, and `Config`. Pass `&AppContext` to `dispatch()` instead of `&dyn LanguageProvider`. This is a small refactor of main.rs and the two existing command handler signatures (outline, zoom). It pays off immediately in S05 which needs both provider and backup store, and scales to S06 without further signature churn.

**BackupStore** should be in-memory first, with `.aft/backups/` flush as a secondary concern. The primary use case is within-session undo — the binary is persistent (D001), so in-memory is always fast. Disk persistence ensures undo stacks survive binary restarts (crash recovery via plugin auto-restart). Use SHA256 of `(file_path, timestamp)` for backup IDs — deterministic and collision-free.

**CheckpointStore** snapshots should store full file copies under `.aft/checkpoints/{name}/`. Each checkpoint records which files were included and their content at snapshot time. "Tracked files" = files the binary has operated on (files present in BackupStore's key set), plus any explicitly listed in the checkpoint request. Auto-cleanup via TTL from Config.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|-------------------|------------|
| UUID generation for backup IDs | SHA256 hash of path+timestamp | Avoids adding `uuid` crate — deterministic from inputs, collision-free for our use case |
| Timestamp formatting | `std::time::SystemTime` + `UNIX_EPOCH` | Already used in parser.rs cache. No need for chrono crate. |
| File content storage | `std::fs::read_to_string` / `std::fs::write` | Simple file copies per the context doc. No compression, no delta encoding. |
| Directory creation | `std::fs::create_dir_all` | Standard library handles recursive mkdir |

## Existing Code and Patterns

- `src/main.rs` — dispatch function passes `&dyn LanguageProvider` to command handlers. S04 refactors this to pass `&AppContext`. The match arms pattern (D010) stays unchanged — just the parameter type changes.
- `src/commands/outline.rs`, `src/commands/zoom.rs` — established handler pattern (D021): `handle_*(req: &RawRequest, provider: &dyn LanguageProvider) -> Response`. After refactor, these pull `provider` from context: `handle_*(req: &RawRequest, ctx: &AppContext) -> Response`.
- `src/protocol.rs` — `RawRequest` and `Response` types. S04 command handlers use the same `Response::success()` / `Response::error()` constructors. No changes needed.
- `src/error.rs` — `AftError` enum. May need new variants: `CheckpointNotFound`, `NoUndoHistory`, `BackupError`. Or reuse `InvalidRequest` and `FileNotFound` for simplicity.
- `src/config.rs` — `Config` already has `checkpoint_ttl_hours: u32` and `project_root: Option<PathBuf>`. Both consumed by S04 directly.
- `src/parser.rs` — uses `RefCell<FileParser>` for interior mutability (D014), `HashMap` for cache, `SystemTime` for timestamps. S04 stores follow the same patterns.
- `tests/integration/protocol_test.rs` and `commands_test.rs` — `AftProcess` helper struct is duplicated between the two test files. Integration tests for S04 will add a third file following the same pattern. Consider extracting `AftProcess` into a shared test helper module to avoid the duplication (3 copies would be excessive).

## Constraints

- **Single-threaded binary** (D014) — no Mutex needed, RefCell is sufficient for interior mutability on stores.
- **`.aft/` directory location** — must be relative to project_root if set, otherwise relative to cwd. Must be created lazily on first write. Must be added to `.gitignore`.
- **Disk persistence format** — simple file copies, not git objects. Checkpoint files are full copies of source files stored under `.aft/checkpoints/{name}/{encoded_path}`. Backup entries are full file content stored under `.aft/backups/{encoded_path}/{backup_id}`.
- **Path encoding for storage** — file paths contain `/` which can't be used in filenames. Use a hex-encoding or separator-replacement scheme for directory names derived from source paths.
- **Config.checkpoint_ttl_hours** — checkpoints older than this are eligible for cleanup. Cleanup is triggered explicitly (list_checkpoints can prune) or on checkpoint creation. Not a background timer — binary has no async runtime.
- **S05 will call BackupStore.snapshot() before every mutation** — the API must be simple: `snapshot(&self, file_path: &Path) -> Result<String, AftError>` where String is the backup_id. S05 doesn't need to know about storage internals.
- **No new crate dependencies needed** — everything uses std (HashMap, fs, path, time). SHA256 would need a crate; simpler to use a monotonic counter + timestamp as backup ID.

## Common Pitfalls

- **Dispatch signature change breaking existing tests** — outline and zoom integration tests call these commands through the binary, not through Rust function calls, so the signature change doesn't break integration tests. Only unit tests that call `handle_outline`/`handle_zoom` directly need updating (there are 7 in outline.rs, 12 in zoom.rs). These just need the provider arg replaced with a context arg.
- **File path canonicalization** — BackupStore keys must be canonical paths. If `outline` gets `./src/main.rs` and `undo` gets `src/main.rs`, they must resolve to the same key. Use `std::fs::canonicalize()` on paths before using as keys.
- **Checkpoint "tracked files" ambiguity** — R008 says "all tracked files". Define "tracked" as: files the binary has seen via any operation (outline, zoom, future edits). BackupStore's key set is one source, but pre-edit files aren't in BackupStore yet. Better: checkpoint takes an explicit file list (required param), or snapshots all files with backup history. Go with explicit file list as primary, with `"all"` as a convenience that snapshots all files in BackupStore.
- **Disk write failures** — `.aft/` might be on a read-only filesystem or permissions might block writes. All disk operations must be fallible with clear error messages. In-memory state should remain consistent even if disk flush fails.
- **Empty undo stack** — `undo` on a file with no edit history must return a clear error, not panic. Same for restoring a nonexistent checkpoint.
- **Checkpoint name collisions** — creating a checkpoint with an existing name should either error or overwrite. Overwrite is more agent-friendly (agent doesn't need to check names first). Document the overwrite behavior in the response.

## Open Risks

- **AppContext refactor scope** — changing dispatch and handler signatures touches main.rs, outline.rs, zoom.rs, and their unit tests (~19 test functions). This is well-contained but needs to be the first task before building stores.
- **Backup ID format** — using a simple monotonic counter (per-process, not persisted) means IDs reset on binary restart. This is fine since the primary use case is within-session undo. If persisted undo across restarts matters, the counter needs to be derived from disk state on startup.
- **Large file checkpoints** — checkpointing a project with many large files could be slow and use significant disk space. Not a problem for M001 (agents work on small-to-medium files), but worth noting for future checkpoint size limits.
- **AftProcess duplication in integration tests** — currently duplicated in protocol_test.rs and commands_test.rs. Adding a third copy for S04 tests is technical debt. Extracting to a shared module (`tests/integration/helpers.rs` or `tests/helpers/mod.rs`) should happen in S04 to prevent further duplication.
- **Checkpoint file list scope** — if checkpoint takes an explicit file list, the agent must know which files to include. If it takes "all tracked", the agent gets a broader snapshot but might include files it didn't intend. Both modes should be available.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | apollographql/skills@rust-best-practices (2.3K installs) | available — general Rust guidance, not S04-specific |
| tree-sitter | plurigrid/asi@tree-sitter (7 installs) | available — not relevant to S04 (no parsing work) |

No skills directly relevant to S04's scope (file backup/checkpoint logic in Rust). The work is core Rust std library usage — no specialized libraries or frameworks involved.

## Sources

- Codebase exploration: src/main.rs dispatch pattern, src/config.rs checkpoint_ttl_hours, src/parser.rs RefCell/HashMap/SystemTime patterns
- S01 summary forward intelligence: command dispatch match pattern, RawRequest.params extraction, Response constructors
- D014 decision: RefCell for interior mutability in single-threaded binary
- D021 decision: command module pattern with per-command `handle_*` functions
- R007 requirement: per-file auto-backup and undo — "undo stack in-memory with periodic flush to .aft/"
- R008 requirement: workspace-wide checkpoints — "stored in .aft/checkpoints/, lightweight file copies"
- M001 boundary map S04→S05: BackupStore and CheckpointStore API surface
