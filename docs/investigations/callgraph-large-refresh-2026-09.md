# Large incremental callgraph refresh vs cold build (September 2026)

Every claim below is marked **[measured]** (a number from a run listed under
[Reproduction](#reproduction)) or **[inferred]** (read from code or derived
from measured numbers, without a direct measurement). No product code was
changed. The harness is a test binary kept outside the build
(`docs/investigations/scripts/callgraph-large-refresh-bench.rs`).

## The live specimen

On 2026-09-23 a `git pull` in a local oh-my-pi checkout (about 7,600 files)
changed ~1,650 files. The shared daemon (pid 73010) then logged:

```text
13:46:17 legacy_callgraph_refresh root=.../oh-my-pi total_physical_bytes_written=2215104512
13:46:17 watcher drain unit exceeded 5s: phase=callgraph path=... (+1348 paths) elapsed=550245ms
13:47:51 legacy_callgraph_refresh root=.../oh-my-pi total_physical_bytes_written=1916170240
13:47:51 tier2 dead_code: refreshed callgraph store ... for 1483 watcher path(s): changed=1483 refreshed_own=157
13:48:03 perf tier2_callgraph_snapshot: source=callgraph_store files=5453 exports=21155 edges=668795 entry_points=126 ms=241013
```

Its phys_footprint rose to 14–16 GB (RSS 5–9.6 GB), almost all in the malloc
small zone, and fell back to 5.7 GB at 13:47:57. The questions were: what
the refresh costs against a cold build, where the memory and the bytes go, at
what batch size incremental stops paying, and what the 550 s unit blocked.

## Summary

- **The live specimen reproduces offline.** Refreshing the exact `git pull`
  transition (`e1a86ce4f9` → `21659c1feb`, 1,650 git paths, 1,488 after the
  refresh worker's language filter) took **543 s**, peaked at **11.5 GiB live
  Rust heap / 12.4 GiB phys_footprint**, wrote **1.91 GiB physical** and ended
  with 4.0 GiB footprint the allocator had not handed back. The daemon logged
  550 s and ~2.2 GB. [measured]
- **A cold build is cheaper in memory, not in CPU or bytes.** A cold build of
  the same commit on the same machine used 393–440 s CPU against the
  refresh's 254 s, a 1.6–2.0 GiB heap peak against 11.5 GiB, and wrote
  **26–27 GiB physical**, ~14× the refresh. [measured] The cold build is
  multi-threaded and the refresh is not, so with idle cores the cold build wins
  on wall time (330 s wall for `df624f56b0` vs the refresh's 543 s, though those
  two runs had different machine load). [measured walls; the comparison is
  inferred] The earlier estimate of ~194 s for a cold build (extraction of 20k
  files after the quadratic-extraction fix) is not
  comparable: these runs time the whole cold build (walk, extraction,
  resolution, publication), and the stages were not timed separately.
- **Memory is two full copies of every changed file's extract plus one per
  dependent file, and each extract is dominated by one field.** Every
  call/value reference in an extract carries its own deep copy of the file's
  import-dependency set (`RawRef.dependencies`, `mod.rs:9833`), and most
  entries in that set are module-path candidates, not indexed files
  (1,742 of the 1,893 entries in
  `packages/coding-agent/src/session/agent-session.ts`). At the plateau of the
  specimen: changed-file extracts 5.06 GiB, the `extract.clone()` into
  `caller_extracts` 4.82 GiB, dependent-file extracts 1.79 GiB, selection sets
  0.14 GiB. Of those, 10.4 GiB is per-ref `BTreeSet<String>` clones.
  [measured]
- **Cost follows fan-in, not batch size.** 51 changed files already selected
  201k dependent refs in 1,020 importing files and cost 4.8 GiB heap, 53 s CPU
  and 1.1 GiB written. The 1,488-file batch cost 2.4× the heap and 4.8× the
  CPU of the 51-file one, not 29×. [measured]
- **Crossover.** In memory the incremental refresh is worse than a cold build
  at every measured size (from 51 files, 0.8% of the repo). In bytes written
  it is better at every measured size, including 3,989 changed files (63% of
  the repo). In CPU time it only approaches the cold build at that largest
  batch: 404 s against 440 s (see [the crossover table](#4-crossover)).
  [measured]
- **~2 GB per batch is the WAL plus its checkpoint.** The specimen changed
  ~1.17 M rows (refs, edges, dispatch hints and their seven-plus indexes),
  touching 204k distinct pages, 60% of the 338k-page database. Those pages go
  once to the WAL (803 MiB) and once to the main file at the commit-time
  autocheckpoint. [measured page and row counts; attribution of physical bytes
  inferred]
- **The two ~2 GB log lines are not two ~2 GB writes.** `legacy_callgraph_refresh`
  byte fields are process-wide deltas (`views/io.rs:1-3`), and the tier-2
  dead-code refresh ran in the same process while the worker's refresh was
  still running. A second refresh of the same paths is a 0.43 s no-op with
  zero bytes written. [measured no-op; overlap inferred from timestamps and
  code]
- **`watcher drain unit exceeded 5s: phase=callgraph` is not an executor drain
  unit.** It is `RefreshWorkerWatchdog` on the single process-wide
  `aft-callgraph-refresh` thread (`callgraph_store/mod.rs:1582-1609`), which
  reuses the executor watchdog's wording. The executor only enqueues
  (`runtime_drain.rs:2306-2313`). The 550 s did not block the executor's watcher
  drains; it blocked every other root's callgraph refresh, which queue FIFO
  behind the active batch on that one thread. [inferred from code]
- **Side finding, needs its own look: incremental and cold resolve
  differently.** After the specimen refresh the store had 278,815 edges; a cold
  build of the same commit has 372,187. 94,472 call refs are `resolved` in the
  cold build and `unresolved` after the refresh, for example `describe`, `it`
  and `expect` in `packages/tui/test/editor.test.ts`, which the cold build
  resolves to `packages/tui/src/autocomplete.ts`. The gap grows with the share
  of the repo the refresh re-resolves: after the 3,989-file refresh only
  90,945 refs are `resolved` against the cold build's 199,704 (264,230 vs
  372,187 edges). This looks like a cold-build
  over-resolution rather than an incremental loss, but it was not diagnosed.
  Either way, falling back to a cold build changes query results. [measured
  counts; direction of the bug not established]
- **Side finding: the cold build writes ~19× the database size.** 26–27 GiB
  physical for a 1.4 GB store, at every commit measured. Not attributed here.
  [measured]

## Reproduction

Machine: Apple M5 Max, 18 cores, 128 GiB, macOS 27.0; rustc 1.98.1; AFT at
`f9f25a2cf` (v0.57.2). The machine was shared with other build workers and the
1-minute load average ranged from ~50 to ~460 during the runs, so **wall time
is noisy**. CPU time (`getrusage` user+sys) is the stable time measure; the
refresh is single-threaded and the cold build is not, so a cold build's wall
time drops below its CPU time when cores are free (330 s wall / 393 s CPU for
`df624f56b0`).

Isolation: the oh-my-pi checkout was cloned into scratch, and every run was a
standalone test process with its own `AFT_STORAGE_DIR`, `XDG_DATA_HOME` and
`XDG_CACHE_HOME`. No daemon was involved and the live `~/Work/OSS/oh-my-pi`
was only read by `git clone` and `git reflog`.

```sh
git clone --no-hardlinks ~/Work/OSS/oh-my-pi /private/tmp/lrb/omp
cp docs/investigations/scripts/callgraph-large-refresh-bench.rs \
   crates/aft/tests/callgraph_large_refresh_bench.rs
cargo test --profile stage -p agent-file-tools \
   --test callgraph_large_refresh_bench --no-run
```

Each measurement is one process (`run.sh <label> <mode> <store> [paths]`):

```sh
BIN=target/stage/deps/callgraph_large_refresh_bench-<hash>
env XDG_DATA_HOME=/private/tmp/lrb/env/data XDG_CACHE_HOME=/private/tmp/lrb/env/cache \
    AFT_STORAGE_DIR=/private/tmp/lrb/env/storage \
    AFT_LRB_MODE=$MODE AFT_LRB_STORE=$STORE AFT_LRB_ROOT=/private/tmp/lrb/omp \
    AFT_LRB_PATHS=$PATHS AFT_LRB_OUT=/private/tmp/lrb/out/$LABEL \
    "$BIN" --ignored --nocapture --test-threads 1 large_refresh_bench
```

- `cold`: `CallGraphStore::cold_build_with_lease_chunked(store, root, &[], 100)`,
  the daemon's call shape (empty list, the builder walks the root; chunk size
  is the config default).
- `refresh`: the store directory is an APFS clone (`cp -cR`) of the base cold
  build; the checkout is moved to the target commit; `CallGraphStore::open_ready`
  then `refresh_files_profiled(paths)`. Paths come from
  `git diff --name-only --no-renames <base> <target>` and are filtered by
  `parser::detect_language`, exactly as `process_callgraph_refresh_batch` does.
  The WAL is truncated first so its size afterwards is this refresh's.
  `AFT_LRB_REPEAT=1` repeats the refresh; `AFT_LRB_COUNT_ROWS=1` and
  `AFT_LRB_WAL_BREAKDOWN=1` give the work-count pass.
- `snapshot`: `project_dead_code_snapshot(db)`.

Memory comes from a counting global allocator in the test binary (live bytes,
peak live bytes, cumulative bytes and calls by size class; it wraps the system
allocator the daemon uses), a 200 ms sampler of `rusage_info_v4`
`ri_phys_footprint`/`ri_resident_size`, `ri_lifetime_max_phys_footprint`, and
`ru_maxrss`. Bytes written are `ri_diskio_byteswritten` (physical) and
`ri_logical_writes` (logical) for the process.

Attribution used a second run of the specimen under `MallocStackLogging=lite`
and `malloc_history <pid> -callTree -ignoreThreads` at 2.1 GiB (early) and at
the 9.9 GiB plateau, summarised with
`scripts/callgraph-large-refresh-malloc-tree.py`. SQL replay used
`scripts/callgraph-large-refresh-dep-selection.py` on the base store.

Commits (oh-my-pi, first-parent `main`):

| role | commit | git paths from base | refresh paths | already in base store |
| --- | --- | ---: | ---: | ---: |
| base | `e1a86ce4f9` (HEAD before the 2026-09-23 pull, from `git reflog`) | – | – | – |
| target | `757b49a4bc` | 57 | 51 | 51 |
| target | `6d31705a08` | 106 | 99 | 94 |
| target | `5a9fe9ff0a` | 305 | 194 | 156 |
| target | `015541b058` | 531 | 403 | 346 |
| target | `df624f56b0` | 809 | 676 | 494 |
| target = specimen | `21659c1feb` (HEAD after the pull) | 1,650 | 1,488 | 1,184 |
| second base | `c4da0d08e8` (HEAD before the 2026-09-18 pull) | – | – | – |
| target | `21659c1feb` from `c4da0d08e8` | 4,240 | 3,989 | 2,769 |

The base store (cold build of `e1a86ce4f9`) has 6,761 files, 91,785 nodes,
879,573 refs, 362,366 edges, 191,938 file-dependency rows, 467,970 dispatch
hints; 1.385 GB, 338,229 pages. [measured]

## 1. Specimen costs: refresh, cold build, snapshot

| operation | wall s | CPU s | heap peak GiB | max footprint GiB | max RSS GiB | physical written GiB | logical written GiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| (a) incremental refresh, 1,488 paths | 543.2 | 253.7 | 11.48 | 12.38 | 11.23 | 1.91 | 10.02 |
| (a) same refresh repeated on the refreshed store | 0.43 | 0.13 | 0.0005 | – | – | 0.00 | 0.00 |
| (b) cold build of `21659c1feb` | 825.7 | 440.1 | 1.83 | 2.41 | 2.45 | 26.98 | 26.03 |
| (b) cold build of `df624f56b0`, parallel run (wall < CPU) | 330.7 | 393.0 | 1.61 | 1.08 | 2.42 | 26.60 | 25.62 |
| (c) dead-code snapshot, refreshed store | 22.7 | 7.4 | 0.59 | 0.60 | 0.66 | 0.00 | 0.00 |
| (c) dead-code snapshot, cold-built store | 19.6 | 7.9 | 0.59 | 0.61 | 0.68 | 0.00 | 0.00 |

All rows [measured]. After the refresh returned, the process footprint was
still 3.95 GiB with 0.5 MiB of live heap: freed small-zone pages the allocator
kept, matching the daemon's fall-back to 5.7 GB rather than to its baseline.
The daemon's relief path is what returns those pages. [measured; relief
behaviour inferred from `memory.rs`]

The refresh's phase profile (`RefreshFilesProfile`) [measured]:

| phase | ms | what it does |
| --- | ---: | --- |
| dependency_selection | 199,123 | `ref_ids_depending_on` for each surface-changed or deleted file |
| ref_resolution | 133,763 | re-resolve own refs and every selected dependent ref |
| parse | 70,873 | `build_file_extract` for changed files |
| dependent_parse | 44,758 | `build_file_extract` for every file that owns a selected ref |
| row_deletes + row_inserts | 29,089 | per-file row replacement |
| commit | 18,116 | commit plus the autocheckpoint it triggers |
| method_dispatch | 11,617 | dispatch edges for refreshed files |
| index_load | 10,442 | `ProjectIndex::from_db_and_callers`: every `files` and `nodes` row |

The live `perf tier2_callgraph_snapshot ... ms=241013` is not the snapshot
cost: the snapshot alone is ~20 s. That timer starts before
`open_writable_dead_code_store` and `refresh_writable_dead_code_store`
(`inspect/manager.rs:4068-4096`), so it includes the tier-2 job's own refresh
and any wait behind the worker's write transaction. [inferred from code]

## 2. Where the memory goes

Heap over time in the specimen refresh (live heap, 200 ms sampler) [measured]:

| t (s) | live heap GiB | phase at that time (from the profile order) |
| ---: | ---: | --- |
| 63 | 0.48 | parse + dependency selection loop |
| 165 | 2.93 | same loop |
| 268 | 4.26 | loop ends ~270 s |
| 309 | 8.99 | `extract.clone()` into `caller_extracts`, then dependent parse |
| 330–515 | 9.94–9.96 | plateau: index load, own-row replacement, ref resolution, dispatch, commit |
| 536 | 4.44 | function returning; maps dropping |

Live allocations at the plateau (9.9 GiB by the counter, 11.9 GiB by
`malloc_history`, which counts allocator-rounded block sizes), by call site inside
`refresh_files_profiled_with_workspace_crate_prefix_cache` [measured]:

| call site | live | of which | scales with |
| --- | ---: | --- | --- |
| `mod.rs:4743` `build_file_extract` for changed files | 5.06 GiB, 50.0 M blocks | 4.60 GiB in `build_callable_refs` → `import_dependencies.clone()` (`mod.rs:9833`) | changed files × their call refs × their import-candidate set |
| `mod.rs:4775` `caller_extracts.insert(.., extract.clone())` | 4.82 GiB, 48.4 M blocks | 4.61 GiB cloning `Vec<RawRef>`, 4.45 GiB of it `BTreeSet<String>` subtrees | same as the row above: a second full copy |
| `mod.rs:4781` `build_file_extract` for dependent files | 1.79 GiB, 18.2 M blocks | 1.33 GiB in `build_callable_refs` | files that own a selected ref (importers of surface-changed files) |
| `mod.rs:4752-4754` `ref_ids_depending_on` + `record_dependent_refs` | 0.14 GiB, 1.2 M blocks | ref-id strings in `selected_ref_ids` and `selected_refs_by_caller` | selected dependent refs (535k) |
| index (`from_db_and_callers`), prepared statements, SQLite page cache | < 20 MiB each | – | repo size (index), fixed (cache_size 8 MiB) |

Why the per-ref copy is so large [measured on the base store]: the 1,033
changed files with call refs have 258,469 call/value refs, and the sum over
those files of (call refs × dependency-set entries) is 37.1 M. That matches
the 45.4 M blocks under `build_callable_refs` at the plateau (strings plus
B-tree nodes, ~109 bytes per block). One file,
`packages/coding-agent/src/session/agent-session.ts`, has 3,909 call refs and a
1,893-entry dependency set, of which 151 entries are indexed files; its
extract alone is ~7.4 M strings. The set is the import-resolution candidate list
(`module_dependencies` adds every `relative_module_candidates` path,
`mod.rs:15217-15222`), and every call ref gets its own copy.

What scales with the batch [measured across the sweep in section 4]:

- **per changed file:** two extracts (the parse and the clone). Cost is not
  uniform: hub files with thousands of calls and hundreds of imports dominate.
- **per dependent file:** one extract, for every file owning a selected ref.
  The selection takes every `call` ref of every importer of a surface-changed
  file (`ref_dependency_row_depends_on` returns `true` for `kind = "call"`,
  `mod.rs:15153`), so 31 surface-changed files already pulled 1,020 importer
  files into the batch.
- **per edge/ref:** only the selection sets (0.14 GiB at 535k refs). The
  project index and the row replacement are not where the memory goes.

The cold build does not hold extracts across the corpus: it stages raw refs in
SQLite and resolves in chunks, so its peak (1.6–2.1 GiB) is independent of how
many files changed. [measured peaks; mechanism inferred from
`cold_build_*` code]

The daemon's 14–16 GB (vs 12.4 GiB here) fits the tier-2 dead-code job
running its own `refresh_files` on 1,483 paths while the worker's refresh was
still in flight: both hold extracts in one process. [inferred]

## 3. Why it writes ~2 GB per batch

Work-count pass on the specimen (row-audit triggers installed; the pass is
slower and its logical bytes are inflated by the triggers, but row and page
counts are exact) [measured]:

| table | deleted | inserted | updated |
| --- | ---: | ---: | ---: |
| refs | 235,797 | 263,100 | 67,739 |
| edges | 167,741 | 84,190 | 9,753 |
| dispatch_hints | 134,107 | 148,237 | 0 |
| nodes | 17,585 | 21,562 | 377 |
| file_dependencies | 4,337 | 14,796 | 0 |
| files / backend_file_state | 77 / 77 | 303 / 303 | 1,107 / 1,107 |

That is ~1.17 M row changes. The WAL held ~204,400 distinct pages (803 MiB);
the largest owners were `refs` 45,925 pages, `idx_refs_caller_node_kind`
21,592, `sqlite_autoindex_refs_1` 18,727, `edges` 17,240,
`idx_refs_kind_caller_file` 10,525, `dispatch_hints` 10,371,
`sqlite_autoindex_dispatch_hints_1` 9,992, `idx_refs_caller_file` 9,640, and
seven edge and ref indexes at 3,800–7,800 pages each. Secondary indexes are
more than half of the pages. [measured]

Accounting for the 1.91 GiB of physical writes:

- WAL: 0.78 GiB, one frame per distinct page touched (SQLite rewrites a page's
  frame in place when the same transaction dirties it again). [measured size]
- Checkpoint into the main database: another ~0.78 GiB. The commit leaves
  >4,000 pages in the WAL, so the writer's autocheckpoint runs inside the
  refresh; a `wal_checkpoint(TRUNCATE)` right after found nothing left to copy
  (`(0, 0, 0)`). [measured checkpoint result; size inferred as equal to the WAL
  page count]
- SQLite temporary B-trees for `DISTINCT` and `ORDER BY` in the
  dependent-ref selection: replaying that query for all 1,184 changed paths
  the base store indexes (an upper bound: the refresh runs it only for the
  1,114 surface-changed and 77 deleted files) wrote 0.21 GiB physical /
  0.27 GiB logical and took 34 s. [measured]
- Sum ≈ 1.77 GiB of 1.91 GiB. [inferred]

The 10 GiB of logical writes is far above the physical 1.9 GiB. The 8 MiB
page cache (`cache_size = -8192`) cannot hold a 205k-page transaction, so
SQLite spills dirty pages to the WAL repeatedly and rewrites the same frames;
the OS page cache absorbs most rewrites. [inferred: not traced at syscall
level]

Scaling with batch size: WAL size went 328 → 417 → 450 → 598 → 667 → 803 MiB
across the sweep and 980 MiB at 3,989 files; physical bytes 1.11 → 2.04 GiB,
then 2.72 GiB. Even the 51-file batch
rewrote ~84k pages (25% of the database), because it re-resolved 201k
dependent refs across 1,020 files. [measured]

Compared with the earlier write-amplification investigation
(`callgraph-refresh-write-amplification-2026-09.md`): that fix made
graph-neutral one-file refreshes write 4 pages. It does not help here. These
batches change graph rows, and the dependent re-resolution rewrites refs and
edges in files that did not change. [inferred]

Why the live log shows two ~2 GB events: `Window::event` reports
process-wide `rusage` deltas (`views/io.rs:1-3`: "not per-root counters. Other
roots and background work can contribute"). The worker's refresh ran
~13:37:07–13:46:17; the tier-2 refresh's window ended at 13:47:51 and its
snapshot timer started ~13:44:02. The two windows overlap by minutes, so their
byte fields double-count. The second window also covers every other root's and
index's I/O in that interval. [inferred from timestamps and code]

## 4. Crossover

All refreshes start from the same base store (`e1a86ce4f9`). "Paths" is after
the language filter. [measured]

| target | paths | % of 6,761 files | surface-changed | importer files | selected refs | wall s | CPU s | heap peak GiB | max footprint GiB | physical GiB | WAL MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `757b49a4bc` | 51 | 0.8% | 31 | 1,020 | 201,275 | 182.7 | 52.6 | 4.80 | 4.68 | 1.11 | 328 |
| `6d31705a08` | 99 | 1.5% | 63 | 1,289 | 249,869 | 208.0 | 81.1 | 5.81 | 6.12 | 1.35 | 417 |
| `5a9fe9ff0a` | 194 | 2.9% | 155 | 1,407 | 279,355 | 158.2 | 99.8 | 6.22 | 6.69 | 1.43 | 451 |
| `015541b058` | 403 | 6.0% | 352 | 2,225 | 417,026 | 393.1 | 210.0 | 8.87 | 9.57 | 1.88 | 598 |
| `df624f56b0` | 676 | 10.0% | 594 | 2,700 | 481,166 | 235.6 | 186.6 | 9.84 | 10.46 | 2.04 | 667 |
| `21659c1feb` | 1,488 | 22.0% | 1,114 | 3,203 | 535,427 | 543.2 | 253.7 | 11.48 | 12.38 | 1.91 | 803 |
| `21659c1feb` from `c4da0d08e8` | 3,989 | 63% of 6,309 | 3,592 | 3,953 | 616,801 | 481.4 | 404.3 | 13.27 | 14.65 | 2.72 | 980 |
| cold build (4 commits) | all | 100% | – | – | – | 331–1,086 | 393–440 | 1.61–2.01 | 1.08–2.43 | 25.8–27.0 | – |

"Importer files" counts distinct files with a `file_dependencies` row pointing
at a changed file, from the base store. The last refresh row starts from a
cold build of `c4da0d08e8` (6,309 files; 288 s wall, 337 s CPU, 1.83 GiB heap
peak, 22.6 GiB physical written) instead of `e1a86ce4f9`. The cold-build row
covers the four commits of 6,761–6,983 files.

Crossover by resource [measured unless marked]:

- **Memory:** incremental is above the cold build at every size. Even 51
  files (0.8%) peaked at 4.8 GiB against the cold build's ≤2.1 GiB. There is no
  crossover point to tune; the incremental path's memory is set by the extracts
  of hub files and their importers.
- **Bytes written:** incremental stays 10–24× below the cold build at every
  size, including the 3,989-file batch (2.72 GiB vs ~26 GiB).
- **CPU time:** 52.6 s at 51 files, 254 s at 1,488 files (58–65% of a cold
  build), and 404 s at 3,989 files (63% of the repo), still 8% under the cold
  build of that commit (440 s; 393–434 s for the other commits of that size).
  The CPU crossover is therefore at or just above ~60–65% of the repository
  changed. Below that the refresh is cheaper in CPU, by 2–8× at or under 10%
  changed.
- **Wall time:** when it gets cores the cold build parallelises (330 s wall
  for 393 s CPU) and the refresh does not, so the wall-time crossover comes
  earlier than the CPU crossover: the 403-file and 1,488-file refreshes (393 s,
  543 s wall) already exceeded that cold build's 331 s. Load noise
  makes a precise wall-time point unreliable. [measured walls; comparison
  across differently loaded runs inferred]
- **Batch size is a weak predictor.** The 403-file batch used more CPU than
  the 676-file one. Cost follows surface-changed files, the importers they pull
  in, and hub files' extract size.

## 5. The 550 s "watcher drain unit" and what it blocked

- The line comes from `RefreshWorkerWatchdog::drop`
  (`callgraph_store/mod.rs:1582-1609`), guarding one call of
  `process_callgraph_refresh_batch` on the `aft-callgraph-refresh` thread. It
  uses the same wording as the executor's `WatcherDrainUnitGuard`, so it reads
  as an executor drain unit, but it is not one. `(+1348 paths)` is the batch
  size: one call to `refresh_files` with 1,349 paths, which is one transaction
  and one unit. [inferred from code]
- The executor's callgraph phase only filters paths and enqueues them
  (`refresh_callgraph_store_for_watcher`, `runtime_drain.rs:2287-2314`; "Opening
  and mutating SQLite belongs to the process-wide store worker, outside every
  executor lane"). Executor watcher drains were therefore not blocked by the
  refresh. [inferred from code]
- What was blocked: there is **one** refresh worker thread per process
  (`CALLGRAPH_REFRESH_WORKER`, `RefreshWorker::spawn`). It pops one root's batch
  at a time in FIFO order (`callgraph_refresh_worker_loop`). New paths for the
  active root merge into a queued batch for later; **every other root's watcher
  refreshes wait** until the 550 s batch commits. [inferred from code]
- The refresh worker does not take a cold-build limiter permit. A background
  tier-2 job does: it acquires a `Maintenance` permit before it is spawned
  and keeps it until the job ends or times out (`inspect/manager.rs:1099-1127`,
  `run_tier2_pass_with_deadline`). The dead-code job ran its own
  `refresh_files` inline under that permit (`inspect/manager.rs:4064-4096`),
  so the cold-build slot held during the live incident was that job's, for
  its ~241 s, not the worker's 550 s. [inferred from code]
- The executor's own apply loop is budgeted per unit and yields
  (`apply_watcher_path_phase`, `runtime_drain.rs:2343-2368`); for the callgraph
  phase each unit is an enqueue. [inferred from code]

## Recommendation

In order of leverage. None of these is implemented here.

1. **Stop copying the dependency set into every ref.** Make
   `RawRef.dependencies` a shared `Arc<BTreeSet<String>>` (or an index into a
   per-file table), and avoid cloning whole extracts into `caller_extracts`:
   borrow or move them instead. On the specimen that removes ~9–10 GiB of the
   11.5 GiB peak (the per-ref clones in both copies plus the second copy). It
   also shrinks the cold build's extraction heap. [inferred from the attribution
   table]
2. **Narrow dependent selection.** Selecting every `call` ref in every importer
   of a surface-changed file makes a 31-file surface change re-resolve 201k
   refs in 1,020 files, rewrite 25% of the database pages, and write >1 GiB.
   Selecting refs whose short name is in the changed export surface, or
   comparing old and new export sets first, would bound the dependent parse,
   resolution and row churn to the symbols that actually changed. Resolving
   module candidates through a memo shared across the batch would also cut the
   ~165 s of non-SQL time in dependency selection (199 s total, 34 s of it SQL
   in the replay). [inferred]
3. **Bound the batch rather than fall back to a cold build.** A cold build
   costs 26 GiB of writes and resolves differently from the incremental path
   (section "Summary", side finding). Falling back above a file-count threshold
   would trade ~9 GiB of transient memory for ~25 GiB of extra writes and
   changed results. Instead, split `refresh_files` into sub-batches with a
   memory budget (for example, flush and commit when the held extracts pass
   ~1 GiB; the counting allocator in this harness gives the number to set),
   and yield the worker between sub-batches so other roots' refreshes
   interleave. Sub-batches cost more dependency selection and re-resolution
   (hub importers get selected in several sub-batches), so pair this with (2).
   [inferred]
4. **If a threshold fallback is still wanted,** make it about fan-in, not
   file count: the number of importer files of the changed paths (computable
   from `file_dependencies` before any extract is built) predicts cost; changed
   file count does not. Use CPU time and memory as the trigger, never bytes:
   bytes favour the incremental path at every size measured. [inferred]
5. **Make the logs honest.** Rename the refresh watchdog's message (for
   example `callgraph refresh batch exceeded 5s`) so it is not mistaken for an
   executor stall, and label `legacy_callgraph_refresh` byte fields as
   process-wide in the log line. Start the `tier2_callgraph_snapshot` timer at
   projection, or log the refresh and projection times separately. [inferred]
6. **Investigate separately:** the cold vs incremental resolution divergence
   (94k call refs), and the cold build's ~19× write amplification.

## Cleanup

The scratch clone (`/private/tmp/lrb/omp`), all stores, logs and `malloc_history`
reports under `/private/tmp/lrb` were deleted after the numbers were recorded.
The harness copy in `crates/aft/tests/` was removed; only the copy under
`docs/investigations/scripts/` is committed.
