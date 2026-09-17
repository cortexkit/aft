# In-situ materialization, 2026-09-16

## Before-fix capture

Captured at 17:17:59Z with the placed `~/.local/share/cortexkit/bin/ck-aft`
(16:25 build), unchanged product, fresh worktree-local storage, and
`AFT_VIEW_PROFILE=1 scripts/views-branch-drill.sh --mode views-on` against
`~/Work/OSS/opencode`. HEAD was `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`,
A `a085bf62a459c21d90a89bcae006e969df60d944`, B
`b85cf3d67fe3d36a8d04f4247fc781045d5b3dee`. Exclusive subject lock was held
outside the checkout for the entire drill, including restoration.
Raw evidence is worktree-local `.bg-shell/materialization-baseline/output/`.
No product fixes precede this capture. The placed binary's exact source revision
was not independently verified; these are not measurements of a rebuilt task branch.

### Phase breakdown (milliseconds)

The existing publication event already exposes nested phase timings from
`crates/aft/src/views/materialization/profile.rs`. `assembly.rs` brackets clone,
materialization call, and closure. Importantly, **`derived_ms` includes closure**;
adding closure to derived double-counts it in this binary.

| phase | HEAD→B graph | HEAD→B fill | B→HEAD graph | B→HEAD fill |
|---|---:|---:|---:|---:|
| manifest assembly | 3085 | 107 | 2963 | 149 |
| blob phase | 653 | 361 | 1478 | 883 |
| derived total (includes closure) | 9993 | 3692 | 22245 | 6954 |
| clone | 13 | 27 | 13 | 87 |
| materialization call | 7511 | 67 | 17825 | 162 |
| load bindings / dependent selection | 828 | 3 | 2687 | 9 |
| delete rows | 1223 | 0 | 3443 | 0 |
| owned blob decode / insert | 616 | 0 | 1922 | 0 |
| selected join (inclusive) | 2168 | 0 | 5071 | 0 |
| ↳ decode/bind index entries | 883 | 0 | 3138 | 0 |
| ↳ index / surface replay | 57 | 0 | 122 | 0 |
| ↳ decode resolved callers | 565 | 0 | 607 | 0 |
| ↳ resolve / record | 579 | 0 | 1016 | 0 |
| ↳ dependency union | 50 | 0 | 112 | 0 |
| write bindings | 261 | 0 | 610 | 0 |
| emit refs / edges | 1429 | 0 | 2299 | 0 |
| transaction commit | 151 | 2 | 283 | 2 |
| closure | 2467 | 3596 | 4403 | 6704 |

Selection, deletion, emission, and transaction commit are in `materialization.rs`;
join timers use the same `materialization/profile.rs` collector from the callgraph
join. The current counters do not isolate manifest diff, memo hit rate, index
rebuild, or fsync. Transaction commit is not a standalone fsync measurement.
Consequently this capture does **not** prove cold cache versus regression against
the offline 3.78 s figure. The graph materialization calls range from 7.511 to
17.825 s, and host load from 20.44 to 28.17, versus the supplied 10–16 baseline.
Index/surface replay alone is only 57/122 ms; row deletion and emission together
are 2652/5742 ms. Attributing the entire gap to resolver memo warmth is unsupported.

The fill does enter the materializer, but `manifest_callgraph_equivalent` already
skips graph rows and only advances metadata. Assembly still clones the derived
generation, opens a keeper, schedules its checkpoint, and validates full closure.
The measured fill bottleneck here is closure (3596/6704 ms), not graph emission.

### Four rows against supplied baseline

| switch | supplied views correct s | capture correct s | supplied views CPU s | capture CPU s | embeds | reported puts |
|---|---:|---:|---:|---:|---:|---:|
| HEAD→A | 15.6 | 37.191 | — | 66.18 | 3 | 0 |
| A→HEAD | — | 14.491 | — | 79.05 | 0 | 0 |
| HEAD→B | 22.2 | 24.758 | 49.2 | 62.03 | 1 | 0 |
| B→HEAD | 24.1 | 43.022 | 56.2 | 56.38 | 0 | 0 |

All four probes report correct, no defects, and return legs report zero embeddings
and puts. However, `puts` is not trustworthy on forward rows: HEAD→A's event
reports 269 puts and HEAD→B's reports 13, while the drill summarizes zero. The
A→HEAD window also contains three publications, including a pending-path count
that rises from 185 to 272. Row timing is therefore retained as observed, not
claimed to be a clean apples-to-apples acceptance result. No target is claimed met.

## Semantic-fill change and verification

The parent confirmed that the placed baseline executable was card 103 from
`651311547c3b`; the retained executable is `/tmp/card-wt/target/release/aft`.
The baseline stands, with the inclusive accounting correction above. This task's
candidate is built from task base `f5b953a33` plus the semantic-fill change, so it
is not a binary-identical baseline rebuild.

Commit `e5c3da010` makes a semantic-only publication reference the previous durable
derived owner through `derived-<generation>.ref`. It does not clone, open a derived
writer, change derived metadata, schedule a derived checkpoint, resync the derived
database, or checkpoint aliases. Closure validates only newly referenced blob
keys and includes only semantic blob durability. The existing full publication
path is unchanged. The next graph edit clones the actual owner and uses that
owner's manifest for its fingerprint precondition. Sweeping retains owners of
references and may reclaim an obsolete owner one sweep later. Ownership-reference
creation and sweeping serialize through the pointer transaction so a publisher
cannot release its base pin after an out-of-date ownership snapshot.

`crates/aft/src/callgraph_store/mod.rs::ReadonlyCallGraphStore::open_manifest_view`
needed the allowed materializer seam change: readers previously constructed a
`derived-<generation>.sqlite` filename directly and now resolve the shared owner.
No configure or runtime-drain code changed.

### Red-first and non-vacuity evidence

The strengthened existing test
`view_assembly_wiring_test::semantic_plane_follows_an_immediately_published_callgraph_plane`
failed before the fix with `semantic fill must reuse the durable callgraph database,
not clone or checkpoint it` (different generation paths). It now also checks
sweeping and a subsequent graph edit.

Mutation runs staged the live implementation before each change, confirmed an
empty working diff, applied a `NON-VACUITY BREAK`, captured the nonempty diff,
and restored from the index followed by `touch` and an empty working diff.

| control | named failure | result |
|---|---|---|
| Force old fill materialization | `views::assembly::semantic_fill_tests::semantic_fill_has_no_derived_writes_or_checkpoint_and_reader_uses_owner` | First attempt passed: the unit fixture had not tracked lib.rs. Fixed fixture to commit lib.rs and assert a pending initial publication and a completed fill. |
| Force old fill materialization, corrected fixture | same test | FAILED: `InvalidManifest("sqlite error: derived metadata rewritten")`; 0 passed, 1 failed |
| Schedule a shared-graph checkpoint | same test | FAILED: `semantic fill scheduled a callgraph checkpoint`; 0 passed, 1 failed |
| Remove publisher ownership serialization | `views::generation::ownership_tests::ownership_reference_waits_for_sweep_pointer_lock` | FAILED: `ownership reference escaped the sweep lock`; 0 passed, 1 failed |

No mutation remains. These controls establish actual derived-write avoidance and
checkpoint omission rather than just a matching path or a no-op publication.

### Gates

- `cargo test -p agent-file-tools --test view_assembly_wiring_test`: unavailable
  in this base; the file is a module of the `integration` target.
- Equivalent integration module: 6 passed. Publication/CAS, migration,
  schema, pins/GC, and staging selected integration modules: 44 passed.
- `cargo test -p agent-file-tools --test watcher_integration branch_switch`:
  2 passed (134.18 s). This real-watcher matrix is intentionally registered in
  the serial watcher binary, not the parallel integration binary. An accidental
  duplicate registration in integration/main.rs was reverted before committing.
- Views plus executor publication units: 56 passed, 3 opt-in benchmarks ignored.
- Callgraph join and executor publication units: 13 passed, including disk/manifest
  resolver row parity and publication/query isolation.
- `cargo rustc -p agent-file-tools --lib -- -D warnings` and the corresponding
  `--bin aft` command: passed on the final product code.
- Release build: passed. AFT inspection timed out in tier-2 rescan, so no clean
  AFT diagnostic result is claimed.
- `cargo clippy -p agent-file-tools --lib --bin aft -- -D warnings`: failed with
  63 existing warnings promoted to errors, including search-index
  `manual_is_multiple_of`, `chunks_exact_to_as_chunks`, and documentation list
  indentation. No unrelated lint cleanup was attempted. Compiler deny-warnings
  passed separately.
- The separately described 69-fixture gate was not identified/run as such;
  the parity tests above are not represented as a substitute fixture count.

## After-fix drill: blocked before transitions

Attempted the same exclusive-lock, fresh-storage, views-on drill with the locally
built `target/release/aft`. Storage:
`.bg-shell/materialization-after/storage`; output:
`.bg-shell/materialization-after/output/branch-drill-views-on.stderr.log`.
The drill exited 1 during the cold readiness phase before producing any switch
rows:

```text
cold work readiness failed while requesting the tier-2 dead-code pass:
inspect could not complete: inspect_request_timeout: tier2_rescan could not
complete within the 120000ms request budget (5000ms terminal reserve)
Completed phases: 98.
```

The existing drill restored the subject and the wrapper released its external
lock. There are **no after-fix four-row measurements**, and neither the ≤5 s graph
materialization target nor the relative CPU/time acceptance is claimed achieved.
The semantic-only unit is verified locally; its in-situ savings remain unmeasured.
Graph-changing publication still has the baseline selection/deletion/emission
costs shown above. No additional graph optimization or resolver cache rewrite is
included. The offline 3.78 s discrepancy remains unresolved, rather than being
attributed to load without evidence.

Next action: rerun with a fresh storage directory when the cold tier-2 readiness
pass can finish on this host, retain the clone/materialization/closure split, and
obtain the exact 69-fixture parity command from the owner. A change to the drill's
cold-readiness contract or runtime scheduling is outside this slice's product
changes. Review this delivery as a coherent **partial semantic-fill improvement**,
not a completed performance-target result.

## Continued investigation and final capture (22:29Z)

The earlier blocked attempt is historical; the drill now completes without
skipping cold readiness. The parent authorized two script fixes:

1. `branch_drill.py::log_metrics` previously overwrote `phase_puts` on every
   publication. A zero-put semantic fill erased the graph publication's 269/13
   puts. It now sums root-owned published/no-op phase events within the switch
   window, without double-counting legacy summary lines. Red-first test:
   `test_phase_puts_accumulate_across_graph_and_semantic_publications`, `0 != 282`.
2. The overall cold budget was already 1800 seconds. The failure was the engine's
   independent 120-second inspect-request deadline. `common.py::wait_cold_work_ready`
   now retries **only** `inspect_request_timeout` inside the original overall
   budget, still requiring completed dead-code evidence. Other errors remain
   fatal. Retry count and elapsed timestamps are retained in warmup JSON.
   Red-first test: `test_cold_gate_retries_only_inspect_timeout_and_requires_completion`.
   Both script mutations were restored and followed immediately by all six tests
   passing. The final real run needed zero retries and completed cold readiness
   in 582.555 s with explicit `--cold-work-timeout-s 1800`.

### The missing materialization tail was a real keeper bug

`materialization.rs` originally stopped its phase collector immediately after
transaction commit. Its caller measured the complete return, including cleanup.
New `cleanup_memory` and `cleanup_connections` phases exposed the difference.
On the 22:01Z in-situ B legs, memory destruction took 79/85 ms but connection
close took **578/722 ms**. This was not resolver work.

The assembly's keeper was only `Connection::open` plus `busy_timeout`. That does
not attach a SQLite pager to the WAL. Even adding `journal_mode=WAL` was
insufficient on a fresh database. The materializer was therefore closing the last
actual WAL connection and implicitly checkpointing before publication. The
offline benchmark, in contrast, executes WAL pragmas/checkpoint on its keeper.
This is a concrete offline/in-situ lifecycle mismatch, not just a warmer memo.

`assembly.rs` now reads `sqlite_schema` on the WAL-mode keeper before running
materialization. Red-first
`views::assembly::semantic_fill_tests::prepared_callgraph_retains_committed_wal_before_pointer_publication`
failed with `committed graph WAL was checkpointed on writer close before pointer
publication`. Removing only the schema read with `NON-VACUITY BREAK` fails that
same test while the semantic-fill test stays green. After restoring from the
staged live state, both tests pass. Final B-leg connection-close time is **2/1 ms**.
Durability is preserved: FULL transaction commit and pre-publication WAL fsync
remain in place; the existing post-publication checkpoint remains scheduled.

### Same-pair offline control: quantify, do not blame load generically

The copied baseline generation 6→7 pair is exactly the HEAD→B publication pair:
7060→7045 entries, 283 manifest changes for the 298-file checkout transition.
A SQLite backup of the stopped baseline arm's blob database supplies its payloads.
No live SQLite file was used as a benchmark writer.

`bench_real_manifest_diff` now samples load at the timed call, not before cargo
compilation, and compares original per-reference SQL against caller-batched reads
in the same process. The old implementation is available only in test builds.
The new `emission_lookup_queries` counter first measured the current per-ref
behavior; the cross-file parity fixture failed its caller-count bound (2 queries,
1 dependent caller). The implementation now loads existing reference states once
per dependent caller. The fixture's complete database snapshot still equals cold
materialization. Its exact-row-count test changed only the newly introduced
lookup count from 2 to 1, not any graph/dependency write expectation.

| same-pair sample | load 1m at measured call | old wall / CPU s | batched wall / CPU s |
|---|---|---:|---:|
| paired control, before final drill | 7.89→7.45 | 4.440 / 3.347 | 4.445 / 3.315 |
| immediately following 21:36Z drill | 5.96→5.72→5.90 | 4.406 / 3.283 | 4.946 / 3.695 |
| final code, after 22:29Z drill and test compilation | 2.76→2.70→2.56 | 4.107 / 3.054 | 3.909 / 3.034 |

Each pair matches cold rows exactly. Lookup SQL executions drop **57051→323**;
this is a work-bound improvement, not a large or consistently demonstrated wall
win. The last sample's delete / selected join / emission phases were
731 / 1273 / 816 ms. The first uninstrumented-load offline attempt took 15.344 s
wall / 7.894 CPU s; host load was 42.86 before compilation and 59.84 afterward,
so it cannot be assigned an exact at-call load and is not a controlled comparison.

The previous 3.78-second offline number is reproduced in shape by the 3.909 s
low-load result. At load near 6, offline was 4.4–4.9 s, versus in-situ
materialization 6.065/7.926 s before the keeper fix. The measured implicit close
checkpoint explains a real part of the mismatch; the final in-situ call also
runs concurrently with other root work whereas the offline replay does not.
These observations **do not prove that all remaining variation is host load**.
They do disprove a required full surface rebuild on every switch: the replay
rebuilt 284 of 4921 surfaces, selected 588 callers, and resolved 96259 refs / 40848
bindings. Index/surface replay itself remains only tens of milliseconds.

### Final four rows, against supplied card-103 table

Final product commit: `939797122`. Fresh arm storage and raw output:
`.bg-shell/materialization-keeper/{storage,output}`. Same HEAD/A/B identities as
baseline. The wrapper held `~/Work/OSS/opencode.aft-drill.lock` until the existing
drill restored the subject; the lock was released normally. All probes are
correct, `defects=[]`, and both return legs have zero puts and zero embeds.
No sibling byte-accounting changes have been integrated into this branch;
**in-situ per-phase bytes remain pending**, rather than duplicated here.

| switch | supplied views correct s | final correct s | supplied views CPU s | final CPU s | final load start→end | puts | embeds |
|---|---:|---:|---:|---:|---|---:|---:|
| HEAD→A | 15.6 | 12.836 | — | 54.00 | 5.31→5.63 | 269 | 3 |
| A→HEAD | — | 10.661 | — | 42.71 | 5.63→6.28 | 0 | 0 |
| HEAD→B | 22.2 | 13.564 | 49.2 | 49.00 | 6.28→6.60 | 13 | 1 |
| B→HEAD | 24.1 | 14.298 | 56.2 | 41.95 | 6.60→6.70 | 0 | 0 |

These lower-load rows are not a causal before/after estimate. Against the supplied
legacy rows, HEAD→B beats 33 s wall but not 38.2 CPU s; B→HEAD is still slower than
12 s wall but below the approximate fair 45 CPU s. Updated fairness-slice legacy
numbers have not been supplied to this branch. The 66.7 CPU-s embed-inclusive
shape is not substituted for a non-embedding B row.

| publication | derived total ms | clone ms | materialization call ms | closure ms | memory cleanup ms | connection cleanup ms |
|---|---:|---:|---:|---:|---:|---:|
| HEAD→A graph | 7319 | 15 | 5542 | 1760 | 82 | 2 |
| HEAD→A fill | 47 | 0 | 0 | 40 | 0 | 0 |
| A→HEAD graph | 6011 | 8 | 4663 | 1338 | 80 | 1 |
| A→HEAD fill | 60 | 0 | 0 | 48 | 0 | 0 |
| HEAD→B graph | 7603 | 9 | 5715 | 1876 | 88 | 2 |
| HEAD→B fill | 59 | 0 | 0 | 51 | 0 | 0 |
| B→HEAD graph | 8102 | 12 | 5552 | 2536 | 82 | 1 |
| B→HEAD fill | 47 | 0 | 0 | 41 | 0 | 0 |

`derived total` is inclusive. The target **is met for semantic fills** (47–60 ms
inclusive, zero materialization and no graph checkpoint). The graph-plane ≤5000 ms
target **is not met**, either inclusively or for both B materialization calls.
Remaining HEAD→B/B→HEAD phases are selection 579/562, deletion 807/833, owned
insert/decode 605/599, selected join 1999/1783, binding writes 181/207, emission
1284/1393, commit 161/84, memory cleanup 88/82 ms; graph closure adds 1876/2536 ms.
The remaining work is real row/index mutation, selected payload decode/resolution,
and durable graph closure—not the eliminated empty-fill rewrite or implicit
last-connection checkpoint. This delivery does not establish that no deeper
within-fence optimization is possible, and must not be stamped as full performance
acceptance. It provides a measured bucket answer and two proven lifecycle fixes.

### Final gates and restore evidence

- `cargo test -p agent-file-tools --lib -- views:: executor::view_publication::tests callgraph_store::join::`:
  66 passed, 3 opt-in benchmarks ignored (69 discovered tests, **not** a claim
  that this is the separately named 69-fixture parity gate).
- The selected publication/CAS/migration/schema/pins/staging/assembly integration
  command above: 44 passed on final product code.
- `cargo test -p agent-file-tools --test watcher_integration branch_switch`:
  2 passed on final code (126.84 s), including the return-leg embedding/put checks.
- Both compiler deny-warnings targets and release build passed on final code.
- Final offline paired replay passed full cold-row parity.
- Six Python drill tests passed after each script mutation restore.
- Caller-read-bound mutation failed only
  `incremental_rows_match_cold_with_cross_file_relink`, then that exact test passed
  after restore. Keeper mutation failed only
  `prepared_callgraph_retains_committed_wal_before_pointer_publication`, with the
  semantic-fill test green; both passed immediately after restore. Every mutation
  used the staged live state and recorded nonempty then empty working diff stats.
- Final AFT inspection completed its wait but reported unavailable Rust LSP
  diagnostics (broken pipe); compiler checks, not that incomplete report, are the
  authoritative diagnostic result.

## Materialization-internals continuation (2026-09-17, base 1ddc627dd766)

### Before any optimization: instrumented captured-pair replay

Input `.bg-shell/captured-pair/{base,next}.json` and its offline blob database were
copied by the owner from the retained HEAD→B capture: 7060→7045 entries, **283**
changed manifest entries. Only diagnostic counters were added before this run.
`AFT_VIEW_PROFILE=1 AFT_VIEW_DIFF_INPUT="$PWD/.bg-shell/captured-pair" cargo test
--release -p agent-file-tools --lib views::materialization::tests::bench_real_manifest_diff
-- --ignored --nocapture` passed complete cold-row parity for both lookup modes.
Raw output: `.bg-shell/offline-before-instrumented.log`. Two preceding compilation
attempts reached their 20-minute timeout; the successful build took 19m33s with a
one-hour budget. The timings below exclude compilation.

| phase / work | landed in-situ HEAD→B / B→HEAD | captured-pair offline HEAD→B |
|---|---:|---:|
| load bindings / selection ms | 579 / 562 | 567 |
| delete ms | 807 / 833 | 1085 |
| owned decode / insert ms | 605 / 599 | 553 |
| selected join ms | 1999 / 1783 | 2052 |
| ↳ decode / bind index entries ms | — | 737 |
| ↳ surface replay ms | — | 59 |
| ↳ decode resolved callers ms | — | 530 |
| ↳ resolve / record ms | — | 642 |
| ↳ dependency union ms | — | 55 |
| binding writes ms | 181 / 207 | 275 |
| emit refs / edges ms | 1284 / 1393 | 1357 |
| commit ms | 161 / 84 | 140 |
| materialization wall / CPU s | 5.715 / 5.552 (wall only) | 6.137 / 4.659 |
| closure ms (outside materializer) | 1876 / 2536 | not measured |
| surfaces restored / rebuilt | not captured | 4637 / 284 |
| rebuild reasons: changed / consulted facts / membership / missing | not captured | 265 / 19 / 0 / 0 |
| selected paths / resolved callers / replay-skipped callers | not captured | 1430 / 588 / 825 |
| unchanged dependents actually resolved | not captured | 323 |
| full_resolution / unattributed callers | not captured | false / 0 |
| identical dependent references skipped on emission | not captured | 55387 |
| dependent graph rows deleted / inserted | not captured | 3325 / 3327 |
| changed-owner graph rows deleted / inserted | not captured | 53914 / 51632 |

Selected paths include configuration/non-callgraph paths and are not a caller
count. The 588 callers include 265 changed callers and 323 unchanged dependents.
Thus **323 really is both the measured dependent count and the batched lookup
count on this pair**; the direct counters, not a SQL-count inference, establish it.
Fact-level invalidation is already implemented at this base (materialization
version 6). The 19 fact-invalidated surface rebuilds are not missing surface keys.
Stable dependent rows are already compared before writing; 55,387 reference
comparisons avoid deletion/insertion. Neither reimplementing fact invalidation
nor blanket dependent row comparison is a new optimization opportunity.

| byte attribution, keeper open | isolated offline HEAD→B | new in-situ capture |
|---|---:|---:|
| derived SQLite cache-write pages (4096-byte pages) | 74649 | pending |
| derived WAL bytes, before → after | 0 → 152955032 | pending |
| process physical bytes, materializer interval | 155062272 | pending |
| process logical bytes, materializer interval | 309316152 | pending |
| host load 1m, timed call start → end | 16.86 → 16.12 | pending |

These are literal bytes, not MiB. The offline physical cost is **155 MB**, not the
18–34 MB quoted from a different incremental workload; it cannot establish that
all daemon-window writes are unrelated writers. WAL pages include 50,000,320
frame bytes attributed to `view_bindings`, 19,269,240 to `refs`, plus graph indexes.
Cache-write pages include repeated spills, whereas WAL size is final file length;
they are deliberately separate measures. The process interval remains labelled
process-wide, including in the daemon. The isolated offline run removes other
daemon writers, not filesystem write-amplification or measurement timing effects.

### Fresh instrumented baseline drill (12:25:57Z)

The baseline executable was built before the caller-lookup change from the
instrumentation-only tree (commit `146359023`, formatting aside). Fresh storage
and raw JSON/stderr: `.bg-shell/in-situ-before/{storage,output}`. The wrapper held
`~/Work/OSS/opencode.aft-drill.lock`; the script restored the clone. All four rows
were correct, `defects=[]`; return rows put/embed zero. A release-test compilation
was running concurrently: these are not low-load timing controls.

| switch | landed wall s | baseline wall s | landed CPU s | baseline CPU s | load start→end | puts / embeds |
|---|---:|---:|---:|---:|---|---:|
| HEAD→A | 12.8 | 29.326 | 54 | 52.02 | 26.11→23.24 | 269 / 3 |
| A→HEAD | 10.7 | 13.270 | 43 | 14.28 | 23.24→19.86 | 0 / 0 |
| HEAD→B | 13.6 | 11.627 | 49 | 32.65 | 19.86→17.70 | 13 / 1 |
| B→HEAD | 14.3 | 12.772 | 42 | 28.65 | 17.70→23.25 | 0 / 0 |

| phase / counter | HEAD→A | A→HEAD | HEAD→B | B→HEAD |
|---|---:|---:|---:|---:|
| materialization call ms | 13319 | 8463 | 7463 | 8638 |
| closure ms, outside materializer | 137 | 37 | 69 | 41 |
| selection ms | 1033 | 1568 | 783 | 896 |
| delete ms | 1406 | 1188 | 1261 | 1178 |
| owned decode / insert ms | 892 | 896 | 787 | 692 |
| selected join ms | 4025 | 2282 | 2476 | 3194 |
| decode / bind index entries ms | 1004 | 970 | 1036 | 912 |
| decode resolved callers ms | 2076 | 538 | 632 | 578 |
| resolve / record ms | 751 | 616 | 645 | 1451 |
| emit refs / edges ms | 5021 | 1805 | 1530 | 1925 |
| restored surfaces | 4635 | 4635 | 4632 | 4637 |
| rebuilt changed / facts / membership / missing | 267/19/0/0 | 280/19/0/0 | 270/19/0/0 | 278/19/0/0 |
| resolved callers / unchanged dependents | 563/296 | 576/296 | 593/323 | 601/323 |
| selected paths / replay-skipped callers | 1621/1041 | 1621/1041 | 1635/1025 | 1430/825 |
| full_resolution / unattributed | false/0 | false/0 | false/0 | false/0 |
| identical dependent refs skipped | 53283 | 53283 | 55387 | 55387 |
| dependent rows deleted / inserted | 2949/2951 | 2951/2949 | 3325/3327 | 3327/3325 |
| cache-write pages (4096 bytes) | 67123 | 63461 | 57101 | 64097 |
| WAL bytes after (all before=0) | 149065752 | 128745912 | 132503352 | 130060192 |
| process physical bytes inside probe | 654708736 | 130875392 | 134635520 | 132194304 |
| process logical bytes inside probe | 768019078 | 263721740 | 237742244 | 267145460 |

The isolation comparison does **not** have identical SQLite counters: offline B
rebuilt 284 surfaces versus 289 in situ; the fresh in-situ diff touched 288 paths
versus 283 in the retained pair. The SQLite base also has a different physical
layout from a freshly cold-built offline base. Offline cache-write pages 74,649
and WAL 152,955,032 bytes differ from in-situ 57,101 and 132,503,352 bytes. Neither
run fell back to full resolution or unattributed reads. Thus this is not the
requested equal-work proof that the daemon excess is entirely other writers.
On these B legs, however, the actual in-materializer physical delta is about
133–135 MB and closely tracks its own WAL length, not 349–411 MB. HEAD→A visibly
overlapped the legacy callgraph build's final resolution and dispatch stage;
its process physical delta greatly exceeds the derived WAL. No attribution of
that excess to the derived writer is justified by process counters alone.
Closure is reported rather than modified: this base's 37–137 ms differs from the
landed historical 1.9–2.5 s and is not credited to a materialization-internal fix.

### Caller-node binding unit: bounded lookup, not another invalidation cache

The measured decode/bind bucket still builds changed and actually re-resolved
caller extracts. `ParseBlob::bind_with_dependencies` scanned every node to find
the caller of every reference. A per-extract scoped-name map now makes that work
linear in nodes plus references, preserving the **first** source-order node for
overloads/duplicate scoped names. No resolver selection, binding dependencies,
row schema, reference tuple, or publication boundary changes.

Red-first test `callgraph_store::join::binding_caller_node_lookup_is_linear_and_preserves_first_duplicate`
failed with **20,100** inspected nodes for 201 symbols and 200 refs. It passes with
401 indexed-node/lookup operations and checks caller-node IDs against the former
first-match semantics. The 46 selected materialization/join tests pass (2 opt-in
benchmarks ignored), including complete derived-table parity.

Mutation proof: the optimized source and test were staged, and `git diff --stat`
was empty. Restoring the symbol scan under `NON-VACUITY BREAK` produced
`join.rs | 7 ++++++-` (6 insertions, 1 deletion). Running the bound test alongside
`incremental_rows_match_cold_with_cross_file_relink` failed **only** the bound test
(20,501 operations); row parity remained green. `git checkout -- join.rs` followed
by `touch` restored the staged implementation with an empty diff. Recompiling and
running those exact two tests returned **2 passed**. Raw logs:
`.bg-shell/caller-node-{red,green,mutation,restored}.log`. No mutant remains.

The first after replay (`.bg-shell/offline-after.log`) passed cold-row parity but
is **not a matched-load speedup claim**: load 28.10→27.38 versus 16.86→16.12 before.
Incremental wall/CPU was 8.536/5.956 s versus 6.137/4.659 s. Selected join was
2113 ms (decode/bind 808, caller decode 530, resolve 625) versus 2052 ms before.
All work counts, 74,649 cache-write pages, and 152,955,032 WAL bytes are unchanged.
The unit proves a work bound, not a demonstrated in-situ wall-time improvement.

### Paired offline scan/index control on final code

The ignored real-pair benchmark now runs both caller-node lookup modes in the
same process, in addition to the existing per-reference SQL control and cold
writer. The scan control is test-only and retains the index construction, so its
small extra construction cost makes it conservative rather than a binary-exact
old-implementation timing. All four resulting databases match every cold table.
`.bg-shell/offline-paired.log` records the final run:

| same-process arm | load 1m start→end | wall s | CPU s | selected join ms | bind index ms | caller decode ms | physical bytes | logical bytes | WAL bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| previous node scan, batched SQL | 23.56→22.95 | 6.200 | 4.728 | 1932 | 776 | 508 | 155062272 | 309402168 | 152955032 |
| indexed caller lookup, batched SQL | 22.95→22.47 | 5.506 | 4.377 | 1773 | 703 | 467 | 155095040 | 309037624 | 152955032 |

This is a nearby-load paired observation, not proof that all 694 ms of wall
variation belongs to lookup (deletion and emission also varied). Work, row counts,
full-resolution=false, unattributed=0, and WAL bytes are identical. The final
indexed offline call is still **above five seconds**. Its remaining phases are
selection 616, deletion 1034, owned insert 657, selected join 1773, bindings 200,
emission 1024, commit 105, memory cleanup 88 ms.

### Final fresh-storage views-on drill (14:05:10Z)

Production code is `f94f2482d`; subsequent changes only add the test-build paired
control above and this report. `cargo build --release -p agent-file-tools --bin
aft` passed before running the drill. Storage/output:
`.bg-shell/in-situ-after/{storage,output}`. The wrapper acquired the external
opencode lock, the drill restored the subject, and the wrapper released the lock.
All four probes are correct and `defects=[]`; both return legs have zero puts and
embeds. **Performance acceptance is not achieved.**

| switch | landed correct s | final correct s | landed CPU s | final CPU s | load start→end | puts / embeds |
|---|---:|---:|---:|---:|---|---:|
| HEAD→A | 12.8 | 36.977 | 54 | 65.41 | 40.14→38.68 | 269 / 3 |
| A→HEAD | 10.7 | 13.977 | 43 | 28.48 | 38.68→34.57 | 0 / 0 |
| HEAD→B | 13.6 | 23.280 | 49 | 29.96 | 34.57→30.73 | 13 / 1 |
| B→HEAD | 14.3 | 26.446 | 42 | 28.01 | 30.73→33.10 | 0 / 0 |

HEAD→A contains **two graph publications**, 13,072 and 7,838 ms materialization,
not one; its final correctness row must not be paired with just the first call.
The B legs have the following actual per-publication measurements:

| phase / counter | final HEAD→B | final B→HEAD |
|---|---:|---:|
| derived inclusive ms | 14310 | 17829 |
| materialization call ms | 14155 | 17643 |
| closure ms (reported only) | 130 | 161 |
| selection ms | 3123 | 1811 |
| deletion ms | 2839 | 2291 |
| owned decode / insert ms | 955 | 924 |
| selected join ms | 2486 | 4794 |
| ↳ bind index entries ms | 971 | 2192 |
| ↳ surface replay ms | 65 | 76 |
| ↳ caller decode ms | 729 | 894 |
| ↳ resolve / record ms | 615 | 1106 |
| ↳ dependency union ms | 63 | 359 |
| binding writes ms | 727 | 1144 |
| emit refs / edges ms | 3517 | 5500 |
| commit ms | 191 | 376 |
| memory / connection cleanup ms | 298 / 4 | 776 / 6 |
| restored surfaces | 4632 | 4637 |
| rebuilt changed / facts / membership / missing | 270/19/0/0 | 278/19/0/0 |
| resolved callers / unchanged dependents | 593/323 | 601/323 |
| selected paths / replay-skipped callers | 1635/1025 | 1430/825 |
| full_resolution / unattributed callers | false/0 | false/0 |
| identical dependent references skipped | 55387 | 55387 |
| dependent rows deleted / inserted | 3325/3327 | 3327/3325 |
| SQLite cache-write pages (4096-byte pages) | 61738 | 60087 |
| derived WAL before → after bytes | 0→132486872 | 0→129994272 |
| process physical bytes inside probe | 743448576 | 537341952 |
| process logical bytes inside probe | 1569821170 | 926211644 |

The later daemon observation shows why process-wide bytes cannot be assigned to
the derived file: its own WAL remains ~130 MB and cache writes ~60k pages, while
the process interval grows to 537–743 MB physical and 926–1570 MB logical. The
before drill's B legs had the same logical row workload but 57,101/64,097 cache
writes, reflecting different spill/layout history. The retained offline pair
has 283 changes and 588 resolved callers, versus 288 and 593 on the in-situ
forward leg; it is not an equal-counter isolation proof. The evidence supports
reporting **both scopes separately**, not declaring the byte attribution closed
or attributing the entire difference to one unidentified writer. The final
process-wide numbers also must not be compared to the alleged 18–34 MB offline
cost: this same retained pair measured ~155 MB physical in isolation.

**Remaining measured bottleneck:** final B emission is 3517/5500 ms; selected
join is 2486/4794 ms, including bind/index 971/2192 ms. Deletion adds 2839/2291 ms.
Closure adds only 130/161 ms in this base and was not changed. Higher host load
and different publication splitting prevent treating this drill as a causal
before/after regression estimate. It nevertheless fails the ≤5 s criterion and
the supplied ~12 CPU-s legacy transition criterion. No widening of the fence,
reader changes, or legacy-plane retirement is included in this delivery.

### Final verification

- `cargo test -p agent-file-tools --test integration tool_call_parity_test`:
  12 passed, including the owner-confirmed 69-fixture
  `tool_call_matches_direct_spine_envelopes` matrix, with default views-off.
- `cargo test -p agent-file-tools --test watcher_integration branch_switch`:
  2 passed (134.20 s), including views-on round-trip reuse and the branch matrix.
- `cargo test --release -p agent-file-tools --lib -- views::materialization::tests
  callgraph_store::join::`: 46 passed, 2 ignored, on final test-control code.
- Real captured-pair cold-table parity: passed before, after, and in the final
  same-process scan/index control. Selected mutation/restore: only the named
  work-bound test red, then both selected tests green after checkout/touch.
- Release executable build and Rust compilation through all test targets passed.
  Two scoped AFT inspections timed out in tier-2 rescan; no clean LSP report is
  claimed. One benchmark comment flagged in review was clarified; formatting
  and diff checks passed.
- No separate views-on version of the 69-fixture route/envelope harness was
  introduced: this is a binding implementation/work-bound change with unchanged
  cold rows, not a reader-return contract change. Views-on behavior is exercised
  by the existing watcher matrix and both real drills.
