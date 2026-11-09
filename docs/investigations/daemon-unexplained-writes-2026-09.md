# Daemon unexplained writes — September 2026

## Scope and result

Investigated AFT `7cf7345d6a2d09422af5acb5524248b88f7721e2` (0.58.0) on macOS. PID 11567 was only observed with `ps`, `lsof`, log reads and SQLite's online backup from a **separate read-only process**. It was not suspended, traced, injected into, or restarted. All workload runs and syscall instrumentation below target new, isolated processes.

**The 9.3 GiB/h production residual is not yet fully attributed.** A small, realistic daemon workload reproduces a sizeable *percentage* of unexplained writes, but not that absolute rate. In that workload the largest missing named writer is the symbol snapshot: `symbol_cache_disk.rs:190–195` explicitly credits its serialized length to **logical bytes and zero physical bytes**. This is not evidence that symbols explain the production incident: the captured production hour has only 54,324,905 symbol bytes. Semantic saves have the same logical-only accounting, but together these do not explain the remaining multi-GiB production gap either.

There are three distinct quantities here: bytes passed to write syscalls, SQLite page/frame credits, and kernel-reported physical writes. Calling all three “physical” does not make them interchangeable. A new missing seam must not simply take a process-wide before/after delta while other roots write concurrently; that would assign unrelated writes and can double-count them.

## What “unexplained” means

- `process_io.rs:14–34` obtains `ri_diskio_byteswritten` and `ri_logical_writes` using `proc_pid_rusage(RUSAGE_INFO_V4)` on macOS. Linux uses `/proc/self/io`. These are process counters, not net storage growth or tree-size differences.
- `write_ledger.rs:479–616` folds pending per-domain counters and same-boundary process deltas into minute rows, retaining seven days. The shared aft.db's pager is sampled before and after the fold. Census reads persisted rows plus pending counters and a live process delta (`619–803`). The start boundary rounds down to a minute; coverage must be checked.
- `write_ledger.rs:810–814` computes the **signed** difference: `process.physical_bytes - attributed_physical_bytes - unmeasurable_physical_bytes_estimate`. A negative residual is possible. Named estimates are not measured writer bytes. Root-filtered writer rows still coexist with a process-wide total; do not interpret that difference as a root's physical write rate.
- `db/lifecycle.rs:583–618` uses `SQLITE_DBSTATUS_CACHE_WRITE * page_size`, reset at each sample. It does not measure all VFS writes. WAL frame headers, rollback/subjournals, sorter spills, filesystem allocation/writeback effects and delayed sampling can escape this counter. `427–460` credits autocheckpoint frame movement separately. `352–419` deliberately keeps a conservative baseline when a WAL restart is ambiguous; last-close checkpoint estimates are recorded before close, not measured from a second file descriptor.

## Production snapshot: do not confuse file size with writes

A short-lived Python `sqlite3` process opened `file:…/aft.db?mode=ro` with a 3-second busy timeout and ran `source.backup(destination, pages=512, sleep=.05)`. Backup completed in 5.17 seconds; all SQL below ran on the isolated destination. No `immutable=1`, raw copy, or raw read of live database/WAL/SHM files was used. The parent explicitly confirmed this separate-process SQLite usage is permitted; the forbidden second-descriptor case is inside a process already holding SQLite locks.

`PRAGMA quick_check` on the snapshot returned `ok`. Page size is 4,096; page count 403,121; freelist count 169,098.

| Space owner (table and its indexes) | Bytes | Rows in table |
| --- | ---: | ---: |
| bash_tasks | 498,384,896 | 160,502 |
| backups | 212,086,784 | 192,865 |
| compression_events | 196,231,168 | 220,324 |
| write_ledger_minutes | 28,971,008 | 108,057 |
| write_ledger_unmeasurable_minutes | 18,116,608 | 33,523 |
| Other allocated pages | 4,767,744 | — |
| **Freelist (reusable, not live table contents)** | **692,625,408** | — |
| **Total main file** | **1,651,183,616** | — |

The three main tables alone occupy 423,292,928 (bash), 111,890,432 (backups), and 129,224,704 (compression) bytes, before indexes. Thus the 1.65 GB file is not a 1.65 GB continuously rewritten payload, and **41.95% is already free space**. Deleting rows does not shrink SQLite's main file without compaction.

Retention at the snapshot's latest ledger minute, `1790616120000` (2026-09-28 17:22 UTC): zero compression events older than 30 days, 6,295 lifetime rollup rows, and only 24 age-qualified acknowledged terminal bash rows. Bash pruning additionally requires absent task layout, no protected live registration/lifecycle state and related safety checks (`db/bash_tasks.rs:180–209`); the count is not proof that those 24 are safe to delete. Ledger timestamps span seven days. These are evidence that compression/bash/ledger retention is operating, not that every store has an age cap. Backup depth retention is per session/path, not a global historical row cap. Alert record tables are absent from this aft.db snapshot; standing roots have 18 rows and freshness 21. No live VACUUM is proposed: it would add substantial writes and would not cure recurrent traffic.

The same copied ledger's last complete hour (60 minute buckets ending 17:22 UTC) reports:

| Domain | Credited physical bytes | Logical-only bytes where applicable |
| --- | ---: | ---: |
| callgraph_refresh | 886,427,648 | — |
| callgraph_checkpoint | 752,758,784 | — |
| aft_db | 323,563,520 | — |
| inspect_cache | 205,881,344 | — |
| search_index_delta | 178,445,041 | — |
| callgraph_cold_staging | 782,336 | — |
| semantic_compaction | 0 | 464,345,252 |
| semantic_delta | 0 | 161,348,859 |
| symbol_cache | 0 | 54,324,905 |
| backups | 0 | 39,293,321 |
| logs | 0 | 1,454,604 |
| bash_task_io | 0 | 30 |
| **OS process total** | **8,444,788,736** | — |

Last-close checkpoint estimates total another 4,481,024 bytes (12 observations). This is an independent snapshot window, not a remeasurement of the task's original 17 windows. The listed logical-only payloads total about 0.67 GiB, insufficient to label the multi-GiB residual “semantic/symbol saves” without more measurement.

## Isolated daemon reproduction

Built this checkout with `cargo build -p agent-file-tools --bin aft --locked`. Started installed ck-subc 0.22.1 with that binary as its AFT module, a kernel-selected port, new process group, and private HOME, every XDG directory and TMPDIR. Three independent Git roots each contain 200 Python files / 4,000 functions. Three sessions each run a 10,001-line bash command, search, inspect, four successful edits and four more searches. Waited 65 seconds for maintenance/writeback. Semantic service was disabled. Inspect completed but reported missing authoritative language diagnostics; this is not an LSP test. This fixture did not exercise a large callgraph build.

The measurement interval was **70.726 seconds**, PID 53032. OS physical delta and census process delta both equal **7,000,064 bytes**. Census attributed **3,079,478**; new estimates **0**; unexplained **3,920,586** (56.0%). Existing startup estimates were removed by subtraction. Rates below are normalized from this short run, **not a claim of sustained production rates**.

The optional macOS interposer records successful write/pwrite/writev byte counts by F_GETPATH and fsync counts; it does not open database files or inspect their contents. It rewrites its own TSV every two seconds, which itself contributes to the process OS counter. Socket/pipe writes and paths beyond the 4,096-row bound are excluded. The TSV is a syscall-payload diagnostic, **not a per-file physical-I/O meter**. Differencing snapshots avoids attributing database initialization to the workload.

| Writer | Syscall bytes in interval | Syscall bytes/hour | Census physical bytes/hour |
| --- | ---: | ---: | ---: |
| Symbol snapshots | 2,295,132 | 116,823,119 | **0** |
| Search snapshots | 1,708,389 | 86,957,670 | 86,903,156 |
| aft.db and sidecars | 1,256,600 | 63,961,433 | 63,588,842 |
| Inspect SQLite | 135,039 | 6,873,538 | 6,254,640 |
| Logs | 53,271 | 2,711,515 | 0 |
| Backup files/metadata | 51,528 | 2,622,795 | 0 |
| Edited source files | 25,116 | 1,278,414 | 0 |
| Artifact-owner leases | 20,554 | 1,046,207 | 0 |
| Bash metadata/control | 16,977 | 864,136 | 0 |
| Other small files/locks | 2,145 | 109,181 | 0 |
| Semantic, views/blob publication, large callgraph refresh | Not exercised | Not measured | Not measured |

The dominant missing **payload** seam in this fixture is symbol snapshot publication (`symbol_cache_disk.rs:176–195`), 2.29 MB. It is smaller than the 3.92 MB OS residual: filesystem effects and probe output are not separated by this experiment. It would be incorrect to attribute the entire residual to that file.

## Writer coverage inventory

“Covered” below means the named ledger model has a counter, not that it measures every physical byte.

| Writer / path under `crates/aft/src` | Present attribution / remaining gap |
| --- | --- |
| `db/mod.rs:396–405`: aft.db bash tasks, watches, compression events/rollups, backups, state/alerts, GitHub cache, standing roots, ledger itself | All share tracked AftDb connections. Pager and hook checkpoints are covered in aggregate, **not per table**. Rollback/subjournals, temporary sorters and close/restart ambiguity remain. They are not wholly uninstrumented connections. |
| `callgraph_store/mod.rs:4315–4320`, `db/lifecycle.rs:89–110` | Refresh, cold staging and checkpoints have separate domains. In-flight cold sampling already exists. Cold migrations/copies, journals/sorters and last close need separate accounting; raw file growth cannot measure WAL reuse. |
| `inspect/cache.rs`, tracked InspectScopeCache | Pager plus checkpoint credit in inspect_cache; the same SQLite temporary-file and boundary gaps apply. |
| `db/lifecycle.rs:160–236` raw opener inventory | AliasStore WAL, ManifestSqliteStore rollback journal, GC blob-store sweeps, derived TRUNCATE checkpoint and PathStatusStore are named unmeasurable seams. Read-only projection/semantic/blob-reader connections can still use temporary storage, but are not ordinary main-database writers. |
| `symbol_cache_disk.rs:176–195` | Full snapshot temporary file + rename + sync; **logical only**. |
| `semantic_index.rs:6728–6762` and remaining cold/delta/compaction credits | **Logical only** serialized bytes; torn-tail truncation, fsync/rename and side effects are not physical credits. |
| `views/io.rs:107–125` | Blob, derived clone/materialize and closure phases use process deltas; susceptible to overlapping other-root writes. Manifest and CAS phases have no counter in this function. Raw auxiliary SQLite openers are classified separately above. |
| `bash_background/persistence.rs:1738–1785`, `1218–1263` | Guarded task writer credits logical bytes. Task metadata/control/marker replacement and task output are not universally physical-attributed. Direct child output must not be assigned to the daemon OS counter automatically. |
| `backup.rs:3761`, `checkpoint.rs:918` | Logical payload credit, not all metadata, fsync, directory operations or pruning effects. DB mirrors belong to aft_db. |
| `logging.rs:1000` | Batch append bytes are logical-only; rotation/deletion metadata is not separately measured. |
| `fs_lock.rs:677–719`, `artifact_owner.rs:598–613` | After the heartbeat change: ordinary heartbeat overwrites in place without fsync, same-length JSON; length change takes unsynced atomic replacement. Creation/reclaim still sync. Neither is a ledger physical seam. Owner lease/read-marker updates and other small control files also remain outside named physical domains. |

This inventory covers the requested persistence families; it is not proof that every filesystem syscall in the daemon has been statically enumerated. Root-cache pointers/read markers, artifact-owner metadata, effective-path cache and edited user files are additional small writers to retain in a future per-file census.

## Offline database controls

A prefrontal callgraph snapshot was also acquired through the authorized read-only SQLite backup API (3.48 seconds). The ignored Rust probes operate only on these snapshots/copies. Existing `db::wal_credit_probe` counts SQLite VFS payload bytes, not kernel physical bytes; it must not be treated as an exact OS physical attribution oracle.

- Production bash session reader, largest copied session: **zero** physical/logical writes in both passes (0.205 / 0.362 seconds).
- Production callgraph dead-code projection: **32,768 / 49,152** physical bytes (20.201 / 16.551 seconds). These reads did not explain multi-GiB writes.
- Synthetic sparse control: delete/reinsert the busiest file's 14,220 refs ten times on a copied callgraph, using tracked production connections and the 4,000-page checkpoint threshold. **2,132,348,928 physical bytes**, 2,161,443,618 logical bytes in 31.816 seconds; new ledger credits 1,612,845,056. The ~519.5 MB gap is real in this control, but its bulk DELETE/INSERT statements are **not the production refresh's per-row statement shape**. The syscall trace shows large `etilqs_*` temporary journals as well as main/WAL traffic. Do not extrapolate this control into a production bytes/hour claim.

## Proposed next change

No production behavior, retention policy, durability setting or cache format is changed here. The smallest useful follow-up is **explicit measurement-basis reporting** plus named SQLite temporary-journal and logical-only artifact seams. Keep physical OS counters, file payload bytes, estimates and unmeasurable operations distinct. For symbol/semantic saves, credit known file-write payload to a payload metric or explicitly labeled estimate; don't silently promote serialized length into an exact physical measurement. For SQLite, extend the existing offline VFS probe to classify subjournals/sorters and compare **OS counters too**, then replay the actual refresh API on a copied source/store. A production VFS wrapper is a broader change requiring lifecycle/locking review, not a small patch to ship from this investigation.

A complete explanation of the original rate still needs a production-shaped, large-root workload trace with retained per-file and per-phase evidence. The two-file production refresh controls below did **not** reproduce the missing multi-GiB traffic. `sudo -n true` was unavailable (`sudo: a password is required`), so fs_usage was not used. A future authorized fs_usage run should target an isolated daemon, not PID 11567. The supplied interposer is an alternative for payload attribution in an isolated process, not a substitute for per-file kernel physical accounting.

## Reproduction commands

```sh
cargo build -p agent-file-tools --bin aft --locked
clang -dynamiclib -Wall -Wextra \
  -o /tmp/aft-write-interposer.dylib \
  docs/investigations/scripts/daemon-write-interposer.c -lpthread
python3 docs/investigations/scripts/daemon-unexplained-writes.py \
  --aft-bin "$PWD/target/debug/aft" \
  --subc-bin "$HOME/.local/share/cortexkit/bin/ck-subc" \
  --probe-bin "$HOME/.local/share/cortexkit/bin/subc-probe" \
  --interposer /tmp/aft-write-interposer.dylib \
  --output-dir /tmp/aft-isolated-write-run

# INPUT contains OFFLINE SQLite-backup snapshots, never live databases.
AFT_WRITE_PROBE_DIR="$INPUT" cargo test --features test-hooks -p agent-file-tools \
  --test daemon_unexplained_writes_probe offline_database_read_writes \
  -- --ignored --nocapture --test-threads=1
AFT_WRITE_PROBE_DIR="$INPUT" cargo test --features test-hooks -p agent-file-tools \
  --test daemon_unexplained_writes_probe offline_sparse_callgraph_writes \
  -- --ignored --nocapture --test-threads=1
```

The output directory must be new. Inspect raw tool responses as well as process exit status. The first prototype run raced module registration and another waited for census before any route had initialized the database; both were discarded. The corrected final daemon run had zero tool errors. No snapshot containing user task commands/history is committed.

## Actual refresh controls and late memory-pressure hypothesis

The copied prefrontal database was paired with a disposable source clone, and only the copy's `backend_file_state.workspace_root` was re-rooted before opening the production `CallGraphStore`. The first attempt correctly triggered the concurrent-clone cold-rebuild protection and was terminated; its traffic is excluded. The corrected runs call `refresh_files_profiled`:

| Actual production API workload | Time | OS physical bytes | Ledger evidence |
| --- | ---: | ---: | --- |
| Append comments to manager_runtime.rs and dispatch.rs | 286.596 s | 819,200 during refresh | 73,728 pager bytes, plus 73,728 close estimate; structural graph unchanged |
| Repeat identical refresh | 0.005 s | 0 | No graph rewrite |
| Delete those two files from isolated clone | 250.974 s | 365,576,192 during refresh | 313,868,288 refresh + 100,425,728 checkpoint = **414,294,016**, greater than OS writes |
| Repeat deletion refresh | 0.005 s | 0 | No graph rewrite |

Deletion selected 7,573 dependent references. No large `etilqs_*` traffic appeared in this actual-API trace. Therefore the synthetic bulk-SQL control's temp-journal gap must not be presented as the production explanation. Page credits can exceed physical writes because OS coalescing/cache behavior differs from SQLite's accounting.

After these runs, the operator reported that PID 11567 had grown to **25 GB footprint, Malloc Small 23 GB dirty, 23k pageouts**, with system swap **4.7/6 GB**, and was restarted at **18:02 UTC**, replacement PID **96581**. These are operator-provided observations, not measurements made by these probes. **Untested hypothesis:** compressor/swap pageout I/O could contribute to the process counter without passing through any AFT file writer.

The exact census field is `rusage_info_v4.ri_diskio_byteswritten`, fetched in `crates/aft/src/process_io.rs:17–33`. A quick public-source check found XNU [`osfmk/kern/bsd_kern.c`, `fill_task_io_rusage`](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/bsd_kern.c): lines 1301–1302 populate this as `task->task_io_stats->total_io.size - task->task_io_stats->disk_reads.size`. That function **does not document a swap/pageout exclusion**, but it also does not establish which task is charged for compressor backing-store writes. The downloaded public `main` is not a verified source match for this installed macOS kernel. Consequently this investigation **cannot establish whether this field includes anonymous swap pageouts charged to the originating process**. Compression in memory alone is not disk I/O; backing-store writes and charging policy are the relevant questions.

No deliberate whole-machine memory-pressure experiment was run: allocating enough to force swap on this shared build/agent host could disrupt unrelated workers and would still require a control to distinguish file-backed and anonymous pageouts. Recommended next test is on a disposable isolated machine: compare a low-memory no-file-write allocator with an incompressible anonymous-memory workload under externally induced pressure, record this exact field plus pageout/compressor/swap counters, and show whether only the pressured condition raises disk writes. Meanwhile compare the new daemon's unexplained rate with its footprint/pageout trend at matched workload. A rate drop after restart is useful correlation, not proof of swap attribution or of a particular AFT writer.

## Free-space recommendation

The copied aft.db reports `PRAGMA auto_vacuum = 0` (**NONE**). Running `PRAGMA incremental_vacuum` now would not reclaim the 692.6 MB freelist. Switching an existing NONE database to INCREMENTAL requires a full `VACUUM` conversion to create pointer-map metadata; merely setting the pragma is not enough. See [SQLite auto_vacuum](https://www.sqlite.org/pragma.html#pragma_auto_vacuum).

**Do not add periodic VACUUM as a response to this write incident.** The free pages are reusable reserve, retention is working, and compaction itself moves/writes pages. If disk-capacity recovery is worth a one-time maintenance rewrite, schedule it explicitly with exclusive application ownership, adequate scratch capacity, and a verified backup. Prefer an offline maintenance window for conversion; stop competing AFT/CLI users, use one SQLite-owned connection, checkpoint/close normally, and never raw-replace an open database. If incremental mode is adopted for new databases or after that migration, run small bounded `incremental_vacuum(N)` steps only during idle maintenance under the **existing connection's mutex**, with bytes/duration instrumentation and a free-page threshold. Validate SQLite's actual reclaimed-page behavior under WAL before choosing a budget.

This respects the descriptor rule: issue PRAGMAs through the already-owned SQLite connection, not a second raw `File::open`/read/close of the database or sidecars in that process. A separate process using SQLite's backup API was safe for this read-only analysis, but that does not make a second live maintenance writer operationally desirable. Do not auto-vacuum immediately after every retention transaction; that trades reusable pages for repeated write amplification.

## Verification and limits

- Current-checkout daemon build passed; Rust probe target typecheck passed.
- Offline production readers, synthetic sparse writes, actual refresh and deletion controls all passed their execution checks. These are **measurement probes, not regression tests proving a fix**.
- The committed Python driver was rerun end to end: PID 15558, 74.609 seconds, physical delta 6,668,288 bytes (745,472 → 7,413,760), all tool calls successful. This corroborates the earlier measured run, not an exact byte-for-byte deterministic total.
- `clang -dynamiclib -Wall -Wextra -Werror` passed for the interposer; Python syntax compilation passed.
- Scoped AFT inspect timed out at `lsp_quiescence` after 120 seconds; Cargo is the authoritative Rust gate. The tool transport restarted during the long deletion probe; its retained output contains the successful result.
- No production fix or mutation proof is claimed. The operator authorized delivery of this bounded report and probes with the original production cause explicitly unresolved.

For the actual refresh control, prepare a disposable `prefrontal-root` clone **inside** INPUT next to the offline `callgraph.sqlite`. The test deliberately edits that clone; `AFT_WRITE_PROBE_DELETE=1` instead deletes its two selected files. Restore/reclone the disposable source before repeating the deletion variant. All store mutations use a fresh temporary copy. Set isolated `AFT_STORAGE_DIR` for these direct-library tests too:

```sh
AFT_STORAGE_DIR="$INPUT/storage" AFT_WRITE_PROBE_DIR="$INPUT" \
  cargo test --features test-hooks -p agent-file-tools --test daemon_unexplained_writes_probe \
  offline_production_callgraph_refresh -- --ignored --nocapture --test-threads=1
# Add AFT_WRITE_PROBE_DELETE=1 for the deletion variant.
```

For per-file tracing, build the test with `--no-run`, then launch only that produced test executable with `DYLD_INSERT_LIBRARIES` and `AFT_IO_PROBE_LOG` set. Do not inject the production daemon or the Cargo/compiler process. Interposer counters include setup/backup unless snapshots are differenced; use the production probe's before/after process/census lines for operation comparisons. The interposer's first-temp-file backtraces are diagnostic and can perturb timing; wall times are not throughput benchmarks.
