# Tier-2 dead-code projection memory — September 2026 (issue #330)

## Result

The reporter-scale run reproduced the memory class, but not the reporter's exact
15.6 GB peak. On a 37,358-file Linux-kernel subset with 2,357,263 projected
outbound calls, the largest of three fresh-process runs reached **7,774,257,152
bytes peak RSS** and **7,247,796,200 bytes peak `phys_footprint`** (`/usr/bin/time
-l`). The three peak-RSS observations were 6.638, 6.762, and 7.774 GB. These are
separate metrics; no conversion or assumed ratio was used.

The dominant allocation was not one of the inventory's three floors. During each
cold reachability-state build, `dispatch_live_source_names_by_file` materialized
every non-Go file crossed with every receiver-dispatched method name for that
file's language. The reporter-scale corpus produced **62,661,141 `(file,
method-name)` set entries** from 3,119 method names. The first of the two
reachability builds raised allocator `size_allocated` by **5,012,193,280 bytes**
while live allocator bytes rose by only 684,224 bytes after the function
returned. The temporary map had already been freed at the checkpoint; macOS's
allocator retained its pages as slack, and process RSS stayed high through the
end of the run. At the full 5,551,215-edge point the same fanout was 185,980,712
entries and peak RSS was 20,343,406,592 bytes.

The snapshot, materialized `internal_calls`, and two reachability edge maps were
substantial but linear. At reporter scale their measured allocator-live costs
were 752.0 MB, 123.1 MB, and 291.4 MB respectively. Bounding only the snapshot
would therefore miss both the downstream resident maps and a roughly 5.0 GB
allocation high-water caused by the dispatch projection.

## Corpus and method

The source corpus was `~/Work/OSS/linux` at commit
`89a312991dc6e638a36adc43ccb91dbc25504c04`. It had unrelated pre-existing
working-tree changes; AFT read it but this investigation did not modify it. A
fresh isolated `aft index` callgraph build used the staged binary from base
`3275db65cbf2a7f1bd4de2ecfb10bc92fe15326f`, an isolated `AFT_STORAGE_DIR`, and
`nice -n 10`. The build took 1,635.21 seconds after resuming its staging store,
reported 693,075,968 bytes maximum RSS and 572,572,992 bytes peak
`phys_footprint`, and produced an 11,195,531,264-byte SQLite generation.

The complete generation contained:

| counter | rows |
| --- | ---: |
| files | 73,319 |
| nodes | 7,204,989 |
| refs | 5,922,357 |
| persisted resolved edges | 1,075,852 |
| exported/default-export nodes | 6,591 |
| outbound calls emitted by the dead-code projection | 5,551,215 |

The last number is the `CallgraphSnapshot.outbound_calls` length, and is the
"edges" number used below and by `perf tier2_callgraph_snapshot`. It is not the
SQLite `edges` table count: projection also emits unresolved calls and value
references from `refs`.

The smaller points were copy-on-write clones of that completed generation. I
kept the lexicographically first N `files`, deleted `refs` whose caller was not
in that set, and deleted out-of-set `nodes` and `files`. This gives exact file
and projection counters without paying for three more cold parses. It is not a
random or representative Linux sample; the 23,000-file point in particular
contains a node-heavy prefix. The 37,358-file point was selected to match the
reporter's file count and landed at 2.36 million projected calls, close to the
reported 2.54 million.

Each measurement ran in a fresh process with an empty symbol cache and eight
Rayon threads. A 20 ms phase sampler was not used for attribution: checkpoints
inside production projection and rollup code read macOS task RSS,
`TASK_VM_INFO.phys_footprint`, and default-zone `malloc_zone_statistics` after
named allocations. `/usr/bin/time -l` independently recorded process peaks.
The harness called production `project_dead_code_snapshot` and
`run_dead_code_scan`; temporary instrumentation changed no product decisions or
thresholds.

## Scale curve

All memory columns below are measured except `snapshot estimate`, which is
computed by the production estimator over the resulting vectors. Peak columns
`/usr/bin/time -l` metrics from fresh processes. The structure columns are
allocator `size_in_use` deltas between adjacent phase checkpoints.

| files | nodes | projected calls | exports | snapshot estimate | snapshot live delta | materialized/internal-call delta | two edge-map deltas | dispatch projected names | peak RSS | peak `phys_footprint` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10,000 | 137,428 | 117,007 | 167 | 28.0 MB | 39.7 MB | 7.3 MB | 15.6 MB | 612,704 | 253.2 MB | 227.3 MB |
| 23,000 | 5,087,312 | 1,019,270 | 535 | 242.3 MB | 322.7 MB | 49.6 MB | 117.9 MB | 18,141,305 | 2,542.4 MB | 2,137.6 MB |
| 37,358 | 5,733,439 | 2,357,263 | 996 | 566.4 MB | 752.0 MB | 123.1 MB | 291.4 MB | 62,661,141 | 7,774.3 MB | 7,247.8 MB |
| 73,319 | 7,204,989 | 5,551,215 | 6,591 | 1,314.9 MB | 1,741.8 MB | 298.6 MB | 691.3 MB | 185,980,712 | 20,343.4 MB | 18,254.3 MB |

The resident structures scale approximately with projected calls:

- Snapshot allocator-live bytes were 314–339 bytes per projected call above the
  smallest point. `outbound_calls` accounted for 737.6 MB of the 752.0 MB
  reporter-scale snapshot delta; `files` cost 14.1 MB, exports 0.2 MB, and
  entry-point symbols were below 0.01 MB.
- Materialization, whose retained addition is predominantly `internal_calls`,
  cost 49–54 bytes per projected call at the three larger points.
- The all-calls and production-only edge maps together cost 115–125 bytes per
  projected call at the three larger points.
- Dispatch projection did **not** scale directly with calls. Projected names per
  call rose from 5.2 to 17.8, 26.6, and 33.5 across the four points. Its
  allocator high-water cost stayed near 80–86 bytes per projected name. The
  expression that predicts this allocation is therefore the sum, by non-Go
  language, of `files_in_language × dispatched_method_names_in_language`, not
  files, nodes, or callgraph edges alone.

The full point's RSS growth is faster than its edge growth for exactly this
reason. The fanout ratio changes with corpus shape, so an edge-only admission
bound cannot predict the peak.

## Reporter-scale phase attribution

This table uses the first reporter-scale run's checkpoints and largest observed
peak. RSS and `phys_footprint` are process totals, while allocator columns are
default-zone totals. A delta is attributed only where adjacent checkpoints
isolate that allocation; totals are not forced to sum.

| phase / resident allocation | process RSS at checkpoint | `phys_footprint` at checkpoint | allocator live delta | allocator `size_allocated` delta | interpretation |
| --- | ---: | ---: | ---: | ---: | --- |
| clean process, before projection | 11.3 MB | 5.3 MB | — | — | no symbol cache or prior snapshot |
| `files` vector complete | 27.7 MB | 21.6 MB | +14.1 MB | +20.0 MB | retained snapshot paths |
| `exported_symbols` complete | 28.0 MB | 21.8 MB | +0.2 MB | 0 | this corpus has only 996 exports |
| `outbound_calls` complete | 2,005.9 MB | 1,479.6 MB | +737.6 MB | +1,355.7 MB | dominant retained snapshot vector; SQLite row/query temporaries and allocator slack account for the larger process delta |
| parsed contributions → materialized contributions | 2,446.6 MB | 1,917.5 MB | +123.1 MB | +117.4 MB | retained materialization, predominantly `internal_calls` |
| all-calls edge map | 2,595.2 MB | 2,066.2 MB | +146.0 MB | +146.8 MB | first retained reachability map |
| production-only edge map | 2,743.6 MB | 2,214.7 MB | +145.4 MB | +151.0 MB | second retained reachability map |
| first reachability state returned | 7,083.4 MB | 2,742.5 MB | +0.7 MB | **+5,012.2 MB** | 62.7 million-name dispatch map was temporary and freed; allocator slack retained its high-water allocation |
| cold rollup complete | 7,208.7 MB | 2,165.8 MB | +157.0 MB after reachability | +136.5 MB after reachability | fragments, aggregate, and returned state |
| process peak | **7,774.3 MB peak RSS** | **7,247.8 MB peak `phys_footprint`** | not sampled at the exact instant | not sampled at the exact instant | transient peak occurred inside the first reachability build |

`traverse_reachable` itself was not the high allocation. Instrumentation counted
2,393 expanded nodes, 3,140 total queue insertions, and a 1,542-node peak queue.
The allocation occurred before traversal while constructing
`dispatch_live_source_names_by_file`: 3,119 distinct dispatched method names
became 62,661,141 per-file set entries. The same projection is built for both
`all` and `production`, but the allocator reused the first build's retained
space for the second.

The high-water moment is brief in `phys_footprint`: after the first reachability
function returned, checkpoint `phys_footprint` was down to 2.74 GB. RSS remained
above 7 GB through rollup completion because allocator `size_allocated` remained
7.24 GB while live allocator bytes were 1.43 GB. Thus the semantic map is
transient, but the process-level RSS consequence is a sustained plateau for the
rest of this request unless allocator relief runs. The two metrics crossed in
other phases and runs; neither was derived from the other.

## Inventory hypotheses

The earlier structural estimates do not reproduce exactly:

| inventory hypothesis | prior estimate | measured reporter-scale value | result |
| --- | ---: | ---: | --- |
| `internal_calls` | 176.92 MiB | 117.44 MiB allocator-live delta | 33.6% lower on this corpus |
| reachability edge maps | 232.66 MiB | 277.92 MiB combined allocator-live delta | 19.5% higher |
| old/new rollup overlap | 819.16 MiB | 0 for this cold path | structurally absent because `previous` is `None`; this estimate applies to incremental replacement, not the reporter's cold run |
| unlisted dispatch projection | not inventoried | 4,780 MiB allocator `size_allocated` rise at the post-function checkpoint; 62.7 million temporary entries | larger than all three inventory floors combined |

The internal-call number is a phase delta, not a per-object heap census: the
materialized contribution also gains liveness roots, imported exports, and
method-name vectors. It is therefore an upper bound on `internal_calls`, not a
claim that every byte in the delta belongs to that field. Conversely, the edge
map checkpoints isolate each `edges_by_source` result closely.

## Estimator and memory-census comparison

At reporter scale, `estimate_callgraph_snapshot_bytes` returned 566,371,678
bytes. The snapshot's measured allocator-live delta was 751,978,960 bytes, so
the estimator was **24.7% below allocator-live bytes**. Snapshot-complete process
RSS was 2,005,975,040 bytes, **3.54× the estimate**. Some of that gap is query
working memory and allocator slack rather than the retained vectors, but it is
real process RSS during admission.

The undercount was stable at scale: 24.9%, 24.7%, and 24.5% below allocator-live
snapshot deltas at 1.02, 2.36, and 5.55 million calls. The smallest point was
29.4% low. The estimator mostly omits collection capacity, tree-node allocation
overhead, and allocator rounding; it cannot represent projection-query
transients.

`callgraph_projection_snapshot_bytes` is not an independent measurement. The
manager assigns it the exact return value of
`estimate_callgraph_snapshot_bytes`, so when a snapshot is retained both values
agree by construction. In a production-daemon run the full snapshot logged
73,319 files / 6,591 exports / 5,551,215 calls in 80,020 ms, but the immediately
following `status` census reported `callgraph_projection_snapshot_bytes: 0` and
zero snapshots. The 1,314,936,022-byte estimate exceeds the process-wide 1 GiB
snapshot fleet budget and the cache slot had already been evicted, while the
scan worker's `Arc` still held the snapshot and downstream rollup allocations.
Consequently the live census counter can read zero during or immediately after
the expensive request. It is neither a peak counter nor an admission measure
for downstream allocations.

For comparison only, an operator captured the shared daemon during the isolated
index build and saw 1,724.2 MB attributed to the Linux root's symbol plane, with
callgraph and inspect both zero. That is an external observation, not a harness
result. The fresh-process projection measurements above intentionally had an
empty symbol cache, so none of their 7.77 GB reporter-scale peak is the 1.7 GB
symbol plane. In a normal long-lived daemon these costs can stack: a resident
symbol cache would be baseline memory beneath the projection, not part of the
projection allocation itself.

## Phase timing

At reporter scale the cold snapshot took 50,417 ms in the final instrumented
run, closely matching the reporter's 50,465 ms. Most snapshot RSS arrived while
reading `outbound_calls`; files and exports were small. File contribution
collection and parsing followed. The first dispatch projection began after both
edge maps existed and was the high-water phase. The complete direct run took
104.75 seconds in that repeat.

A normal `aft_inspect` request against the full 5.55-million-call generation hit
the 120-second Tier-2 request budget. Its snapshot completed in 80,020 ms, then
the request returned `inspect_request_timeout` before rollup completed. The
direct harness was required to observe the full cold path without changing that
product timeout.

## What remains unattributed or unmeasured

- The exact instantaneous object-level composition at `/usr/bin/time`'s peak is
  not available. Checkpoints bracket it, and the 5.0 GB allocator high-water is
  isolated to dispatch projection, but no heap stack sampler was enabled.
- Snapshot-complete process RSS exceeded allocator-live snapshot bytes by 1.25
  GB at reporter scale. The measured allocator slack explains 642 MB; the
  remaining roughly 610 MB includes SQLite/query working memory, non-default
  zones, stacks, mappings, and accounting timing. It remains unattributed.
- The Linux subset has only 996 projected exports versus the reporter's 331,300.
  It matches files and projected calls but not symbol shape. The dispatch fanout
  depends on receiver-call method names rather than exported-symbol count, so
  this mismatch could move the peak materially in either direction. The
  reporter's 15.6 GB is therefore not disproved by the 7.77 GB local peak.
- `entry_point_symbols` was negligible here, and the 37,358-file prefix had zero
  file entry points. The complete corpus had 50. This corpus cannot price a
  reporter-like 44-entry-point set independently, though that structure is far
  too small to explain gigabytes.
- A full cold run has no old rollup, so the inventory's 819.16 MiB replacement
  overlap could not be measured on the requested path. Measuring it would be a
  separate incremental experiment.
- The shared-daemon symbol-cache observation was not repeated inside the
  isolated harness. Its 1.7 GB should not be added to the measured peak as if
  sampled simultaneously.

## Measurement artifacts and teardown

Raw stores, cloned subsets, daemon logs, and samples lived under this
worktree's ignored `.bg-shell/` directory. Production source instrumentation and
the one-off example were committed during measurement so a host restart could
not lose the work, then removed from the final tree. No limiter or threshold was
changed. Daemon measurement children were stopped with `SIGKILL`, not `TERM`, to
avoid the known full-store shutdown rewrite. After the kill, the source store
still reported exactly 73,319 files, 7,204,989 nodes, 5,922,357 refs, 1,075,852
persisted edges, and 6,591 exports; the measurements did not assume the store
remained unchanged.
