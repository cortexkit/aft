# CPU and latency amplification investigation — September 2026

## Result

**A RouteBind was rebuilding the entire fleet health census synchronously on the transport thread, in addition to the existing health worker.** This was not just a slow health-check response: `handle_control_request` submitted configure, then called `HealthRollupCache::refresh` before accepting another transport frame. Both live samples below show that path dominating the main thread. Removing this redundant refresh leaves the existing completion-triggered worker wake and periodic refresh intact.

The work-count regression exercises the actual RouteBind handler and waits for successful configure completion. It observes **one completed fleet refresh before the change, zero after**. On a standalone 36-actor fixture opening a copied production callgraph, admission fell from **6.602 ms wall / 6.756 ms process CPU to 2.309 ms wall / 2.591 ms process CPU**. This is an offline fixture measurement, not a claim that the live daemon's multi-gigabyte census takes only 6 ms.

Other expensive mechanisms remain; their measurements and follow-ups are recorded below. In particular, removing a census from admission does not make the periodic census incremental, nor does it eliminate census construction by LSP status signals.

**A second fix preserves exact dead-code snapshot reuse after an already-fresh duplicate refresh.** Generation-and-write-revision caching already existed, but `refresh_files` rebuilt the whole resolver index and incremented the revision even when it wrote no rows. The shortcut now skips that index and preserves the revision only for a genuinely write-free transaction. On the copied 330,998-row artifact, duplicate refresh plus snapshot retrieval fell from **9,031.213 ms to 68.139 ms**: resolver builds **1 → 0**, total projections across the initial scan and repeat **2 → 1**. Real graph changes, deletions and stale-backend repairs still invalidate. This does not make genuinely changed-file projection incremental; see the detailed follow-up below.

## Provenance and safety

- Source under investigation: `427895e45278924df01fee8353fb36806ec2c1ae`.
- Live PID: **72895**, version **0.55.1**, source identified by its builder as `a710b064da16a43a2b971cd1f4df9fc922c04b32`.
- Live binary SHA-256: `83d5c8b2552350878933edb77fa66f7c3513b3a3cbccaaef2ed94a7996aeef40`.
- Matching Mach-O UUID: `F5EED282-4996-3FEC-BC30-09D183D2C66B`.
- Published v0.55.1 symbols had UUID `4265A833-1107-31A9-8A72-C94A78B24A2C`, so they were **not used**. Automatic download also failed after three body-read attempts; curl recovered the archive and exposed the mismatch. The builder supplied `/tmp/aft-card71/target/release/aft.dSYM`; a dereferenced copy was used locally.
- The installed profiler accepts `--seconds`, not the requested `--duration` spelling. No daemon restart or explicit route bind was used for measurement. Benchmarks run through standalone Cargo test processes, never daemon tool dispatch.
- SQLite copies use the online backup API, not copying a database while ignoring its WAL. Only copied `backend_file_state.workspace_root` values are rerooted. No live generation or source file is changed by the probes.

## Trigger frequency: what the retained logs actually establish

The requested lookback was 48 hours ending **2026-09-12 10:27:11 UTC**. Retained daemon logs only cover **2026-09-11 17:01:02 through 2026-09-12 10:27:11 UTC**, an observed span of **17.4358 hours**. Missing retention is not counted as idle time.

Only logs whose opening lines identify `subc mode, pid` were counted: `aft-{29899,37078,41331,62424,72895,80871,81357,83878,94397}.log`. Standalone test/benchmark process logs were excluded. Counts are trigger **lines**, not deduplicated root events. `perf tier2 category=... reuse=miss` is a timed subset; `index_event kind=build_started plane=tier2` has broader coverage. Category elapsed times include scheduling/waiting, are not CPU time, and overlap nested projection time. Do not add their totals together.

### Top ten triage rows

Rank here is **investigation priority**, combining repeated live stack occupancy with logged frequency and offline work size. It is not a fabricated complete frequency × CPU ordering: several hot hooks have no invocation log, and the small copied-store fixture underrepresents live retained memory. The numeric cost proxy for fully timed rows is supplied separately so that limitation is visible.

| Priority | Kind / trigger | Scales with | Fires per observed hour | Measured cost | Disposition |
|---:|---|---|---:|---|---|
| 1 | tier-2 dead-code projection | all files, exports and outbound graph rows | **25.92** (452 timed lines; busiest hour 47) | copied full read **3,024 ms wall / 2,868.144 ms CPU**; placed-card worst snapshot **368,546 ms**; duplicate refresh + lookup **9,031.213 → 68.139 ms** | **Fixed write-free duplicate refresh**; genuine changes still filed |
| 2 | RouteBind → synchronous health rollup | all hosted roots and their retained subsystem data; allocator zones | **501.61** (8,746 lines; busiest hour 4,649) | copied 36-actor admission **6.602 → 2.309 ms**; main-thread rollup stack in 5,686 / 6,563 inclusive samples | **Fixed**: no census on admission |
| 3 | health worker refresh | roots, memory estimates, allocator zones, durable breaker rows, lifecycle inventory | not logged; timeout is 3 s **after** work, plus coalesced bind-completion wakes (not an exact 1,200/h) | copied 36-actor rollup **8.781 ms wall / 8.765 ms CPU**; worker rollup in 4,813 / 5,281 inclusive samples | Filed: incremental component census |
| 4 | LSP diagnostics/exits and configure Status → `build_status_snapshot` | roots, subsystem sizes, checkpoint entries, cache directories | no per-signal counter in retained logs | copied fleet status **30.592 ms wall / 13.916 ms CPU**; status/checkpoint frames on seven executor workers in both samples | Filed: separate status notification from census |
| 5 | one-file callgraph refresh | whole project resolution index despite graph-neutral extraction | **8.60** (150 tier-2 refresh lines; watcher refreshes not counted here) | copied **2,750 ms wall / 2,563.655 ms CPU**, index load 2,003 ms, only four WAL frames | Filed: resolution index amplification |
| 6 | tier-2 dead-code category reuse miss | contribution set plus graph reachability | **18.81** (328 timed lines) | logged median **5,392 ms**, max 384,043 ms | Filed; contains row 1, not additive |
| 7 | tier-2 duplicates reuse miss | cached contributions / occurrences | **18.53** (323 timed lines) | logged median **985 ms**, max 364,349 ms | Logged cost; no isolated offline category probe |
| 8 | tier-2 unused-exports reuse miss | imports/exports and contributions | **18.53** (323 timed lines) | logged median **1,114 ms**, max 361,500 ms | Logged cost; no isolated offline category probe |
| 9 | tier-2 complexity reuse miss | files / cached contributions | **18.53** (323 timed lines) | logged median **886 ms**, max 363,903 ms | Logged cost; no isolated offline category probe |
| 10 | tier-2 cycles reuse miss | dependency graph and contributions | **18.53** (323 timed lines) | logged median **803 ms**, max 365,090 ms | Logged cost; no isolated offline category probe |

For the fully timed subset, summed elapsed milliseconds per observed hour rank: dead-code category **189,248**, projection **85,458**, duplicates **78,454**, unused exports **70,366**, complexity **69,490**, cycles **64,015** (rounded). These are elapsed-time accounting proxies, **not independent CPU consumption**. A complete global frequency × CPU rank requires adding counters for health refreshes, LSP-triggered snapshots, manifest publications, and request-finalization hooks. This investigation does not claim those absent counters were measured.

### Trigger inventory

`N/L` means not individually logged, not zero. Default cadence is distinguished from observed execution count.

| Entry point / kind | Recomputed work and scaling | Trigger / cadence / observed evidence |
|---|---|---|
| `executor/mod.rs`: WatcherDrain, LspDrain, StandingPass coalesce keys | queue bookkeeping per actor; coalescing does not bound work inside a job | on enqueue, root-scoped |
| `subc/mod.rs::due_maintenance_jobs` | sorts roots, probes sources, searches background subscriptions; roots × subscriptions in the nested check | maintenance timer, 250 ms; at most 32 submitted jobs per turn; N/L |
| `MaintenanceDrainKind::Watcher` | bounded event/path slices; graph refresh, corpus invalidation, delayed view publication can have larger atomic units | source-ready probe; N/L |
| `MaintenanceDrainKind::Lsp` | bounded event drain, plus full status snapshot on diagnostics change/exit; idle-child shutdown | source-ready probe; N/L |
| `MaintenanceDrainKind::ConfigureTail` | one deferred configure unit at a time; stages listed below | pending configure continuation; N/L |
| `MaintenanceDrainKind::CompletionDrains` | inspect/build completions and semantic refresh completion; status/view publication can expand work | queue probes or pending background wake; N/L |
| configure Admission | generation checks, worker-release signals, format-cache invalidation and backup/checkpoint storage setup | per deferred configure job; session-only shortcut may terminate here |
| configure SessionReplay / BashRuntime | per-session task replay; reminder settings | first-session replay/config changes; task count; N/L |
| configure ProjectRuntime / Watcher | gitignore matcher project walk; watcher stop/join/start | changed runtime inputs, not equivalent rebind; N/L |
| configure ViewLoad | view runtime load, legacy semantic import, possibly initial pending-path publication | views enabled and not home root; manifest/semantic corpus size; N/L |
| configure StorageSweeps | backup/checkpoint/log/view housekeeping | deferred stage, storage-entry count; N/L |
| configure ProcessFlags | filter registry / failed-spawn cache clearing | changed flags, cached entries; N/L |
| configure Callgraph / SemanticRelease | schedule graph warm, release semantic worker after graph start | root/config-dependent, cold corpus size; N/L |
| configure Status | full status assembly before emitter signal | final deferred stage; N/L |
| `runtime_drain` search/callgraph/semantic completion handlers | install completed stores, reconcile pending paths, update health/status | worker completion; N/L |
| `runtime_drain::drain_semantic_refresh_events` | pending semantic paths can invoke view publication; empty pending set is already skipped | semantic refresh completion; no successful publication lines in selected daemon logs, not proof of zero invocations |
| tier2 scheduler Debounce / Ceiling / Pull / ConfigureWarm | dispatches corpus/contribution scans; scheduler itself is constant-sized | 45 s debounce, 120 s storm debounce, 30 min ceiling, 5 min minimum, 90 s cold delay; 2,798 tier-2 build-start lines = 160.47/h |
| subc drain tick / pending response poll | retry buffers, wake subscriptions, pending binds / responses, occupancy snapshots | 250 ms / 100 ms; 1,032 perf tick reports = 59.19/h, reports are not drain executions |
| subc idle reaping | roots, path existence, lifecycle/eviction checks | 30 min idle TTL; 171 reap reports = 9.81/h; reports can be rate-limited |
| standing actor tick | standing-root reconciliation and coalesced per-root passes | 250 ms scheduling opportunity; N/L |
| `subc/health.rs` rollup | two sets of root memory estimates, process observations, root health, breaker state, lifecycle inventory | worker timeout/wakes plus pre-fix RouteBind; see rows 2–3 |
| health stuck-watch / occupancy diagnostics | subscriptions, tasks, running jobs | watch scan 60 s; stuck age/log interval 10 min; occupancy threshold 60 s; N/L |
| `bash_background/watchdog.rs` | task polling, output/watch scanning, child reaping, reminders | 500 ms running-task poll; cleanup 60 s, finished retention 1 h; N/L |
| `response_finalize.rs` | per-session completions, fleet status, alert finalization, status-bar counts | each agent-visible response; N/L |
| `alert_render.rs::finalize` | partition candidates, sorting, rendering, dispatch-root canonicalization | each alert-bearing finalization; N/L |
| `commands/status.rs` | root memory census, checkpoint list, backup state, recursive cache size walks, compression query | explicit status requests and internal status signals; N/L |
| `fleet_status.rs::publish` | scope throttle and bounded wire enqueue | invoked from finalization; 2.5 s publication cadence, 7.5 s TTL; N/L |

## Two attributed live profiles

The retained comparison samples started **10:00:32.099 UTC** and **10:11:24.008 UTC**, separated by **10 minutes 51.909 seconds**, each requested for 30 seconds:

```sh
ck-aft profile --seconds 30 --dsym /path/to/copied/matching.dSYM --json --raw
```

The profiler's `heaviest_paths` omits many `verdict=other` stacks even though symbolication succeeds. Therefore the raw samples were also symbolized with `atos -o <DWARF-file> -l 0x100e10000`, passing every `ck-aft` address through stdin. Attribution below uses the complete sample tree, not only the profiler's selected named subsystems. Inclusive stack counts include descendants and waits; they must not be added to leaf CPU counts.

| Thread | Sample A running / total | Sample B running / total | Attributed mechanism |
|---|---:|---:|---|
| main transport thread | 4,803 / 8,943 | 5,462 / 10,515 | `run_module_loop → handle_control_request → HealthRollupCache::refresh → build_health_diagnostic_rollup`; refresh subtree 5,686 / 6,563 samples |
| `aft-health-rollup` | 4,589 / 8,943 | 5,235 / 10,515 | the same whole-fleet builder, independently of admission; refresh subtree 4,813 / 5,281 samples |
| executor workers | worker 0: 1,820 / 8,943; worker 7: 533 / 8,943 | worker 2: 323 / 10,515; worker 6: 249 / 10,515 | `build_status_snapshot_for_session`, `memory_estimates`, `CheckpointStore` paths on seven workers in both captures; this does not prove all their busy time is census work |
| artifact-owner heartbeat | 313 / 8,943 | 198 / 10,515 | `artifact_owner::atomic_write_manifest`, `fcntl` / parent-directory sync; separate from views manifest publication |

Within main-thread health work, `memory_estimates` accounts for 2,220 / 2,537 inclusive occurrences and allocator-observation frames for 2,053 / 2,496. In the health worker those counts are 1,468 / 1,678 and 2,071 / 2,467 respectively. Allocator descendants include `xzm_statistics_self`, `_xzm_foreach_lock`, and `_xzm_introspect_enumerate`. These are process-zone walks, not an O(1) RSS query.

The sampled return site identifies the extra refresh at `subc/mod.rs:4812` in the baseline, immediately after spawning the route-bind completion waiter. Successful bind completion already calls `health_rollup_worker.request_refresh()` in both control-completion handling paths. The fix removes only the former.

## Standalone measurements of the five priority mechanisms

No source corpus was edited. The callgraph probe copies the real generation for artifact key `90ff783f3f4c5cf2`, chooses `crates/aft/tests/engine_comparator_test.rs`, normalizes it once, then forces only the copied freshness marker stale. The graph had **2,294 files, 9,645 exports, 330,998 outbound projection rows**. The local source database copy occupied **507,658,240 bytes**.

The bind/status/health fixture opens that copied generation through 36 read-only handles in 36 registered actors, uses this checkout as the configured project root, and runs a no-op successful configure dispatch. It does **not** recreate the live daemon's loaded semantic indexes, LSP children, checkpoint archive, symbol cache or allocator-zone population. Those measurements establish the work path and a reproducible small offline comparison, not fleet-production latency predictions. The callgraph projection/index measurements operate on the actual row corpus.

| Priority mechanism | Wall ms | Process CPU ms | Work evidence |
|---|---:|---:|---|
| Route admission, original → fixed | **6.602 → 2.309** | **6.756 → 2.591** | **1 → 0** completed health refreshes; configure still completes |
| Periodic health builder | **8.781** | **8.765** | 36 actors with copied-store handles; unchanged by fix |
| Status assembly | **30.592** | **13.916** | copied-store fleet fixture, whole snapshot; unchanged by fix |
| Full dead-code projection | **3,024** | **2,868.144** | 330,998 outbound rows, no WAL delta |
| Graph-neutral one-file refresh | **2,750** | **2,563.655** | index load **2,003 ms**; unchanged extract count 1; 4 WAL frames / 16,512 bytes |

An earlier callgraph run measured refresh 2,793 ms (2,142 ms index load), snapshot 3,648 ms. This variability is why regression assertions count work rather than test wall time. CPU is user + system time from `getrusage(RUSAGE_SELF)` and can exceed wall time in a process with workers. CPU helpers report zero on platforms without Unix rusage; the measurements above are Darwin, not Windows. Allocation counts were not collected.

Reproduce the graph measurements:

```sh
AFT_CALLGRAPH_REFRESH_STORE=/path/to/source-or-copied/root-key-directory \
AFT_CALLGRAPH_REFRESH_ROOT="$PWD" \
AFT_CALLGRAPH_REFRESH_FILE=crates/aft/tests/engine_comparator_test.rs \
cargo test -p agent-file-tools --test callgraph_refresh_bench \
  bench_refresh_files_on_store_copy -- --ignored --nocapture
```

For the bind fixture, first make a SQLite online backup into `$PWD/target/cpu-hunt-local/store-copy`, preserve its `.current` pointer, and reroot only the copied backend-state records to `$PWD`. The fixture refuses artifact paths outside this checkout's `target` directory:

```sh
AFT_CPU_HUNT_STORE_COPY="$PWD/target/cpu-hunt-local/store-copy" \
cargo test -p agent-file-tools --lib \
  route_bind_does_not_recompute_fleet_health -- --nocapture
```

Without that environment variable the same regression uses one temporary actor and no production artifacts.

## Work-count proof and behavior contract

The fixed files were staged before restoring the old call behind an explicit `NON-VACUITY BREAK` marker. `git diff --stat` was empty before mutation, showed `crates/aft/src/subc/mod.rs | 4 ++--` while mutated, and was empty after `git checkout -- crates/aft/src/subc/mod.rs && touch crates/aft/src/subc/mod.rs` restored the staged fix. The copied-artifact mutant produced:

```text
route_bind_health admission_us=6602 cpu_us=6756 refreshes=1
assertion `left == right` failed: route admission must not perform a fleet-wide health census
  left: 1
 right: 0
test subc::tests::route_bind_does_not_recompute_fleet_health ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3165 filtered out
```

Exactly that test failed; no other tests ran in the mutation command. The restored fixed probe reports `refreshes=0` and passes. An earlier attempted copied-fixture mutation stopped at the fixture path fence (Cargo's test working directory is the package, not workspace); that attempt is **not** counted as proof. The fence was corrected to the workspace derived from `CARGO_MANIFEST_DIR` before the successful measurement above.

Tool response rendering and the parity fixture are unchanged. Health diagnostics were already asynchronous/cache-aged; they now remain at the previous published snapshot until the existing worker refresh, rather than being opportunistically refreshed by an unrelated incoming bind. Live liveness counters in `build_health_report` still read fresh. No existing assertion was weakened or inverted.

## Filed follow-ups and coverage limits

1. **Periodic health still repeats root work.** `build_health_diagnostic_rollup` calls `memory_root_snapshot` and then `memory_root_rollup` for every root; both call `memory_estimates`. Process memory assembly also invokes allocator observations. The copied 36-root baseline is 8.781 ms; live stacks show substantially more retained-state work. A correct incremental design needs component revisions / accounting counters, not a time cache that silently changes status semantics.
2. **LSP/configure status amplification remains.** `drain_lsp_events_bounded` signals by constructing `build_status_snapshot`, and the configure Status stage does the same. Copied-fleet status costs 30.592 ms. Add a dirty/status notification seam and make the payload owner publish snapshots, preserving freshness/ordering and per-session fields. No stale-cache shortcut is introduced here.
3. **Genuinely changed graph projection remains O(graph).** The 330,998-row read costs 3,024 ms / 2,868.144 ms CPU. Already-fresh duplicate refreshes now preserve cache identity, but incremental projection must preserve both old and new caller effects, deletions, dispatch edges and revision identity; merely skipping a refresh can return stale dead-code results.
4. **Stale-but-graph-neutral extraction still builds the resolver index.** After the preceding write-amplification fix it writes just four frames, but `ProjectIndex::from_db_and_callers` still costs 2,003 ms in a 2,750 ms forced-stale refresh. `stored_extract_matches` currently needs that index for resolved-reference equality, so removing the index call unconditionally is not safe. The second fix covers already-fresh inputs with no caller extracts, not this forced-stale extraction case.
5. **Zero bound routes does not imply literally zero per-root work.** `due_maintenance_jobs` sorts live roots, and quiesced roots still permit LSP drains. The health worker traverses registered actors independently of route count. Configure jobs cancel on quiescence and Watcher/ConfigureTail/CompletionDrains are excluded by the unbound predicate. 1,098 quiesce lines (62.97/h) establish the transition is common. LSP exit draining is legitimate teardown work; a claim of nil idle work would need a steady-state counter test, which is not provided here.
6. **Views publication, per-import package walks, and git attribute pumping are not ranked as measured fixes in this delivery.** No matching successful view-publication lines appeared in the selected daemon logs. That is an instrumentation/coverage limitation, not evidence that the originally reported 74-second operation disappeared. Semantic ready publication already skips an empty pending set; nonempty publication and configure ViewLoad still deserve a copied semantic/views artifact probe. No new before/after numbers are claimed for them.
7. **The ranking is incomplete where frequencies are unlogged.** The five priority mechanisms above were measured offline; duplicates/unused-exports/complexity/cycles have logged elapsed costs but not isolated offline category measurements. There is no complete global frequency × CPU ordering, invocation count for every hook, allocation census, or measured zero-route steady-state bound in this delivery.

## Gates

- `cargo test -p agent-file-tools --lib`: **3,147 passed, 19 ignored** after the bind fix. Subsequent full-suite reruns after CPU-only benchmark instrumentation exposed unrelated environment/test-order failures: the live default database advanced to schema 10 while this source supports 9 (`standing_roots::tests::daemon_startup_empty_pass_marks_existing_snapshot_strict_on_first_configured_pass`). Rerunning with `XDG_DATA_HOME` inside the worktree avoids that live database; one default-parallel run then failed a semantic HTTP transient-classification test and two configure-tail timeout tests. A four-thread full run passed 3,146 tests and failed only `commands::configure::tests::cold_configure_starts_callgraph_before_semantic`; that test passed when rerun alone under the same isolated storage. These are recorded rather than weakening those tests or modifying the operator database.
- `cargo test -p agent-file-tools --test integration subc_`: **117 passed, 5 ignored** (bridge, storm, detach and other matching subc integration coverage).
- `cargo test -p agent-file-tools --test integration tool_call_parity_test`: **12 passed**.
- `git diff --exit-code 427895e45278924df01fee8353fb36806ec2c1ae -- crates/aft/tests/integration/tool_call_parity_test.rs`: **byte-identical source**.
- `RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --all-targets`: passed.
- `RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --target x86_64-pc-windows-gnu --all-targets`: passed.
- Copied production callgraph probe and fixed copied-store admission probe: passed.
- `aft_inspect` returned incomplete diagnostic coverage for the Rust files; Cargo checks, not that empty diagnostic list, are the authority.

## Follow-up: placed-card dead-code amplification and exact cache reuse

The placed card's first 90 minutes (09:16:39–10:46:39 UTC) contain **22** `perf tier2 phases category=dead_code` lines, **16** with snapshot time over 2 seconds. The earlier supplied census of 21 / 15 preceded the last 10:37:52 entry. Examples verified directly in `aft-72895.log`:

| Root | Snapshot ms | Scan files | Rollup ms |
|---|---:|---:|---:|
| prefrontal, 10:33:13 | 12,961 | 11 | 8,455 |
| magic-context, 10:34:18 | 21,200 | 22 | 6,507 |
| OSS opencode, 10:24:57 | **368,546** | 1,035 | 6,708 |

The scan-file count is the phase log's value, not necessarily watcher-batch size. The small watcher batches reported with these incidents are evidence of amplification; they do not mean only those rows are read by snapshot projection. The 330,998-row copied fixture in this investigation belongs to AFT, not a measured row count for the OSS opencode store. Phase elapsed times also include waits, so the six-minute sample must not be interpreted as six minutes of pure projection CPU without an isolated reproduction.

### Why caching by generation alone is not safe

`InspectManager` already caches an `Arc<CallgraphSnapshot>` by canonical project root, cold-build generation (or legacy database path), **and durable write revision**. `project_dead_code_snapshot_with_revision` reads rows and revision in one read transaction. Two unchanged reads already reuse the Arc. In-place refresh does not mint a new cold-build generation; a generation-only cache would therefore return old symbols after edits. Existing tests exercise both in-place mutation and newly published generation invalidation, and remain green.

The actionable redundant work was lower down: an already-fresh `refresh_files` still constructed `ProjectIndex::from_db_and_callers` and unconditionally bumped `projection_write_revision`. This occurred even when all input rows were HotFresh and `clear_stale_backend_status_for_file` updated zero rows. The next inspect saw a new revision and re-read the entire graph for no changed database state.

### Mechanism change

After applying freshness repairs and deletions, `refresh_files` can return early when `caller_extracts` is empty. It compares the connection's actual SQLite `total_changes` before and after those operations:

- **No written rows:** commit the read-only transaction without changing the durable revision; do not record a write commit or load the resolver. The existing manager cache remains valid.
- **Any written rows:** advance the revision in the same transaction, even with no callers to resolve. This includes deletion without dependents and stale-backend repair. A new regression deletes an unreferenced entry file and verifies that the resolver is skipped but the next projection removes that file.
- **Surviving caller extracts:** preserve the existing resolver, graph equality, dependency refresh, method-dispatch and revision behavior.

This uses the existing generation/revision protocol, without a schema change, time-based stale cache, or weakening of real-edit invalidation. It only avoids corpus work on confirmed-fresh duplicate events. The fraction of live watcher batches already refreshed by another path was **not measured**, so the full six-minute incident is not claimed solved.

### Copied-artifact before/after and non-vacuity

`inspect::manager::guard_tests::profile_fresh_refresh_projection_on_store_copy` is an ignored opt-in unit benchmark that uses the real manager cache. It normalizes the selected copied row, projects once, runs an already-fresh refresh of that file, and asks the same manager for a snapshot again. The copied storage must be below the checkout's `target` directory with `callgraph/<artifact-key>` layout. It neither reads nor changes the live store.

```sh
AFT_CPU_HUNT_STORAGE_COPY="$PWD/target/cpu-hunt-local/projection-storage" \
XDG_DATA_HOME="$PWD/target/cpu-hunt-local/test-data-home" \
cargo test -p agent-file-tools --lib \
  profile_fresh_refresh_projection_on_store_copy -- --ignored --nocapture
```

| Measurement | Original refresh path | Fixed path |
|---|---:|---:|
| Initial projection wall ms (separate warm-up) | 8,580.097 | 6,453.533 |
| Already-fresh refresh + second snapshot wall ms | **9,031.213** | **68.139** |
| Resolver-index builds during refresh | **1** | **0** |
| Total projections across initial + second lookup | **2** | **1** |
| Outbound rows in initial snapshot | 330,998 | 330,998 |

A prior fixed measurement was 123.619 ms for refresh + second lookup, also with zero resolver builds and one total projection. CPU was not separately collected for this combined cache-path comparison; the isolated uncached projection CPU measurement is in the earlier table.

The fixed source was staged; the empty-caller shortcut was then disabled with `NON-VACUITY BREAK`. The diff stat changed from empty to `crates/aft/src/callgraph_store/mod.rs | 3 ++-`, and returned to empty after restoring/touching the staged file. The small regression produced:

```text
fresh_refresh_projection initial_ms=169.874 refresh_and_snapshot_ms=76.918 index_loads=1 projections=2 outbound_rows=1
assertion `left == right` failed: a fresh refresh must not load the corpus resolver index
  left: 1
 right: 0
test inspect::manager::guard_tests::projection_cache_reuses_snapshot_after_already_fresh_refresh ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3167 filtered out
```

A separate run of the copied-store probe against the same mutation also failed exactly its named test, with `index_loads=1 projections=2` and the 9,031.213 ms figure above. Under that mutation the two positive invalidation controls **did not fail**:

- `inspect::manager::guard_tests::projection_cache_invalidates_on_in_place_refresh_for_readonly_scans`
- `inspect::manager::guard_tests::projection_cache_invalidates_when_cold_build_publishes_new_generation`

### Final gates for the projection fix

- Full library with isolated default storage and serial test execution: `XDG_DATA_HOME=... cargo test -p agent-file-tools --lib -- --test-threads=1` — **3,149 passed, 20 ignored**. This avoids the default-database schema mismatch and the parallel/order failures described above, without changing tests.
- `cargo test -p agent-file-tools --test integration callgraph`: initially **67 passed, 10 failed** because in-repository fixtures inherited this checkout's read-only worktree identity; the diagnostic error explicitly said the persisted store was unavailable in a read-only worktree. With `GIT_CEILING_DIRECTORIES="$PWD/crates/aft/tests/fixtures"` and isolated default storage, the same suite passed **77 tests**. This fences only fixture ancestor discovery; no parent checkout or daemon is warmed to make the test pass.
- `cargo test -p agent-file-tools --test integration inspect`: **222 passed, 5 ignored**.
- `cargo test -p agent-file-tools --test integration tool_call_parity_test`: **12 passed**; parity source remains byte-identical to the task base.
- Native and `--target x86_64-pc-windows-gnu` **all-target** Cargo checks with `RUSTFLAGS='-D warnings'`: passed after the final code edits.
- Comment clarity review: no flagged changed comments. Formatter and diff whitespace checks: passed.
