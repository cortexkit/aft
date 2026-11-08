# Durability per store: measured syncs and a proposal

Status: implemented (portable full-sync commit points; no macOS-specific plain-fsync refinement). Base commit `c7ecd5490`. Measured on an
Apple M5 Max (Mac17,6) running macOS 27.0.1, internal APFS SSD. The machine was
shared with other build workers the whole time (load average about 48), so wall
times are noisy. The sync counts are exact and were the same on every repeat.

Line references are `crates/aft/src/...` unless the path says otherwise.

## Summary

"Sync" below means `F_FULLFSYNC`: on macOS, every AFT `sync_all()` is
`fcntl(F_FULLFSYNC)` (see "How AFT syncs on macOS"). On this disk one costs
about **4 ms** (p50 3.99 ms, p90 5.0 ms). A plain `fsync()` costs 0.024 ms.

| Store | Syncs per op today (measured) | Proposed | Guarantee kept | Risk |
|---|---|---|---|---|
| Bash task records and payload files | 8 per command (foreground; background is 6 at spawn + 2 at finish). 0 dir syncs. About 31–37 ms of a 50–72 ms command. | **0** | Same as today after a daemon kill (atomic rename + page cache). After power loss, no worse in practice: today the dir entries are never synced and replay reads `aft.db` first, which is not power-loss durable either. | Low. After power loss a `metadata.json` can be zero-length and the task is quarantined instead of replayed. **Changes what a user can recover**, marginally. |
| Backup (one edit) | 12 (14 for the first backup in a session) | **3** (portable), or **1 + 2 plain fsync** (macOS) | Every edit that returned has its undo entry after power loss: kept, and made true for a file's first backup (today it is not). | Medium: shared lease code; the post-edit fingerprint becomes best-effort. **Flagged.** |
| Undo (one step) | 5 | **2**, plus 1 **added** sync of the restored user file | An undo that returned stays undone after power loss (today it can be lost). | Low. **Changes recovery (improves it).** |
| Checkpoint create (N files) | N + 5 (8 for 3 files) | **N + 3** portable; **2 + N plain fsync** on macOS; plus 1 **added** parent sync for a new checkpoint | A checkpoint that returned survives power loss (today a new one can vanish). | Low. **Changes recovery (improves it).** |
| Checkpoint list / restore | 2 / 4 (lock only) | **0 / 0** | Unchanged. | Low (shared lease code). |
| Search index cache (`cache.bin`, spill) | 2 per cache write (+2 for the cache lease); 1 per 128 MiB spill segment | **0** | Unchanged: a torn cache fails its CRC and is rebuilt from source. | None for data; a cold rebuild after power loss. |
| Semantic index | 4 per full snapshot, 3 per segment append | **1 per snapshot, 0 per append** | Snapshot kept. A lost segment re-embeds only the files it covered. | Low. **Flagged**: re-embedding can cost API calls. |
| Pins (live pin) | 4 at create, 2 per `protect`/`trim` call (so 2 per key for per-key callers), 1 at release | **1 at create, 0 per protect, 0 at release** | Unchanged: keys are read only while their owner is alive. | Low. |
| `aft.db` (SQLite) | 0 per op; plain `fsync()` at WAL checkpoints | **Keep** | Atomic and consistent after any crash; recent commits can be lost on power loss (today too). | None. Documented, not changed. |
| Shared: `fs_lock` lease | 2 per acquire (file + dir), inside every backup, undo, checkpoint, semantic and cache op | **0** | Unchanged: a lease from before a crash is dead by identity, and a torn lease is treated as stale. | Low; contract test changes. |

Implemented correction: the table describes steady-state commits; first use also syncs each newly created ancestor directory into its parent, so a returned backup or checkpoint cannot vanish with its namespace.

Per-op saving at about 4 ms per removed sync: bash about 32 ms per command
(about half the measured response), edit about 36–44 ms of 52–62 ms, undo about
12 ms, checkpoint list about 8 ms.

Three principles drive the proposal:
1. **One durable commit point per logical operation:** write the data, sync it,
   rename atomically, sync the parent directory. Nothing more.
2. **Caches, and anything rebuildable or only meaningful while a process lives,
   get no sync.** Keep the temp-file + rename, because that is what protects
   readers from torn files after a daemon kill. The sync does not.
3. **Nothing is reported durable that isn't.** Today four places sync a file and
   then leave its directory entry unsynced, or rewrite it unsynced a few ms later:
   - bash `metadata.json` (section 1);
   - a file's first backup directory (section 2);
   - the backup `session.json` (section 2);
   - a new checkpoint directory (section 3).

   Undo also commits its synced stack before the unsynced restore of the user
   file (section 2).

## How AFT syncs on macOS

- **`File::sync_all()` and `File::sync_data()` are both `fcntl(fd, F_FULLFSYNC)`
  on Apple targets.** See the Rust standard library,
  `library/std/src/sys/fs/unix.rs:1264-1290` (rustc 1.99.0 source). So AFT never
  uses plain `fsync()` for its own files. It always asks the drive to flush its
  write cache, which is real power-loss durability and is expensive.
- **Directory syncs** (`File::open(dir)?.sync_all()`, for example `backup.rs:5252`,
  `checkpoint.rs:1470`, `fs_lock.rs:1382`, `search_index.rs:5150`) are also
  `F_FULLFSYNC`, on the directory fd.
- **SQLite uses plain `fsync()`.** It only uses `F_FULLFSYNC` when
  `PRAGMA fullfsync` is on (`libsqlite3-sys-0.28.0/sqlite3/sqlite3.c:41638-41652`),
  and AFT never sets it. AFT's databases run WAL with `synchronous=NORMAL`
  (`db/mod.rs:697`, `callgraph_store/mod.rs:8955`, `inspect/cache.rs:1930`,
  `blob_store/mod.rs:677`); the file-identity cache uses `OFF`
  (`db/file_identity.rs:531`). So a commit does not sync at all, and the WAL is
  `fsync()`ed only at checkpoints. That `fsync()` does not flush the drive cache.
  The census shows this: every SQLite sync in its traces is `fsync`, 0.02–0.6 ms.
- **Measured cost per call** (test `durability_census_sync_flavour_costs`; a 4 KiB
  freshly written file, 40 rounds):

  | Call | File p50 / p90 | Directory p50 / p90 |
  |---|---|---|
  | `F_FULLFSYNC` (`File::sync_all`) | 3.99 / 5.00 ms | 3.99 / 5.02 ms |
  | `F_BARRIERFSYNC` | 0.40 / 0.84 ms | 0.37 / 0.73 ms |
  | `fsync(2)` | 0.024 / 0.035 ms | 0.001 / 0.003 ms |

  `F_FULLFSYNC` cost barely depends on size here: syncing the 9.6 MB `cache.bin`
  of this crate's own sources took 8.5 ms.
- **What this means.** AFT's own stores pay the full drive-flush price on every
  sync. Its SQLite stores pay almost nothing and are correspondingly weaker. On
  macOS, an `F_FULLFSYNC` flushes the whole drive cache. Data that earlier plain
  `fsync()`/`F_BARRIERFSYNC` calls pushed to the drive becomes durable with the
  next `F_FULLFSYNC`. So a multi-file commit needs only **one** `F_FULLFSYNC`, at
  its commit point. That is a macOS-specific refinement. The portable form below
  keeps one sync per file written plus one directory sync. On Linux,
  `sync_all` is `fsync` and `sync_data` is `fdatasync` (same std lines).

### Crash model used below
- **Daemon kill** (SIGKILL, panic, OOM): the kernel survives. Every completed
  `write()` and `rename()` is visible after restart. Syncs protect nothing here.
  What protects readers is the temp file + atomic rename, which keeps a reader
  from ever seeing a half-written file.
- **Power loss or kernel panic:** anything not synced may be lost. That includes
  a new directory entry (create, rename, link) whose parent directory was not
  synced, and the contents of a renamed file whose data was not synced before
  the rename. That can leave the new name pointing at an empty or stale file.
- **Partial write** (a crash or ENOSPC mid-write): handled by temp + rename, not
  by sync.

## Method

A test-only syscall tracer, `crates/aft/tests/integration/durability_census_interposer.c`,
is loaded into the real `aft` binary with `DYLD_INSERT_LIBRARIES`. No store code
was changed. The tracer logs these calls with their path, file or directory kind,
duration and byte count:
- `F_FULLFSYNC`, `F_BARRIERFSYNC`, `fsync`, `fdatasync`;
- every rename and link;
- every write to a regular file.

`crates/aft/tests/integration/durability_census_test.rs` holds three ignored
tests:
- `durability_census_per_operation` drives a daemon through the protocol:
  configure, foreground bash ×3, background bash, write, `edit_match` ×3, undo,
  checkpoint create, list ×2, restore, and shutdown. It allows 1.5 s after each
  operation for background threads and slices the trace per operation. It then
  runs the same scenario untraced, for response times.
- `durability_census_in_process_stores` traces a child copy of the test binary
  that persists a semantic index, writes search caches and drives a live pin.
  The daemon scenario does not reach these: semantic indexing is off in the
  test harness, and no `views/` path appeared in the daemon trace.
- `durability_census_sync_flavour_costs` measures the cost of each sync call.

Rerun:

```
AFT_DURABILITY_CENSUS_OUT=/tmp/census cargo test --features test-hooks -p agent-file-tools --test integration -- \
    durability_census --ignored --nocapture --test-threads=1
```

`/tmp/census/{traced,untraced,in-process}/report.txt` then holds one table per
operation and the ordered list of syncs, renames and links behind it.

Caveats:
- The harness sets `AFT_TEST_DISABLE_FILE_WATCHER=1` and
  `AFT_TEST_ALLOW_TEMP_BACKUPS=1` (`tests/helpers/mod.rs`), and stores live under
  the system temp directory. Neither changes which syncs a store issues.
- An idle 1.5 s window had 0 syncs, so the per-operation counts carry no
  background noise. Heartbeats do not sync, as designed (`fs_lock.rs:677-718`).

## 1. Bash task records and payload files

Layout: `<storage>/<harness>/bash-tasks/<session-hash>/<task>/control/` and `io/`.

### What is written (measured: foreground `printf hi`, 3 identical repeats)

| # | File | Sync | Rename | Parent dir synced |
|---|---|---|---|---|
| 1–4 | `control/command.sh`, `wrapper.sh`, `environment.bin`, `manifest.blake3`. Each created with O_EXCL and written once (`sandbox_spawn.rs:651-675` → `bash_background/persistence.rs:1568-1575`). | 1 each (`persistence.rs:1571`) | none | no |
| 5–8 | `control/metadata.json`, rewritten once per state transition through `randomized_atomic_replace` (`persistence.rs:1543-1566`, called from `write_task_in_dir` `:1288-1300`) | 1 each (`:1553`) | 1 each (`renameat`, `:383-410`) | **no** |
| – | `aft.db-wal`: the task row mirrored per transition (`bash_background/registry.rs:1386-1392`) | 0 (WAL, `synchronous=NORMAL`) | – | – |

Totals per foreground command: **8 F_FULLFSYNC, 0 directory syncs, 4 renames**,
plus 70 WAL page writes (144 KB). Sync time measured inside the daemon was
30.6–37.2 ms. Traced responses took 50.6–72.5 ms; untraced responses took
81–193 ms on the loaded machine.

Background command: spawn reply **6** (4 payload + 2 metadata), finish **2**
(metadata). That is 8 per command, the same as foreground. The child writes
the exit marker through an inherited fd. AFT itself writes and syncs one only
when it kills a task (`persistence.rs:1824-1842`, `:1637-1643`,
`bash_background/pty_process.rs:385-397`).

### What each sync protects
- **Payload syncs (4): nothing.** The wrapper reads the payload right away, and
  AFT verifies it in-process against a digest (`sandbox_spawn.rs:676-690`). It
  is never used again after a restart: escalation grants that re-verify it are
  held in an in-memory map (`sandbox_spawn.rs:120-123`, consumed at `:560-600`). After a daemon kill the page
  cache holds the files. After power loss the child is dead too and is never
  re-run.
- **Metadata syncs (4): nearly nothing.**
  - The rename is never made durable: no directory sync in `PinnedDir::rename_to`
    (`persistence.rs:383-410`).
  - The task directory itself is created with `mkdirat` and also never synced
    (`persistence.rs:257-264`).
  - Replay reads `aft.db` first and consults the JSON only when the database has
    no rows for the session (`registry.rs:3232-3262`, `:3551-3581`).
  - `aft.db` loses recent commits on power loss (see section 7).

  So the JSON sync buys a non-torn file only in the case where the rename
  happened to survive, the database lost the row, and the database had no other
  rows for the session.
- The four metadata states are rewritten within about 60 ms of each other, so a
  sync of an intermediate state protects nothing the next write does not replace.

### Recovery today
- Replay prefers `aft.db` rows (`registry.rs:3232-3262`). The disk fallback
  quarantines unresolvable layouts and unreadable metadata as "invalid"
  (`registry.rs:3600-3668`). It skips uninitialized layouts younger than 5
  minutes (`:3604-3615`) and refuses to quarantine a task whose process is still
  live (`:3617-3623`, `:3652-3657`).
- Metadata is version-gated before parsing (`persistence.rs:1230-1260`).
  Concurrent replacement is tolerated by re-opening (`:1163-1205`).

### Proposal: **remove** all 8
- Keep the O_EXCL creates and the temp + `renameat`. They are what protect
  readers after a daemon kill.
- **Guarantee kept:** after a daemon kill, identical. After power loss, the newest
  metadata state or the whole task directory may be missing. That is already the
  case today, because nothing syncs the directory entries and `aft.db` is the
  primary.
- **Changes what a user can recover (flag):** a power loss can now also leave a
  zero-length `metadata.json`. Today the sync before the rename prevents that.
  Such a task is quarantined as "invalid" instead of being replayed from JSON.
  It only matters when `aft.db` also lacks the task. The command's output from a
  power-lost run is gone either way.
- **Alternative if bash history must survive power loss:** make it true in one
  place. Sync the terminal metadata state once and sync its directory: 2 syncs
  per command, plus 1 for the first task of a session. That only helps if replay
  trusts the JSON over a database row that can be older. Today it does not.
- **Saving:** 8 × about 4 ms, so about 32 ms per command, plus 4 fewer
  `F_FULLFSYNC` drive flushes competing with other work.

## 2. Backup / undo

Layout: `<storage>/<harness>/backups/<session-hash>/<path-hash>/{bak_*.bak, meta.json}`,
leases in `<session-hash>/.locks/`.

### What is written (measured)

One edit (`write` or `edit_match`; repeats 2–4 identical) runs two passes: the
snapshot before the mutation, and the post-mutation fingerprint after it
(`backup.rs:2358-2416`).

| # | What | File sync | Dir sync | Rename / link |
|---|---|---|---|---|
| 1 | Stack lease `.locks/<hash>.lock` (`backup.rs:3354-3375` → `fs_lock.rs:796-815`) | 1 | 1 (`.locks/`) | link |
| 2 | Content `bak_<id>.bak` through `write_temp_fsync_rename` (`backup.rs:3810-3829`, `:5218-5243`) | 1 | 1 (`:3830`) | rename |
| 3 | `meta.json` (`:3850-3860`) | 1 | 1 (`:3857`) | rename |
| 4 | Directory sync after pruning, unconditional, even when nothing was pruned (`:3862-3868`) | – | 1 | – |
| 5 | `session.json` touch, unsynced (`backup.rs:2838-2862`) | 0 | 0 | rename |
| – | The **user file** itself, `fs::write` in place, unsynced (`edit.rs:642`) | 0 | 0 | – |
| 6 | Post-state pass: a second lease (`backup.rs:2380`) | 1 | 1 | link |
| 7 | Post-state pass: `meta.json` again, now with `post_state` (`:2405-2412`) | 1 | 1 + 1 (prune) | rename |

Totals: **12 F_FULLFSYNC per edit** (5 file + 7 directory), 4 renames, 2 links.
The first backup in a session adds `session.json` through `ensure_session_marker`:
1 file + 1 dir sync (`backup.rs:3330-3352`), so 14. Sync time was 43.9–55.9 ms of
a 51.9–62.4 ms traced response. Plus 28 WAL page writes for the `aft.db` mirror
(`backup.rs:3888`), unsynced.

One **undo**: lease 1 + 1, `meta.json` 1, then 2 directory syncs. That is
**5 F_FULLFSYNC**. The undo writes the user file in place before the meta commit
and does not sync it (`backup.rs:4695`; trace order: lease → user file →
`meta.json` → dir syncs → `session.json`).

### What each sync protects
- **Content sync + dir sync:** after power loss, `meta.json` never names a
  content file that does not exist or is empty. This matters: content is not
  checksummed. The loader checks only that the file exists
  (`backup.rs:3673-3691`), so an empty `.bak` behind a durable `meta.json` would
  be restored silently as an empty file.
- **`meta.json` sync + dir sync:** the undo stack survives power loss. This is the
  store's one real commit point.
- **Third directory sync (after prune): nothing.** Pruning only deletes
  unreferenced files and temp files. A deletion lost to power loss is redone by
  the next write's prune (`backup.rs:5478-5499`).
- **Post-state pass (5 syncs):** this rewrites the same `meta.json` a few ms
  later, to record what the edit left on disk. `needs_capture_before_undo`
  (`backup.rs:626-639`) uses it to take a safety checkpoint before an undo would
  overwrite content changed outside AFT. Losing it after power loss means the
  entry looks like one written before this field existed. Undo then restores
  without the safety capture (`:634-636`). The user file the fingerprint
  describes is itself unsynced (`edit.rs:642`), so after power loss the
  fingerprint may not describe the disk anyway.
- **Lease syncs (2 per pass): nothing.** See "Shared: `fs_lock` leases".
- **`ensure_session_marker` sync: nothing.** The same file is rewritten unsynced
  by `write_session_marker` a few ms later, on every snapshot and undo
  (`backup.rs:2838-2862`).
- **Not durable although synced:**
  - The **first** backup of a file creates `<path-hash>/` with
    `create_private_dir_all` (`backup.rs:3799-3802`, `:5180-5193`). Nothing syncs
    the session directory that holds that new entry.
  - The first backup of a session likewise never syncs `backups/` for the new
    `<session-hash>/`.
  - So after power loss, a file's whole undo history can vanish even though
    every file inside it was synced.
- **Undo is reported durable but isn't:**
  - Undo commits the popped stack (`meta.json`, synced) after restoring the user
    file, which is not synced.
  - After power loss, the stack can say "undone" while the file still holds the
    edited content, or is torn.
  - The undo step is then lost.

### Recovery today
- Disk meta is authoritative; `aft.db` is a fallback when disk has no stack
  (`backup.rs:1548-1599`, `:2139`).
- Unparseable `meta.json` → that file's undo returns an I/O error naming the file
  (`backup.rs:3642-3670`).
- Missing content file → error (`:3673-3691`).
- Newer-format meta is refused by version (`:3659`).
- Content bytes are not verified.

### Proposal
**Reduce the edit from 12 to 3** (14 → 5 on a session's first backup):
1. Content: temp write, sync, rename. Keep.
2. `meta.json`: temp write, sync, rename. Keep.
3. **One** directory sync after both renames: they are in the same directory.
   Drop the extra dir sync after content and the one after prune.
4. Post-state pass: temp + rename, **no sync**. It is a best-effort annotation.
   Implemented correction: fingerprints use a private `post-state.json` sidecar overlaid on reads; an unsynced replacement of durable `meta.json` could tear and lose the whole undo stack, not just fingerprints.
   Alternatively fold it into the next snapshot write.
5. Leases: no sync (see shared section).
6. **Add:** when `<path-hash>/` is new, sync the session directory (+1). When the
   session directory is new, sync `backups/<harness>/` (+1). Make
   `ensure_session_marker` unsynced, because `write_session_marker` rewrites it
   anyway.

- **macOS refinement:** steps 1–2 use plain `fsync()` (or `F_BARRIERFSYNC`), and
  step 3 is the single `F_FULLFSYNC`. That gives 1 drive flush per edit,
  about 4.5 ms instead of about 48 ms.
- **Add (optional):** a content hash in `meta.json`, checked on load. Then a torn
  `.bak` after any crash is reported instead of restored, and the content sync
  becomes an ordering aid rather than the only guard.

**Reduce undo from 5 to 2, and add 1:**
- **Add:** sync the restored user file before committing `meta.json`.
- Then sync `meta.json` and its directory. Drop the lease syncs and the post-prune
  sync.
- An undo that returned then stays undone after power loss.

- **Guarantee:** every edit or undo that returned is in the undo stack after
  power loss, including a file's first backup (new). The post-edit fingerprint
  is best-effort.
- **Flag, changes recovery:** a power loss within a few ms after an edit can drop
  the fingerprint. The next undo of that entry then skips its safety checkpoint.
  The improvements (first backup, undo ordering) also change recovery, for the
  better.
- **Saving:** about 9 × 4 ms = 36 ms per edit portable, about 44 ms with the
  macOS refinement, and 12 ms per undo before the added sync.

## 3. Checkpoints

Layout: `<storage>/<harness>/checkpoints/<session-hash>/<name>/{file_*.blob, meta.json}`.
The mutation lock is `checkpoints/<project-key>/checkpoint.lock` under the
process's default storage directory, chosen at process start
(`checkpoint.rs:318-330`); in the census that is the harness cache directory.

### What is written (measured, 3 files)
- **create:**
  - lock: lease 1 + dir 1 (`checkpoint.rs:442-467`);
  - each blob through `write_temp_fsync_rename`: 1 sync + rename
    (`:995-1009`, `:1443-1467`);
  - 1 dir sync (`:1028`);
  - `meta.json`: 1 + rename (`:1045`);
  - 1 dir sync (`:1052`).

  That is **N + 5** (8 for 3 files). Sync time was 37–64 ms.
- **list:** **2**, the lock only. It still reads every blob (`:928-976`).
- **restore:** **4**. The lock is taken twice: once to list the paths
  (`commands/checkpoint.rs:153` → `checkpoint.rs:700-720`) and once to restore.
  The restored user files are written with `fs::write` in place, unsynced and not
  atomic (`checkpoint.rs:1683-1721`).

### What each sync protects
- **Blob syncs:** ordering before `meta.json`. As with backups, blobs carry no
  checksum (`checkpoint.rs:217-237`, `:1395-1415`). A durable meta over an empty
  blob would restore an empty file silently.
- **`meta.json` sync + dir sync:** the checkpoint survives power loss. This is the
  commit point.
- **The first dir sync (after blobs) is redundant:** the second one, after the
  meta rename, covers the same directory.
- **Not durable although synced:** the checkpoint directory `<name>/` is new for
  every create (`checkpoint.rs:986-991`), and nothing syncs its parent. So a new
  checkpoint can vanish entirely on power loss.
- **Lock syncs: nothing** (shared section). `list` pays 8 ms for nothing.

### Recovery today
- One unparseable `meta.json` fails hydration for the **whole session**.
  `hydrate_session_locked` propagates the error with `?` (`checkpoint.rs:967`).
  So `list`, `create` and `restore` for every checkpoint in that session error
  until the damaged directory is removed by hand. That is "refuse", and broader
  than it needs to be.
- Missing blob → error (`:1405-1408`).

### Proposal
- **create: N + 5 → N + 3 portable:**
  - N blob syncs;
  - `meta.json` sync;
  - one dir sync;
  - **add** one parent sync for the new `<name>/` directory;
  - drop the lock syncs and the first dir sync.
- **create on macOS:** 2 `F_FULLFSYNC` (meta and its directory, or a single
  final one) + N plain `fsync()`. That is about 8 ms instead of about 32 ms for
  3 files, and the saving grows with N.
- **list / restore:** 2 / 4 → **0**.
- **Guarantee:** a checkpoint that returned survives power loss. That is new
  for a fresh checkpoint.
- **Flag:** the added parent sync changes recovery, for the better.
- **Recommended (not a sync change):** skip and report a damaged checkpoint
  instead of failing the session (`checkpoint.rs:967`). Optionally store a blob
  hash so a torn blob is detected.

## 4. Search index cache and spill

Layout: `<storage>/index/<project-key>/cache.bin`, lease `cache.lock`.

### What is written (measured)
- **Cache write:**
  - temp `cache.bin.tmp.*`: 1 sync (`search_index.rs:4413`);
  - rename (`:4416`);
  - parent dir sync (`:4417`, `:5150-5156`).

  That is **2**: 9.2 ms for a 10 KB cache, 17.1 ms for the 9.6 MB cache of
  `crates/aft/src`.
- The daemon's cold start also takes the cache lease (1 + 1). It wrote
  `cache.bin` twice in the census startup (`runtime_drain.rs:2236-2252`), and
  again on graceful shutdown when dirty.
- **Spill segments** (`search_index.rs:4614-4661`): 1 sync per segment. Segments
  are written only when the in-memory block reaches 128 MiB of records (about 8M
  postings; `:76-78`, `:4273-4280`). They are merged into `cache.bin` and
  deleted. Not reached in the census; the count is from code.

### What each sync protects
- **Nothing a user would lose.** `cache.bin` is a cache rebuildable from source.
  A torn or empty file fails its header or CRC check (`search_index.rs:7626-7636`,
  `:2022-2130`) and is rebuilt.
- Spill segments are temporary input to the merge in the same process, and are
  deleted after it. Syncing them protects nothing at all.

### Recovery today
CRC-checked sections (`:4396-4404`, `:7626-7636`). Any read failure → rebuild
from the project.

### Proposal: **remove** (2 → 0 per write; spill 1 → 0 per segment)
- Keep temp + rename for concurrent readers.
- **Guarantee kept:** the cache is never wrong, only absent.
- **Cost of the trade:** after power loss right after a write, the next start
  rebuilds the index (a cold build: seconds on a large repo).
- **Saving:** about 8–17 ms per cache write, plus about 4 ms per 128 MiB spilled.
  It also removes two drive flushes per write that compete with foreground syncs.

## 5. Semantic index

Layout: `<storage>/semantic/<project-key>/semantic.bin`, lease
`semantic.persist.lock`.

### What is written (measured, in-process, stub embedder)
- **First persist (full snapshot):**
  - lease 1 + 1 (`semantic_index.rs:4309-4323`, taken at `:7029`);
  - temp `semantic.bin.tmp.*`: 1 sync (`:6767`);
  - rename (`:6800`);
  - parent dir sync (`:6804`).

  That is **4**, 30.5 ms.
- **Persist after one changed file (segment append):** lease 1 + 1, then append
  the blake3-framed segment to `semantic.bin` and sync it (`:6890-6911`). That
  is **3**, 10.8 ms.
- Torn-tail truncation on load or append syncs once (`:7150-7160`,
  `:7612-7622`).

### What each sync protects
- **Snapshot sync + dir sync:** after power loss, `semantic.bin` is not empty
  or torn. Without it the loader sees a corrupt file, deletes it and rebuilds
  (`semantic_index.rs:7632-7645`). That is a **full re-embed**: minutes, and
  paid API calls with a remote embedder. The index is "rebuildable", but not
  cheaply.
- **Segment append sync:** the frame is checksummed, and a torn tail is detected
  and cut off (`:7150-7160`, `:7612-7622`). Without the sync, power loss can lose
  the last frame(s). The index records per-file mtime and size
  (`semantic_index.rs:4056-4060`), so the files those frames covered show as
  stale and are re-embedded by `refresh_stale_files` (`:5539`). Only those files.
- **Lease syncs: nothing** (shared section).

### Proposal
- Full snapshot: **keep 1**, the data sync before the rename, so the name never
  points at an empty file. Drop the dir sync: if a power loss undoes the rename,
  the previous snapshot, synced when it was written, is still there. Only a
  first build that is lost within moments of finishing is rebuilt. Snapshots
  are rare: first build and compaction.
- Segment append: **remove** (3 → 0). Leases: remove.
- **Guarantee:** the base snapshot survives power loss. The newest appends may be
  lost, and their files are re-embedded.
- **Flag, changes recovery:** after power loss, files edited just before the
  crash are re-embedded. With a remote embedder that costs a small number of
  API calls.
- **Saving:** about 12 ms per incremental persist, and about 12 ms per snapshot.

## 6. Pins (views live pin and assembly pins)

Layout: `<storage>/views/v2/<family>/pins/<label>.{json,keys}`.

Production wiring: no `views/` path appeared in the daemon trace. The views
planes create live pins at `views/trigram.rs:572`, `views/first_load.rs:984` and
`views/semantic.rs:537`. Another change is batching the per-key callers; the
counts below are the current per-call costs.

### What is written (measured, in-process)
- **Live pin create:**
  - keys file: temp, 1 sync, rename, dir sync (`pins/live.rs:141-165`);
  - then metadata: temp, 1 sync, rename, dir sync (`pins/mod.rs:355-372`);
  - keys first by design (`pins/live.rs:86-88`).

  That is **4**, 17 ms.
- **`protect` or `trim` call that changes the set:** the keys file is rewritten,
  1 + 1 = **2** (7–8 ms), whether it adds 1 key or 10. The rewrite issues one
  `write` per key line (330 writes for 10 single-key calls).
- **Per-key callers** (`protect(&[key])` in a loop): **2 per key**. 10 keys cost
  20 syncs and 73 ms.
- **Release:** 1 dir sync (`pins/live.rs:173`).
- **Assembly pin:** 4 at create (`pins/mod.rs:342-372`), then 2 per renewal every
  10 minutes (`pins/mod.rs:208-222`).

### What each sync protects
- **Keys syncs: nothing.**
  - Both sweeps read a pin's keys only while its owner process is alive
    (`gc/family.rs:527-541`, `gc/mod.rs:146-153`).
  - While the owner lives, the page cache serves the latest keys, synced or not.
  - After a crash or power loss the owner is dead, judged by pid and start time
    (`pins/mod.rs:318-322`). The pin is then reclaimed and its keys never read.
- **Metadata sync at create: protects the sweep.** `gc/family.rs:517-518` reads
  metadata strictly and **aborts the whole sweep** on a malformed file. A
  power-lost rename without a data sync could leave a zero-length `.json`, and
  family GC would stall until it is removed by hand. The older sweep skips such a
  file instead (`gc/mod.rs:136`).
- **Dir syncs, and the release sync: nothing.** A pin that reappears after power
  loss belongs to a dead owner and is reclaimed.

### Proposal
- Keys rewrites: **remove** (2 → 0 per call; per-key callers 2 per key → 0).
- Create: **keep the metadata data sync** and drop its dir sync and the keys
  syncs (4 → 1). Alternatively make the strict sweep reclaim an unreadable pin
  file older than the current boot; then 0.
- Release: remove.
- **Guarantee kept:** a live owner's keys are always protected. A dead owner's
  pin is reclaimed.
- **Saving:** about 8 ms per protect batch, and 8 ms per key for per-key callers
  until they are batched.

## 7. `aft.db` (and the other SQLite stores)

- `synchronous=NORMAL` in WAL mode (`db/mod.rs:688-699`).
- **0 syncs per operation** in every census slice. Bash rows (70 WAL page writes
  per command) and backup mirrors (28) are not synced.
- Plain `fsync()` runs at WAL checkpoints and journal transitions:
  - 4 at startup in the census;
  - 0.02–0.05 ms each;
  - `sqlite3.c:41638-41652`.

**Protects:** SQLite's own atomicity. After any crash the database is consistent.
After power loss, transactions since the last checkpoint can be lost, and since
SQLite's `fsync()` does not flush the drive cache, even checkpointed pages are
not strictly power-loss durable on macOS.

**Recovery:** SQLite WAL replay. AFT treats a missing row as a miss and falls
back to disk where it has a disk copy: backup (`backup.rs:1575-1599`) and bash
(`registry.rs:3232-3262`).

**Proposal: keep.** It is the right trade for mirrors and indexes. This store is
**the reason the bash JSON syncs buy nothing**: the replay primary is weaker
than the fsynced secondary. If an operator ever wants bash history to survive
power loss, it is this store's settings that would have to change, not the JSON
syncs.

## Shared: `fs_lock` leases

Used by:
- backup stacks (`backup.rs:3354-3375`);
- checkpoints (`checkpoint.rs:442-467`);
- the semantic persist lock (`semantic_index.rs:4309-4323`);
- the search cache lock;
- callgraph, inspect and symbol writers (census startup).

What `create_lock_file_atomically` (`fs_lock.rs:801-815`) does:
1. writes a temp file;
2. syncs it (`fs_lock.rs:796-799`, `:1392-1395`);
3. hard-links it to the lease name;
4. syncs the parent (`:1382-1389`).

That is **2 per acquire**. Release unlinks without a sync (`:902-926`).
Heartbeats are already unsynced, by design (`:677-718`).

**What the syncs protect: nothing.**
- Ownership is decided by liveness. A same-host owner from before a crash is dead
  by pid + start time, plus boot id on Linux (`fs_lock.rs:1480-1505`).
- A lease that vanished after power loss means no live owner, which is correct.
- A torn or empty lease is "malformed" and is removed after one poll
  (`fs_lock.rs:370-390`, `:756-761`).

**Proposal: remove both** (2 → 0 per acquire). This is shared infrastructure.
The existing test `lease_create_and_reclaim_still_fsync` (`fs_lock.rs:2076`)
asserts the current contract and must be changed deliberately, with this
reasoning in its place. The reclaim-token path (same test) should be reviewed
at the same time.

**Saving:**
- backup: 4 per edit;
- undo: 2;
- every checkpoint operation: 2;
- every semantic persist: 2;
- each startup writer lease: 2.

## Outside the brief (notes, not proposals)
- **User files are written in place and never synced.** That covers edits
  (`edit.rs:642`), undo restores (`backup.rs:4695`) and checkpoint restores
  (`checkpoint.rs:1698`), all `fs::write`, which truncates then writes. A daemon
  kill mid-write can tear a user file, and a power loss can leave it empty. That
  is a bigger durability gap than any store above. The backup taken before an
  edit is synced, so it covers the edit case; nothing covers an undo or restore.
- **Other stores synced at startup** (census configure slice; 83 syncs before the
  first request):
  - callgraph generation, pointer and readers (`callgraph_store/mod.rs:8757`,
    `:8817`, `:10124`);
  - inspect pointer (`inspect/cache.rs:1625`);
  - symbol cache (`symbol_cache_disk.rs:188`, `:413`);
  - the reader floor (`reader_floor.rs:260`, `:277`): its file + directory syncs are retained and count-pinned because it is a monotonic rollback-safety guard, not a cache; it must never lag newer durable data after a crash.
  - artifact owner manifests (`artifact_owner.rs`);
  - the root cache (`root_cache.rs:896`).

  These are mostly caches or pointers to rebuildable generations, and they would
  follow principle 2. They were not analysed one by one here.
  Implemented: callgraph/inspect/symbol artifacts and live artifact-owner/read markers now have no extra drive flushes; SQLite's own policy, the reader floor, audit/logging syncs and view publication remain unchanged.
- The gh shim (`gh_shim.rs:6177`, `:6511`) and logging's terminal line
  (`logging.rs:999`) sync on purpose, for audit and crash diagnostics. They are
  rare, so keep them.
- View publication syncs its blob databases, WAL and parent before moving a
  generation pointer (`views/mod.rs:588-625`, `:867`). It did not run in the
  census; it belongs with the views planes when they are wired in.
