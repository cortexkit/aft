# Disk-write hunt (September 2026)

## Summary

This audit starts at `427895e45278924df01fee8353fb36806ec2c1ae`, after the callgraph refresh fixes. Numbers below are **MiB (2^20 bytes), not decimal MB**. Physical process counters include filesystem writeback, sidecars and synchronization; WAL payload is reported separately. Artifact sizes alone are not measurements of write amplification.

| Finding / isolated operation | Physical MiB | Logical MiB | WAL MiB | Disposition |
| --- | ---: | ---: | ---: | --- |
| Re-materialize an existing view from its unchanged real manifest | 517.519 | 974.103 | 258.938 | O(corpus); file incremental materialization follow-up. Existing assembly already skips an identical manifest. |
| Persist the AFT family's semantic snapshot | 145.469 | 150.438 | n/a | O(corpus); file delta persistence follow-up, not a format change here. |
| Read largest retained bash session, 28,451 rows, SQL ordering | 54.141 | 118.801 | 0 | Disk-backed sorter; targeted ordering fix follows. |
| Read largest session's backup history, 4,631 rows | 2.461 | 3.164 | 0 | Smaller disk sorter; file follow-up. |
| Removal-health read | 1.863 | 2.410 | 0 | Activity aggregation temporary storage; not a write transaction. |
| Fold/prune 500 expired compression events | 0.004 | 1.508 | 1.477 | Bounded maintenance, lifetime totals preserved. Checkpoint adds 5.824 physical MiB. |
| One-time compression-retention schema migration | 22.586 | 26.972 | not sampled separately | Builds the date/range index once, not on every sweep. |

The first three are the largest measured operations, **not an attribution of all 42.9 GB to these operations**. The original daemon's captured log does not contain view-publication markers, and several persistence paths lack per-write success markers. Rates cannot responsibly be inferred from artifact mtimes or from the number of roots.

## Capture and reproducibility

- Captured `aft.db` using SQLite online backup, read-only source URI, at 2026-09-12 09:55 UTC. Source was 1,634,738,176 bytes; capture contains 430,217 compression events and 383,885 bash tasks.
- Copied `semantic/90ff783f3f4c5cf2/semantic.bin` (152,533,650 bytes), `symbols/90ff783f3f4c5cf2/symbols.bin` (8,689,003 bytes), and `index/90ff783f3f4c5cf2/cache.bin` (23,675,940 bytes).
- View input: `views/0f3900af641f5248/derived.sqlite`, `manifest-6-c3d6ca11eca18e25d2e58e52629921215cdea1d4f23ce9278941c1a13faa95d1.json`, and `blobs/aa69d52ef2dcad4d/callgraph.sqlite`. All 5,056 manifest callgraph keys existed in the copied blob store (7,570 payloads). Derived input contained 4,921 files, 41,311 nodes, 299,425 refs and 65,459 edges.
- Inspect sample: `8f93aad09f2535d0.g1783585342962962000.95932.sqlite`, 9,055 contributions and five aggregates.
- Read-only opening the live blob store failed with SQLite `SQLITE_CANTOPEN` during the first query. These view/inspect inputs were main-file copies, not online backups; the blob directory had no WAL at capture. `PRAGMA quick_check` passed on every copied database. The manifest closure check makes the measured view self-contained, but this is not a claim of an atomic multi-artifact production snapshot.
- No benchmark operates on the live input files. `crates/aft/src/disk_write_hunt.rs` creates another temporary directory underneath the input directory, uses SQLite's backup API for the measured databases, and re-roots semantic paths to a temporary non-Git project. It calls the real persistence/pruning/materialization functions in a standalone AFT test process, not the daemon. No embedding service is needed.

Prepare a directory containing `aft.db`, `semantic.bin`, `derived.sqlite`, `callgraph.sqlite`, `manifest.json`, and (for the query inventory) `inspect.sqlite`, then run on macOS:

```sh
AFT_DISK_HUNT_INPUT="$PWD/target/disk-write-hunt" \
  cargo test -p agent-file-tools --lib bench_disk_write_hunt -- --ignored --nocapture
AFT_DISK_HUNT_INPUT="$PWD/target/disk-write-hunt" \
  cargo test -p agent-file-tools --lib bench_disk_write_query_plans -- --ignored --nocapture
```

These are debug-build I/O probes, not throughput benchmarks. The expensive view operation took 426 seconds in the recorded run. The probes use Darwin `proc_pid_rusage(RUSAGE_INFO_V4)`, reject an unavailable counter, reset WAL before sampling, keep a connection alive while observing WAL, and map WAL frame page numbers through `dbstat`. Passive checkpoint counts are not always newly written pages: the view's writer already autocheckpointed, so the following passive checkpoint returned `(0,65902,65902)` with **zero additional physical/logical writes**. Its 257.430 MiB page payload is already included in the materialization counter. The compression pass's subsequent checkpoint returned `(0,376,376)` and wrote 1.469 MiB of main-file payload.

## Connection settings

Defaults were checked with AFT's bundled **SQLite 3.46.0**, not inferred from `/usr/bin/sqlite3`. The system Python SQLite reported synchronous=1 and cache_size=2000; the bundled library's fresh readers report synchronous=2 and cache_size=-2000. `synchronous`, cache and temporary-store settings are per connection, not persisted database configuration.

In this table, **D** means the bundled default: synchronous FULL (2), wal_autocheckpoint 1,000 pages, page_size 4,096, cache_size -2,000 KiB, temp_store 0 (compile default), mmap_size 0. A copied existing file retains its page size and journal mode; a fresh unconfigured file starts with rollback journaling. No blanket durability-setting change was made.

| Opener / users | journal_mode | synchronous | autocheckpoint | page_size | cache_size | temp_store | mmap_size | Explicit checkpoint / hot rebuild |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `db::open` / `apply_pragmas`: bash_tasks, compression_events, watches, state, backups, standing roots, GitHub cache, alert records | WAL | NORMAL | D | D | D | D | D | No per-transaction manual checkpoint; migrations run only on version change |
| `db::open_readonly` / removal health | inherited | D | D | inherited | D | D | D | None; busy timeout only |
| `inspect::cache::configure_connection` | WAL | NORMAL | D | D | D | D | D | No per-transaction manual checkpoint |
| inspect read-only opener | inherited | NORMAL | D | inherited | D | D | D | query_only=ON; cannot eliminate SQLite temporary sorting by itself |
| inspect in-memory cache | MEMORY | NORMAL | D | D | D | D | D | no durable artifact |
| `views::configure_connection` / pointer publication and durability helpers | WAL | NORMAL | D | D | D | D | D | PASSIVE and fsync at publication boundaries |
| `blob_store::configure_connection` | WAL | NORMAL | D | D | D | D | D | publication helpers checkpoint referenced blob DBs |
| `alias::AliasStore::open` | WAL | NORMAL | D | D | D | D | D | publication helper synchronizes alias DB |
| `path_status::open_at` / derived DB | inherited (DELETE when fresh) | D | D | D | D | D | D | creates schema, no connection pragma policy |
| `materialize_manifest_view_database` / derived writer and blob reader | inherited | D | D | D | D | D | D | deletes/reinserts files/nodes/refs/edges; writer autocheckpoints; publication later checkpoints |
| `migration::create_publication_artifacts` | WAL | NORMAL | D | D | D | D | D | one-time legacy import artifacts |
| view closure readers / migration closure / GC plane opener | inherited | D | D | D | D | D | D | GC deletes over-budget unreferenced payloads; no VACUUM |
| `BuildDeathBreaker::open` | WAL | NORMAL | D | D | D | D | D | schema initialization; no manual checkpoint |
| callgraph refresh writer (prior investigation) | WAL | NORMAL | 4,000 | D | -8,192 | D | D | bounded idle checkpoint, at most once/root/minute |

The requested stores have no hot-path VACUUM, ANALYZE or REINDEX. AFT schema migration V2 historically uses GROUP BY to deduplicate compression identities, and V9 rebuilds watches for the task foreign key; neither runs on ordinary opens of a current database. New V10 builds the retention range index once. Closing the last SQLite connection can checkpoint even where no manual checkpoint call is present.

## Ordered-read inventory

`USE TEMP B-TREE` is not by itself a large-write finding. The amount of qualifying data, selected payload width, and whether the connection is repeatedly opened matter. EQP probes used real copied tables. SQL projections in the diagnostic helper use `SELECT *` for task/backup rows; the production functions select the corresponding row fields explicitly.

| Read / source | Qualifying rows in capture / bound | Plan / disposition |
| --- | ---: | --- |
| `db/bash_tasks.rs::list_bash_tasks_for_session` | largest session 28,451 | session-status index + **USE TEMP B-TREE FOR ORDER BY**; 54.141 physical MiB/read |
| `list_replayable_bash_tasks_for_project` | selected project 0 replayable; table 383,885 | project-lookup index + temp ORDER BY; predicate may select large undelivered history |
| `list_bash_tasks_by_id` | missing task probe 0 | harness prefix scan + temp ORDER BY; 6.9 seconds, zero physical writes in this probe |
| `find_bash_task_for_project` | LIMIT 1 | `(harness,project_key,task_id,started_at DESC)` covers filtering/order |
| `db/bash_watches.rs` session/task/scanning lists | entire table 52 | created_at + task/watch/session sort keys not covered by PK; small sorts, not a corpus-sized spill |
| `db/backups.rs` path/order and newest-order queries | per path LIMIT / per session | `idx_backups_session_path_order` or `idx_backups_session_order` cover order_blob |
| backup session file_path/order_blob projection | chosen session 4,631 | session-path index + **USE TEMP B-TREE FOR ORDER BY**; 2.461 physical MiB/read |
| `db/removal.rs` two activity GROUP BY projections | seven-day task window; backups table 148,211 | activity materialization + temp grouping; full operation 1.863 physical MiB |
| removal undo-history session GROUP BY | 148,211 input, 5,612 groups | covering session-path-order index; zero measured writes in isolated grouping |
| inspect contribution load / metadata / contribution hash ORDER BY file_path | 9,055 across categories | PK `(category,project_key,file_path)` covers exact-prefix order |
| inspect latest aggregate ORDER BY generated_at DESC LIMIT 1 | <=1 per category/project | PK uniqueness bounds lookup before ordering |
| `alert_records::FIVE_TURN_RESOLUTION_QUERY` | tables absent from captured aft.db | offline query; producer/episode join indexed but final ordinal/fingerprint order not covered; potential temp sorter, no live row estimate |
| `build_breaker.rs` suspended-root/domain order | keyed by root/domain/corpus | small control state; file not part of aft.db capture, no measured count |
| `gc::sweep_plane` ORDER BY created_at_ms,full_key | 7,570 callgraph payload rows | no age-order index; temp sort of keys/lengths/timestamps, not payload bytes |
| views/migration closure point reads; standing roots/state/GitHub cache | point reads or unordered lists | no ORDER BY/GROUP BY/DISTINCT corpus projection in these paths |

The activity query currently compares the same millisecond cutoff with `backups.created_at`, whose stored timestamps are seconds. That pre-existing reporting discrepancy is recorded, not changed by this disk-write patch.

## Whole-artifact rewrites and observed frequency

Frequency window: captured `aft-62424.log`, **2026-09-12 00:19:07–09:16:39 UTC (8.9589 hours)**. These are process-log observations, not rates extrapolated from the entire storage tree. An absent success marker is **unknown**, not zero work.

| Writer | Complexity and trigger | Logged events / rate in this window |
| --- | --- | --- |
| semantic `write_to_disk` | every admitted persistence serializes the entire snapshot, temp + fsync + rename; completed build/refresh, subject to epoch/fingerprint/generation guards | `semantic index persisted:` 16, **1.786/hour**, 1,331.013 MiB serialized across all logged saves |
| trigram `SearchIndex::write_to_disk` | streams merge of full base+delta to cache.bin, resets delta after successful replacement; cold builds also spill sorted segments | no one-to-one success marker; save rate unknown. 161 `transient search cache sweep` lines (17.971/hour) are cleanup attempts, **not** builds |
| symbol `symbol_cache_disk::write_to_disk` | serializes all cached entries, fsync + atomic rename; configure prewarm/persist skips unchanged cache and borrow-only roots | `persisted symbol cache:` 17, **1.898/hour** |
| view assembly / derived.sqlite | changed manifest triggers full joined materialization plus indexes, even when most blob keys are reused; exact same manifest already elided at base | no HEAD-reuse/publication markers in this captured process log; cannot infer a publication rate |
| immutable semantic/callgraph blob stores | keyed payload insert is O(new blobs), existing keys reused; GC and derived joins can still scan the corpus | no per-put count inferred from root count |
| inspect | contribution upserts/deletes and aggregate replacement; full result publication loops supplied contributions; per-worktree scope keys inhibit cross-checkout reuse | no one-to-one commit success marker; 198 callgraph-snapshot lines (22.101/hour) are reads, not inspect commits |
| backup snapshots | content files written only if absent; metadata for the retained <=20-entry per-file stack rewritten; DB mirror and directory fsyncs | no per-edit success marker; three backup cleanup messages are not edit counts |
| named checkpoints | full selected file contents captured, not a patch delta; blob files and named metadata fsynced; previous blobs pruned | `checkpoint created:` 4, **0.446/hour**; file count differs by checkpoint |
| logs | append; rotate at 32 MiB, not whole-log rewrite on each event | hourly maintenance; live process history retained under configured age/size policy |

Semantic hourly success counts (UTC): 00=8, 01=1, 03=3, 06=1, 07=1, 08=1, 09=1. Symbol counts: 00=8, 01=2, 03=3, 06=1, 07=1, 08=1, 09=1. This startup-heavy distribution is not a steady-state per-root refresh frequency.

## Retention and accumulation

The filesystem census counts **files, not unique project families**. It includes legacy/orphaned generations; source main-file sizes omit WALs and other sidecars.

| Store | Captured rows / bytes / timestamp range | Retention and remaining risk |
| --- | --- | --- |
| bash_tasks | 383,885 rows; table payload 898.344 MiB; started_at 2026-05-20–2026-09-12, milliseconds | delivered terminal rows can be deleted by lifecycle cleanup; not a blanket 30-day SQL sweep |
| compression_events | 430,217 rows; table payload 252.414 MiB, identity index 47.457 MiB; same date range, milliseconds | 281,442 older than 30 days, zero protected by a live task in this capture; new bounded fold/prune below |
| backups | 148,211 rows; table payload 85.293 MiB; created_at 1779252197–1789206935, seconds | per-session/path depth 20; disk lifecycle sweeps exist, no independent raw SQL age deletion in this patch |
| bash_pattern_watches | 52; created_at 1784930860921–1788588160631 ms | task FK cascades deletion; cannot discard pending notifications by age alone |
| harness_state / host_state | 1 / 0; harness updated_at 1783474798495 ms | keyed replacement, no event history |
| standing_roots / freshness | 18 / 21 | explicit root ownership; not arbitrary TTL data |
| github_read_cache | 37; fetched_at_ms 1788150789455–1789206114399 | hard-expiry eviction API exists; no production caller found in this source snapshot; reads enforce freshness separately |
| alert rendered/disappearance records | absent from captured aft.db | append/dedup episode identities; no pruning in module; future unbounded history risk |
| build-breaker records/attempts | separate breaker file, no captured count | keyed state and attempt IDs; no pruning in module |
| inspect | 1,054 SQLite files, 23,079.9 MiB total; sampled scope has 9,055 contributions, five aggregates | scope sweep protects live keys/markers; 14-day age floor, five-second pass budget. One logged pass removed 17 scopes / 304,454,259 bytes |
| views | 27 derived.sqlite files / 775.262 MiB; 30 manifest files / 28.411 MiB | manifest generations and reader pins protect referenced blobs; exact-manifest no-op is already present |
| blobs | 12 SQLite main files / 4,113.410 MiB | byte-budget GC with 15-minute age floor and current/retained/pinned manifest reference protection; publication/configure-triggered sweep |
| semantic | 396 snapshots / 4,989.021 MiB | root-family orphan sweep; whole snapshot per successful save |
| trigram | 713 cache.bin files / 1,042.135 MiB | configure orphan sweep + bounded transient-cache cleanup; orphan temporary files can remain after failed/crashed builds |
| symbols | 4,028 snapshots / 2,872.633 MiB; largest single file 1,548.464 MiB | root-family accumulation deserves follow-up; AFT-family sample itself only 8.287 MiB |
| checkpoints | not byte-censused | 20 names/session, 14-day durable retention, cleanup/hydration sweeps and unreferenced-blob pruning |
| logs | process + plugin rotations | 32 MiB rotation and hourly maintenance, separate from SQLite write counters |

### Compression retention mechanism

Schema V10 introduces a `(created_at,id)` range index, a durable scan cursor, and lifetime rollups keyed by `(harness,project_key,session-nullness,session_id)`. NULL and empty sessions remain distinct. The maintenance hooks in standalone and daemon ticks schedule an off-loop worker at most once per process/minute; it uses `try_lock` on the already-open DB rather than opening a new database or blocking the request loop waiting for the mutex.

One IMMEDIATE transaction examines at most **500** age-qualified rows after the cursor, checks each task through the existing composite primary key, groups counters in Rust, deletes eligible raw rows, adds totals to rollups, and advances the cursor. The cursor wraps after the last old row so a formerly live task is reconsidered. It does not repeatedly scan an unbounded protected prefix. Retain the highest raw ID even when old: the existing warm-cache insertion watermark remains stable across pruning. Project/session readers use one SQL statement for rolled-up plus raw totals, avoiding mixed pre/post-prune snapshots.

Parent-authorized policy: keep 30 days of raw identity suppression; additionally retain old events while a matching non-terminal task can still emit compression. NULL task IDs are age-only. No permanent one-row-per-event identity tombstone is added. Older terminal identities cease participating in INSERT OR IGNORE once pruned. This is an explicit history policy, not a claim that the database API rejects arbitrary synthetic replays forever.

Pruning is **not VACUUM**: free pages remain in the main file for reuse. It does not immediately shrink a 1.55 GiB file or prove a reduction in all daemon physical writes. The measured first batch freed/rebalanced pages while appending 376 WAL frames. Of those, 280 belonged to `idx_compression_event_identity`, 39 to freelist/unmapped pages, 17 each to the two session indexes, 11 to the project index, and the remainder to tables/range index/schema. A wholesale delete would create a much larger maintenance burst; the small pass deliberately trades cleanup latency for bounded work.

Bash-task pruning is deferred. There are 261,091 old terminal rows; 260,719 are also completion-delivered and watch-free. Those are **upper bounds**, not safe deletion counts. The required predicate additionally needs the on-disk task layout to be gone and no pending lifecycle tombstone. `db/` does not own those filesystem/lifecycle facts; duplicating their interpretation in a SQL sweep would risk losing a completion. The follow-up belongs with `bash_background` lifecycle cleanup, which can prove layout absence, acknowledgment, no watch and no tombstone together.

### Regression proof

`retention_preserves_lifetime_totals_and_live_task_identity` inserts 510 old events across projects and NULL/empty/named sessions, a live task and a boundary-age event. It checks 499 then ten deletions, equality of cold and warm lifetime totals, preserved live/recent dedup, cursor wrap after task completion, and subsequent local insert cache updates. `retention_rollup_failure_rolls_back_raw_deletes_and_cursor` injects an aborting rollup trigger. `retention_selection_is_indexed_and_keeps_the_watermark` checks the real selection plan and the retained highest ID.

Controlled mutations were staged safely before alteration and restored from the index afterward; every mutation had a nonempty working diff while applied and an empty working diff after restore. Captured failures:

```text
NON-VACUITY BREAK: clear grouped totals before the durable fold
retention_preserves_lifetime_totals_and_live_task_identity ... FAILED
left:  events: 2, original_tokens: 117, compressed_tokens: 49
right: events: 256, original_tokens: 25517, compressed_tokens: 10209
0 passed; 1 failed (only this exact test selected)

NON-VACUITY BREAK: bypass live-task protection
retention_preserves_lifetime_totals_and_live_task_identity ... FAILED
left: 500; right: 499
0 passed; 1 failed (only this exact test selected)

NON-VACUITY BREAK: omit the retention range index
retention_selection_is_indexed_and_keeps_the_watermark ... FAILED
SCAN e
CORRELATED SCALAR SUBQUERY 1
SEARCH b USING INDEX sqlite_autoindex_bash_tasks_1 (...)
USE TEMP B-TREE FOR ORDER BY
0 passed; 1 failed (only this exact test selected)
```

## Filed follow-ups (not silently fixed)

1. Incremental derived-view materialization: retain immutable generation semantics while avoiding O(corpus) row/index replacement for changed manifests. Measured 517.519 physical MiB/op. Do not remove publication durability syncs to make this number look smaller.
2. Semantic base+delta persistence or coalesced snapshots: measured 145.469 physical MiB/save; 1,331 MiB of serialized successful saves in the captured daemon window. Requires a reader/format/lifecycle design, not an unconditional `temp_store=MEMORY` switch.
3. Backup-history sorter and removal-health aggregation: measured 2.461 and 1.863 physical MiB/read; retain current ordering/count contracts in separate fixes.
4. Symbol-cache largest-family and orphan census, trigram compaction rate, inspect full-result write profiles: artifact sizes identify exposure but do not measure per-operation amplification. Add per-success byte markers before claiming a rate. The 1.55 GiB largest symbol file is not represented by the much smaller AFT-family snapshot.
5. Bash lifecycle deletion predicate and alert/breaker history retention as described above. Compression rollups themselves remain O(number of historical sessions), not O(number of events).

## Verification for the retention commit

- `cargo test -p agent-file-tools --lib`: 3,149 passed, 21 ignored.
- `cargo test -p agent-file-tools --test integration`: 1,708 passed, 12 ignored, 11 failed. Ten callgraph tests require a writable checkout/store and fail with `callgraph_unavailable` here; all ten reproduced after restoring the implementation files to the unchanged base, in a 53-test callgraph run (43 passed). The eleventh, `standalone_ndjson_polls_cold_navigation_off_the_input_loop`, passed when rerun alone on the fixed code (parallel-suite deadline sensitivity).
- `cargo test -p agent-file-tools --test integration tool_call_parity_test`: all 12 passed, including byte-envelope parity.
- `RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --all-targets`: passed.
- `RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --target x86_64-pc-windows-gnu --all-targets`: passed.
- Both ignored measurement probes passed. Scoped AFT inspection completed but had no authoritative Rust diagnostics; Cargo checks, not the diagnostic summary, are the compile gate.
