# S04: Safety & Recovery System — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: live-runtime
- Why this mode is sufficient: Safety system must be tested through the actual binary protocol — checkpoint/restore correctness depends on real filesystem I/O and store state management

## Preconditions

- `cargo build` succeeds with 0 warnings
- The `aft` binary is at `target/debug/aft`
- A scratch directory for test files (the binary creates/restores files on the real filesystem)
- JSON commands sent as newline-delimited JSON on stdin, responses read from stdout

## Smoke Test

Send `checkpoint` → create a file → send `restore_checkpoint` → verify the file is gone (or restored to pre-checkpoint state). If checkpoint/restore round-trips correctly, the slice works.

## Test Cases

### 1. Checkpoint → Modify → Restore Cycle

1. Create two temporary files (`file_a.txt` with "original A", `file_b.txt` with "original B")
2. Send: `{"id":"1","command":"checkpoint","name":"before_changes","files":["file_a.txt","file_b.txt"]}`
3. **Expected:** Response has `"ok":true`, `"name":"before_changes"`, `"file_count":2`
4. Overwrite both files externally (write "modified A" to file_a, "modified B" to file_b)
5. Send: `{"id":"2","command":"restore_checkpoint","name":"before_changes"}`
6. **Expected:** Response has `"ok":true`, `"name":"before_changes"`, `"files_restored":2`
7. Read both files from disk
8. **Expected:** file_a.txt contains "original A", file_b.txt contains "original B"

### 2. Undo Restores Previous Version

1. Create a temporary file (`test.txt` with "version 1")
2. Send: `{"id":"1","command":"snapshot","file":"test.txt","description":"initial"}`
3. **Expected:** Response has `"ok":true` with a `backup_id`
4. Overwrite test.txt with "version 2"
5. Send: `{"id":"2","command":"undo","file":"test.txt"}`
6. **Expected:** Response has `"ok":true`, `"path"` matches the file
7. Read test.txt from disk
8. **Expected:** Contents are "version 1"

### 3. Edit History Returns Ordered Stack

1. Create a temporary file
2. Send three `snapshot` commands for the same file with different descriptions
3. Send: `{"id":"4","command":"edit_history","file":"<path>"}`
4. **Expected:** Response has `"ok":true`, `"entries"` array with 3 items
5. **Expected:** Entries are ordered most-recent-first (highest backup_id first)
6. **Expected:** Each entry has `backup_id`, `timestamp`, and `description` fields

### 4. List Checkpoints Returns Metadata

1. Create checkpoints named "cp1" and "cp2" with different file lists
2. Send: `{"id":"3","command":"list_checkpoints"}`
3. **Expected:** Response has `"ok":true`, `"checkpoints"` array with 2 entries
4. **Expected:** Each entry has `name`, `file_count`, and `created_at` fields
5. **Expected:** `file_count` matches the number of files in each checkpoint

### 5. Checkpoint Name Overwrite

1. Create a file, checkpoint it as "mycp" with 1 file
2. Create a second file, checkpoint as "mycp" again with 2 files
3. Send: `{"id":"3","command":"list_checkpoints"}`
4. **Expected:** Only 1 checkpoint named "mycp" exists
5. **Expected:** `file_count` is 2 (the overwritten version)

### 6. Multiple Undo Steps

1. Create a file with "v1"
2. Snapshot, overwrite with "v2", snapshot again, overwrite with "v3"
3. Send `undo` → file should be "v2"
4. Send `undo` again → file should be "v1"
5. **Expected:** Each undo pops one level from the stack

## Edge Cases

### Undo With No History

1. Send: `{"id":"1","command":"undo","file":"/tmp/never_snapshotted.txt"}`
2. **Expected:** Response has `"ok":false`, `"code":"no_undo_history"`, message includes the file path
3. **Expected:** Process stays alive — send a `ping` and get a response

### Restore Nonexistent Checkpoint

1. Send: `{"id":"1","command":"restore_checkpoint","name":"does_not_exist"}`
2. **Expected:** Response has `"ok":false`, `"code":"checkpoint_not_found"`, message includes "does_not_exist"
3. **Expected:** Process stays alive — send a `ping` and get a response

### Checkpoint With Empty File List (All Tracked)

1. Snapshot two files via `snapshot` command (populates BackupStore's tracked files)
2. Send: `{"id":"2","command":"checkpoint","name":"auto","files":[]}`
3. **Expected:** Checkpoint created with `file_count` equal to the number of tracked files (2)

### Edit History for Untracked File

1. Send: `{"id":"1","command":"edit_history","file":"/tmp/untracked.txt"}`
2. **Expected:** Response has `"ok":true`, `"entries":[]` (empty array, not an error)

## Failure Signals

- Any response with `"ok":false` where `"ok":true` was expected
- File contents after restore don't match original — checkpoint data corruption
- Process crashes (EOF on stdout) after any error command — process should stay alive
- `edit_history` entries out of order (not most-recent-first)
- `list_checkpoints` missing metadata fields (name, file_count, created_at)
- Checkpoint restore leaves files from a different checkpoint — name collision bug

## Requirements Proved By This UAT

- R007 (Per-file auto-backup and undo) — partially: snapshot + undo round-trip works; auto-snapshot on mutation is S05 scope
- R008 (Workspace-wide checkpoints) — fully: checkpoint create/restore/list/overwrite/cleanup all verified through protocol

## Not Proven By This UAT

- Auto-snapshot before mutations (R007 full) — requires S05 edit commands calling BackupStore.snapshot
- Disk persistence of stores — stores are in-memory only (D001 persistent process assumption)
- TTL-based cleanup in production conditions — unit tested but not exercised via binary protocol in UAT
- Cross-platform filesystem behavior — tested on macOS only

## Notes for Tester

- The `snapshot` command is a test-facing command (D027) — it exists to populate the backup store since S04 has no mutation commands. In production, S05's edit commands will call snapshot automatically.
- All file paths should be absolute or relative to the binary's working directory. BackupStore uses `std::fs::canonicalize` internally.
- Stderr output includes `[aft] checkpoint created/restored: ...` lines — check these for diagnostic verification but they don't affect stdout JSON responses.
- Checkpoint restore overwrites files on disk — use temporary directories for testing.
