# Views publication performance hunt — 2026-09-16

## Scope and measurement discipline

Base engine: `f5b953a33d4048167a2911700038469c339879fc`. Subject: the standalone process on `~/Work/OSS/opencode`, HEAD `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`, A `a085bf62a459` (300 changed paths), B `refs/remotes/upstream/v2-timeouts` / `b85cf3d67fe3` (298). This document distinguishes engine revisions from the subject revision (the drill's `observed_at` header names the latter).

Baseline built locally with `cargo build -p agent-file-tools --profile stage --bin aft -j 4`, including only opt-in phase instrumentation (`AFT_VIEWS_PERF_HUNT=1`). Fresh storage; `scripts/views-branch-drill.sh --mode views-on --root ~/Work/OSS/opencode --binary "$PWD/target/stage/aft" --storage "$PWD/target/hunt-baseline/storage" --output-dir "$PWD/target/hunt-baseline"`. The sibling-directory exclusive drill lock was held throughout checkout and restoration. No live daemon was profiled, restarted, or used as the measurement subject.

Raw baseline artifacts in this task worktree: `target/hunt-baseline/branch-drill-views-on.json`, `branch-drill-views-on.stderr.log`, `sample-1.txt`, `sample-2.txt`. Standalone PID **56249**. Samples were taken with `/usr/bin/sample 56249 5 -file ...` at 17:57:34Z and 17:58:09Z, immediately after the first two checkout transitions. Both resolve symbols and source lines against the matching stage binary. Baseline correctness defects: **none**. Concurrent host work and the two samples affect absolute wall/CPU comparisons; these are not isolated-machine release results.

## In-situ attribution before fixes

Milliseconds; CG = callgraph-plane publication, SF = semantic fill. These are the actual local baseline events, not the older card-103 rows.

| transition | plane | manifest | blobs | materialization call | clone (including preparation) | closure | membership probe | blob durability | derived durability | alias durability |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→A | CG | 2239 | 1779 | 9086 | 16 | 7562 | 7460 | 40 | 12 | 20 |
| HEAD→A | SF | 123 | 445 | 47 | 14 | 3817 | 3767 | 13 | 8 | 4 |
| A→HEAD | CG | 3345 | 722 | 8779 | 12 | 5726 | 5666 | <1 | 12 | 12 |
| A→HEAD | SF | 289 | 428 | 58 | 20 | 4403 | 4337 | <1 | 13 | 15 |
| HEAD→B | CG | 3127 | 611 | 10037 | 15 | 7663 | 7581 | 20 | 10 | 13 |
| HEAD→B | SF | 134 | 707 | 109 | 10 | 6661 | 6612 | 12 | 4 | 4 |
| B→HEAD | CG | 4851 | 1154 | 12800 | 7 | 4764 | 4692 | <1 | 14 | 19 |
| B→HEAD | SF | 209 | 737 | 115 | 22 | 8634 | 8547 | <1 | 18 | 11 |

Store opens took **1.2–2.4 ms** on these eight publications, not the hundreds of milliseconds attributed to `blobs_ms`. `head_tree_entries` took **48–134 ms**; candidate assembly took **2157/3218/3024/4704 ms** on CG versus **21/32/23/37 ms** on SF. SF `blob_and_alias_puts` took **0.064–0.119 ms**; its **426–736 ms** path-status loop explains nearly all its `blobs_ms`. The rest of closure is small file/parent durability (trigram 5–13 ms, manifest 16–29 ms, pointer preparation 5–17 ms). The publication durability lock measured <1 ms in every row.

**Correction to the supplied table:** 2108/1337/9359/2410 and 94/128/2100/2052 belong to **HEAD→A**, not HEAD→B, in the actual card-103 stderr (lines 647 and 667). Its true HEAD→B rows at lines 713/721 are 1980/947/11593/3179 and 104/160/2099/1995. The return B→HEAD numbers in the task agree with lines 736/743. Compare by transition and plane, not the mistaken first label.

## Ranked findings

1. **Tier-2 runs the legacy callgraph end-to-end on a views-on root.** `inspect/manager.rs::build_tier2_callgraph_snapshot_with_refresh_inner` chooses `CallGraphStore::open_readonly` or a writable legacy store, calls `refresh_writable_dead_code_store`, and projects that store's SQLite path. The sample stacks show both `CallGraphStore::refresh_files_with_workspace_crate_prefix_cache` and its profiled implementation active during switches. The supplied stderr records a 281-path legacy refresh and `tier2_callgraph_snapshot source=callgraph_store ms=63902` at lines 712/717, despite `callgraph_invalidations=0` at 681. Local baseline records **68815 ms** at 17:58:46Z (overlapping the first two switches). `callgraph_invalidations=0` alone is therefore not evidence that no legacy callgraph runs. This is a large CPU and residency cost; do not add its overlapping wall time to publication wall time.
2. **Closure membership traverses payload-bearing primary-key pages.** `blob_store/mod.rs::BLOB_SCHEMA` declares `blob_payloads WITHOUT ROWID`, so `SELECT full_key` or `SELECT 1` using the primary key walks the same tree as payloads. `views/assembly.rs::SqliteClosure::probe_blobs` already batches/sorts and retains two connections; batching did not remove the payload-tree I/O. In-situ membership is **3.8–8.5 seconds per publication, twice per switch**, while fsync/checkpoint is tens of milliseconds. On self-contained copies of the card-103 final manifest and blob DBs (`target/hunt-closure`), the existing ignored `bench_closure_probe_strategies` measured 8891 keys: per-key open 28801 ms, retained handle 7557 ms, batch 5154 ms. A standalone SQL experiment on that same copy measured batched semantic/callgraph primary-key probes **814/3923 ms**, versus **2.3/3.2 ms** with `INDEXED BY blob_membership`. Merely creating an index did not change SQLite's chosen plan; the membership query must select the narrow covering index explicitly.
3. **Warm content is re-extracted before `put` discovers reuse.** `views/assembly.rs::prepare_checkout` skips extraction only when the key matches the *immediately previous manifest*. Switching back to an already stored key still calls `CallgraphBlob::extract` and serializes it, then `BlobStore::put` returns Reused. Cost is the CG candidate assembly above (2.2–4.7 s locally), plus avoidable payload hashing/put/alias work. This is not `git ls-tree` or check-attr cost.
4. **Path-status no-op work survives zero-put publications.** `prepare_checkout` invokes `PathStatusStore::clear` for essentially every manifest candidate. Measured SF cost is 0.4–0.7 s locally, versus 1–2 ms for store opens. This is separate from blob puts and from closure. `path_status` lies outside the edit fence; left for a transactional/batched status follow-up rather than weakening pending-path cleanup.
5. **Read handles reopen per operation.** `context.rs::callgraph_store_for_ops_with_wait` opens a `ReadonlyCallGraphStore` for the pinned view on each call; `callgraph_store/mod.rs::open_manifest_view` opens derived SQLite and checks readiness. It does not re-run manifest-join resolution; that is materialized at publication. The older drill logs show repeated `callers` around 156–356 ms. A generation-scoped reader cache is a separate opportunity; navigation behavior is preserved by this change.
6. **Manifest metadata work is O(tracked paths), but not the 2-second phase.** `alias::head_tree_entries` executes ls-tree and check-attr each call; no one-second-window cache is present. Publication setup in `context::publish_view_paths` and `assembly::prepare_checkout` both request the tree. `configure::open_view_runtime_for_configure` additionally calls `report_head_checkout`, which reads the tree again. Check-attr is the concurrent pump, not the old pipe deadlock. Local head phase was only 48–134 ms. Cache invalidation would need to account for attributes, index/worktree and HEAD changes; no speculative metadata cache added.
7. **Generation clone and durability are not the observed seconds.** The supplied clonefile calls themselves take 0–6 ms; local whole clone preparation is 7–22 ms. `generation::clone_derived` serializes with deferred checkpoint before copying main SQLite; `PreparedAssembly::commit` schedules the deferred checkpoint after CAS. `ViewStore::sweep_generations` is configure maintenance, not called by publication. Old files remaining on disk are not evidence that their pages remain resident. The SF materialization/clone/derived checkpoint is the sibling's bucket and remains untouched.
8. **RSS is not retained view state alone.** PID 56249's first sample has 2.4 GiB footprint (3.5 GiB peak), second 1.1 GiB (same peak): substantial memory is released between switches. Active legacy refresh and the view materialization allocate at the same time. This experiment does not partition allocator slack versus semantic vectors versus graph allocations precisely. The drill's `health_before/after` memory block is the live daemon, not the standalone PID; it must not be used to attribute subject growth. RSS row deltas and samples are the valid subject evidence.

## Changes, tests and final comparison

Implementation and final verification follow. No change is made to `apply_manifest_diff`, dependent selection, resolver/row emission, or semantic-fill skipping derived; those belong to sibling `wi_aab19ec1`.

Parent-authorized expansion: Tier-2 store chooser and tests in `inspect/manager.rs`; a pure delegation in `context.rs::callgraph_store_for_ops_with_wait` to a shared view-reader helper. A pending HEAD will be reported rather than silently refreshing legacy. Navigation absent-view and pinned-snapshot behavior must remain unchanged.

### Implemented mechanisms and regression controls

- `blob_store::BLOB_SCHEMA` now creates the narrow `blob_membership(full_key)` index idempotently, including existing databases. Both scalar and batched closure queries explicitly select it. Payload schema/version, digest validation, quarantine behavior, closure membership semantics, and durability ordering are unchanged. This is an index, not a cached assertion of blob presence.
- Assembly now validates an existing blob with `BlobStore::get` before extracting content absent from the immediately previous manifest. A digest/schema miss still takes extraction; the existing immutable-store handling of malformed rows is unchanged. This does not skip source hashing or trust Git aliases without bytes.
- Tier-2 now uses the published view and never refreshes legacy when views are enabled. Missing publication, HEAD mismatch, or a tracked watcher source whose content key differs yields **`projection=none reason=view_pending`**, not a wait inside the worker and not a legacy fallback. Reporting pending avoids holding a worker while semantic/callgraph publication needs to progress. The normal scheduler can retry. Navigation's absent-view fallback is unchanged.
- Shared reader signature: `views::read::open_published_callgraph(project_root: PathBuf, family: String, view_dir: PathBuf, generation: &str, pin: Option<Arc<QueryPin>>) -> callgraph_store::Result<ReadonlyCallGraphStore>`. Callers: `context::AppContext::callgraph_store_for_ops_with_wait` (one-line delegation replacing its former direct constructor call) and `inspect::manager::current_view_projection_store`. It opens exactly the selected generation; the navigation proof publishes a newer generation while holding the old pin and compares the original constructor's path with the shared helper's path.
- The common dead-code projector previously inferred a root from legacy `backend_file_state`; a published view correctly has no such rows. Parent authorized a read-only adapter in `callgraph_store/dead_code_projection.rs`, with the same projection implementation and SQL row readers. It takes the view reader's explicit root and rejects a legacy reader. The view remains pinned through projection. No materializer, resolver, or derived schema changed. The parity test collects **one nonempty contribution set per revision**, then rolls it up against legacy and view snapshots of the same fixture checkout and compares the complete verdict JSON across baseline, A, baseline, B, baseline (two round trips).

Red-first output (before the corresponding fixes):

```text
views::assembly::closure_connection_tests::closure_membership_queries_use_payload_free_index ... FAILED
membership must not walk payload-bearing WITHOUT ROWID pages: SEARCH blob_payloads USING PRIMARY KEY (full_key=?)

views::assembly::reuse_tests::views_cached_callgraph_payload_does_not_extract_again ... FAILED
cached content must not be extracted or put again

inspect::manager::guard_tests::views_tier2_published_plane_never_refreshes_legacy ... FAILED
views-on Tier-2 refreshed the legacy store; left: 1, right: 0
inspect::manager::guard_tests::views_tier2_pending_plane_never_falls_back_to_legacy ... FAILED
an unpublished view is pending even when legacy is ready
```

**NON-VACUITY BREAK controls:** the live state was explicitly staged first and `git diff --stat` was empty. Four controls were then applied together, with separate one-test invocations so each expected failure is independently attributed: remove `INDEXED BY` from the production batch query; neutralize the stored-payload early return; neutralize the Tier-2 view chooser; neutralize the projection adapter's reader-kind refusal. Applied stat: `dead_code_projection.rs | 3 ++-`, `inspect/manager.rs | 3 ++-`, `views/assembly.rs | 6 ++++--` (3 files, 8 insertions, 4 deletions). Each invocation failed exactly its named test, with zero other failed tests:

```text
closure_membership_queries_use_payload_free_index: FAILED — SEARCH blob_payloads USING PRIMARY KEY (full_key=?)
views_cached_callgraph_payload_does_not_extract_again: FAILED — cached content must not be extracted or put again
views_tier2_published_plane_never_refreshes_legacy: FAILED — legacy refresh count left 1, right 0
views_projection_adapter_refuses_legacy_reader: FAILED — unwrap_err() on Ok(projected snapshot)
```

Raw outputs: `target/hunt-mutant-<test-name>.txt`. All three mutated files were restored with `git checkout -- <paths> && touch <paths>`; `git diff --stat` was empty afterwards. Restored unit gate: **114 passed, 6 intentionally ignored, zero failed**, including existing legacy stale-row/projection/cache tests and the new view tests. No mutant remains in the delivered tree.

### Scheduling and remaining ownership

`runtime_drain::publish_view_if_quiet` requires the drain to be back in Collect and the one-second deadline to expire. It does not wait for Tier-2 completion. `publish_semantic_ready_view` passes only `ViewRuntimeSnapshot.pending_paths`; it is not a full-root trigger. However, publication request setup (`context::publish_view_paths` → `migration::store_live_semantic_blobs`) still does semantic blob work on the transport/drain thread: the first sample has 234 samples under semantic-store preparation, 196 under `BlobStore::put`, with payload-tree `accessPayload`/`pread` below it. That setup is outside the assembly phase event and remains a follow-up, not conflated with the fixed candidate extraction.

Remaining for sibling `wi_aab19ec1`: callgraph-plane materialization internals, dependent selection, resolver/row emission, and preventing the semantic-fill publication from cloning/materializing/checkpointing derived. Remaining for follow-up owners: transactional path-status batching; generation-pinned navigation reader caching and broader query benchmarks; semantic request-setup work on the drain thread; allocator/type-specific RSS census. The two samples demonstrate released resident memory but do not establish a leak-free long soak or attribute every allocation.

### Read-side measurements

`views_profile_navigation_reads_on_drill_artifacts` ran against same-HEAD opencode stores from the discarded after-run's completed initial publication (before any branch checkout). Ten depth-3 `callers` and ten depth-3 `impact` queries for `TimelineDetailControl` in `packages/app/src/settings/timeline-detail.tsx`; retained legacy handle versus reopened view handle, matching navigation's respective policies. Both returned two callers/two impacted sites. Mean milliseconds:

| reader | open per query | callers | impact |
|---|---:|---:|---:|
| legacy retained | 0.001 | 593.366 | 546.382 |
| view reopened | 0.803 | 234.406 | 227.805 |

This is a manual same-artifact comparison, **not a release latency gate**: the test harness was compiled with the crate's `opt-level=0, debug=0` override used to shorten test links, and ran legacy first. It establishes that reopening is real but is not the hundreds-of-milliseconds cost on this query; it does not justify caching a reader ahead of publication fixes. No per-call manifest rebinding/resolution was observed in the constructor.

### Verification and limitations

- `cargo build -p agent-file-tools --profile stage --bin aft -j 4`: passed; final optimized executable was copied to `target/hunt-final-binary/aft`, SHA-256 `400feda1903c9b2a56b13c1afe2d60035571c05e7cf8030f38965e61f8046293`, before test builds could replace the stage output.
- `RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --lib --bin aft -j 4`: passed for both targets. No manifests/lockfiles changed.
- Unit runner uses `cargo --config 'profile.stage.package.agent-file-tools.opt-level=0' --config 'profile.stage.package.agent-file-tools.debug=0' test -p agent-file-tools --profile stage ... -j 4`. `--lib -- views:: blob_store:: inspect::manager:: ... --test-threads=4`: 114 passed, 6 manual benchmarks ignored. Expanded two-round-trip parity: passed; manual read benchmark: passed without compiler warnings after removing its unused trait import.
- `--lib --test watcher_integration -- view_publication branch_switch_test --test-threads=2`: 5 executor/scheduling tests and both watcher branch-switch tests passed (including zero re-embedding on the return trip).
- `--test integration -- view_assembly_wiring_test publication_cas_test migration_import_test branch_switch_test tool_call_parity_test --test-threads=4`: 24 passed, one **unrelated hashline test** failed at `tool_call_parity_test.rs:331`, missing `edit["op_id"]`. The same exact test fails with the unmodified installed `~/.local/share/cortexkit/bin/ck-aft` via `AFT_TEST_AFT_BINARY`; no hashline/edit code was changed. The required `tool_call_matches_direct_spine_envelopes` parity gate passed in that run and passed again against the preserved optimized drill binary. No test expectation was weakened.
- AFT diagnostics were incomplete: initial inspect timed out; later inspect named unavailable Rust/Bash/YAML producers. Compiler gates above are authoritative. Comment-review sidekick requests timed out twice (390 seconds each); comments were manually reviewed for standalone clarity instead.
- One attempted after-drill (`target/hunt-final`) was **discarded**: its preflight outlasted the subsequent test build, so `target/stage/aft` had been replaced by the unoptimized test binary before the subject spawned. It was interrupted through the Python driver's SIGINT/finally cleanup while still in cold-work gating; the checkout was restored and the owned lock released. No rows from that attempt are used for before/after publication claims. The final run instead points at the immutable optimized copy and fresh `target/hunt-final-optimized/storage`.
