# Shared semantic bases with root-local deltas

## Representation and persistence

`SemanticIndex` now retains its immutable `Arc<SharedSemanticBase>` through
invalidation and refresh. The base keeps relative file identities. Each root
owns only replacement entries, freshness overrides, deferred paths, and a set of
relative base-file tombstones. Re-adding a deleted base file does not unhide its
old vectors: the replacement remains in the delta.

Search traverses surviving base entries followed by delta entries. This is the
same retain-then-append order as the previous materialized representation, so
entry-index tie breaks, score calculation, and top-K selection are unchanged.
The comparison oracle is an independent **private, fully materialized index
following the same edit sequence**. A fresh cold rebuild is not an ordering
oracle for an incremental index: incremental replacement appends chunks, and
existing tie breaks intentionally depend on that order.

The serving/worker fork freezes even the owner's cold index before cloning it.
This avoids the owner's initial full worker copy as well as borrower copies on
the first edit. A shared worker refresh stages only its delta and commits on
success. The old base and the worker's previous delta supply embedding reuse;
a backend failure does not discard that reuse state. Late borrowers of an edited
donor inherit its tombstones and rebased delta, not a newly loaded full disk
corpus. Queries continue reading the serving index, and publication still uses
complete refresh events. Watcher
invalidation continues to hide stale files immediately, as before; the serving
state at refresh start is not partially replaced by an in-flight worker.

Serialization reads base-minus-tombstones plus delta through borrowed entry
references. Full snapshots stream through the existing writer; replacement
segments clone only the selected files. No persistent flat copy is created, and
the V7 disk layout and borrow-only write authorization are unchanged. A new
borrower does not copy the owner's pending dirty-path set. A writer without its
own dirty-path baseline uses the existing structural artifact comparison.

The separately integrated worker census (`eae2261c4`) counts private worker
payload using the same delta-aware estimate as serving indexes. Tombstones are
included in local metadata estimates, and shared base payload remains attributed
once through the existing weak registry. Counts distinguish live entries, base
entries, delta entries, and tombstoned files. Estimates still exclude allocator
capacity/overhead and transient refresh/serialization buffers.

## Regression verification

All commands ran in the isolated task worktree, with native builds serialized
and `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=`.

- `cargo test --locked -p agent-file-tools --lib semantic_index::tests -j 2`:
  **123 passed, 1 ignored**.
- `cargo test --locked -p agent-file-tools --lib semantic -j 2`:
  **325 passed, 2 ignored**, including both worker-census lifecycle tests.
- `cargo test --locked -p agent-file-tools --lib shared_overlay -j 2`:
  **4 passed** after strengthening the oracle to rebuild independently from each
  root's original files and recorded timestamps, and after restoring the final
  old-code red control.
- `cargo build --locked -p agent-file-tools --bin aft -j 2`: passed for the
  final implementation, including production typechecking.
- `cargo build --locked -p agent-file-tools --example malloc_soak -j 2`: passed
  for the warmup-enabled measurement broker.
- `cargo check --locked -p agent-file-tools --lib --example malloc_soak -j 2`:
  passed for the persistence-count and harness follow-up; the subsequent final
  native build above also typechecked the late-donor change.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

New regression cases:

- `shared_overlay_equivalence_and_sharing`: owner plus ten borrowers; edit,
  addition, deletion, re-addition, second edit; equal serialized bytes and equal
  complete search-result debug bytes for positive, zero, and negative scores at
  three top-K depths. All fixture vectors tie. Serving and worker keep the same
  base allocation, own fewer than one quarter of corpus entries, and worker
  estimated bytes stay below one third of the private corpus. Dropping the last
  indices makes the base's `Weak` unupgradeable.
- `shared_overlay_late_borrower_inherits_only_donor_delta`: a borrower arriving
  after donor replacements, deletion, and addition inherits the complete logical
  snapshot, keeps the donor/worker base allocation, owns only two delta rows out
  of a 64-row base, projects paths into its own root, and mutates independently.
- `shared_overlay_corpus_refresh_and_segments_match_private`: the real
  serving/worker fork, corpus refresh, empty-symbol file, deletion, unchanged
  serving snapshot, full snapshot round trip, and replacement-segment replay.
- `shared_overlay_slow_failed_refresh_is_isolated`: a channel gate holds the
  embedder while queries observe the previous serving snapshot; a backend error
  leaves both serving and worker snapshots unchanged.

The existing batch-invalidation test's representation assertion was changed
from "materialized" to "still shared". Its serialized-byte/vector parity checks
remain; the private oracle is now retained before freezing rather than invoking
the removed materializer.

### Old-code red control

After staging the live implementation and confirming an empty unstaged diff,
restored production `semantic_index.rs` and `configure.rs` from `eae2261c4`, while
retaining only the new sharing test. The deliberate break was explicitly marked
in the temporary source. The final applied diff was nonempty (2 files,
314 insertions, 553 deletions). Running

```sh
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo test --features test-hooks --locked \
  -p agent-file-tools --lib shared_overlay_equivalence_and_sharing -j 2
```

produced exactly one failure:

```text
test semantic_index::tests::shared_overlay_equivalence_and_sharing ... FAILED
thread 'semantic_index::tests::shared_overlay_equivalence_and_sharing' panicked:
edits must not materialize the base
test result: FAILED. 0 passed; 1 failed; 0 ignored; 3988 filtered out
```

No other test ran in that filtered control. Both files were restored with
`git checkout -- <paths> && touch <paths>`; the subsequent unstaged diff was
empty. The restored implementation passed 324 semantic tests; the final suite
passes 325 after adding the late-donor regression.

The baseline production source was also built into a separate executable in the
earlier control pass for the paired measurement below. The red control was
repeated with the final independently rebuilt per-root oracle.

## Measurement protocol

Both binaries are unoptimized, debug-information builds, rather than the earlier
investigation's optimized `stage` binaries. This keeps the **paired** comparison
on identical compiler settings without claiming identical native footprint to
the earlier experiment. The `malloc_soak` broker/driver are from `eae2261c4`, with
one harness-only addition: `AFT_SOAK_OWNER_WARMUP_SECS` can extend the owner's
initial warmup before borrowers bind. The driver is unchanged. The debug broker
was installed at `target/stage/examples/malloc_soak`
(the driver's fixed broker path); `AFT_SOAK_BINARY` selects the actual executable.
Both independent workload clones are pinned to `eae2261c4`, with one owner and
ten linked worktrees, 768-float deterministic HTTP embeddings, inspect/callgraph
disabled, and the real five-minute idle TTL.

```sh
AFT_SOAK_SCALING=1 AFT_SOAK_BINARY="$PWD/target/overlay-evidence/aft-before" \
  python3 docs/investigations/malloc_soak.py .malloc-experiment/before
AFT_SOAK_SCALING=1 AFT_SOAK_OWNER_WARMUP_SECS=300 \
  AFT_SOAK_BINARY="$PWD/target/overlay-evidence/aft-final" \
  python3 docs/investigations/malloc_soak.py .malloc-experiment/overlay-final
```

The paired runs use the same loaded fleet host. These are retained-payload and
footprint measurements, not latency benchmarks. Only each broker's recorded
child PID is profiled. No production daemon was attached or
signaled. Raw wire responses, native profiler samples, and daemon logs remain in
the ignored `.malloc-experiment/{before,overlay-final}/` directories; build/test and
ranking outputs remain in `target/overlay-evidence/`.

An initial after trial in `.malloc-experiment/overlay/` was discarded: the
120-second warmup bound two borrowers before the owner had published its semantic
index. Those two never adopted a semantic index, so zero owned bytes were **not**
a valid improvement. Its nominal 1/5/10 milestones exercised only 0/3/8 semantic
deltas. The validated repeat uses a 300-second warmup and checks per-borrower
worker bytes at every milestone, rather than inferring coverage from successful
write responses. No product readiness/bind behavior was changed for this test.

AFT's scoped diagnostic inspection timed out in `lsp_quiescence` after 120 seconds;
the successful Rust test and production compilations above are the authoritative
typechecking evidence.

## Paired scaling results

The workload corpus is **38,547 chunks / 1,774 files**, with 768 floats per
vector. The shared base is **151,187,360 bytes** in both processes at every bound
milestone, counted once rather than once per root. The following estimates are
private payload only; worker totals include the owner's worker.

| Edited borrowers | Before: serving bytes | Before: worker bytes | After: serving bytes | After: worker bytes |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 0 | 156,802,769 | 0 | 0 |
| 1 | 157,070,048 | 313,872,817 | 4,277 | 4,277 |
| 5 | 785,350,240 | 942,153,009 | 21,385 | 21,385 |
| 10 | 1,570,744,384 | 1,727,547,153 | 42,805 | 42,805 |
| 10, after 100 more writes | 1,570,744,374 | 1,727,547,143 | 42,795 | 42,795 |

At ten edited borrowers, private semantic payload falls from **3,298,291,537**
to **85,610 bytes**. Including the common base, total attributed semantic payload
falls from **3,449,478,897** to **151,272,970 bytes** (about 95.6%). Each new edited
borrower previously added about 314 MB of private payload; it now adds about
8.6 KB for this one-chunk edit. Freezing before the worker fork also removes the
owner's initial 156.8 MB private worker copy.

The final after run records exactly **1, 5, and 10 borrowers with nonzero worker
deltas** at the corresponding milestones, and ten resident semantic adoptions.
Both paired runs completed 119 successful writes with zero wire errors and no
logged semantic refresh failures. The additional 100 writes reduce private
payload by 20 bytes in both implementations because replacement text lengths
change; neither accumulates another corpus on every edit.

| Edited borrowers | Before: process footprint bytes | After: process footprint bytes |
| ---: | ---: | ---: |
| 0 | 902,186,616 | 636,028,416 |
| 1 | 1,314,998,192 | 636,290,560 |
| 5 | 2,979,613,864 | 639,043,096 |
| 10 | 5,060,612,824 | 642,631,192 |
| 10, after 100 more writes | 5,062,939,352 | 643,941,912 |

The ten-borrower process footprint is about 87.3% lower. Footprint includes
other caches, allocator retention, and profiling data; it is not the semantic
payload estimate and does not imply every byte of the difference belongs to
semantic vectors. This is a fixed corpus with deterministic HTTP embeddings,
not a real-model or unlimited-root-cardinality memory bound.

### Unbind and TTL release

The baseline child was PID **69093** and the final implementation child was PID
**40497**. In both runs, all worker-owned bytes were zero in the first post-unbind
snapshot. At `idle-360.json`, all eleven roots had zero bound routes, zero serving
semantic bytes, zero worker bytes, and **zero shared semantic base bytes**. Both
logs contain eleven idle-root evictions. Root registrations remain, as designed;
they no longer retain semantic artifacts.

The final after run still held its serving deltas and base at the `idle-300`
snapshot, and released them by `idle-360`. These names are sampler-loop labels,
not exact elapsed timestamps; RPC/profiler pauses and reaper scheduling can move
the observation relative to the configured 300,000 ms TTL. Raw sample times are
included in the evidence JSON.

Post-TTL footprint was 354,046,080 bytes before and 347,457,000 bytes after. The
remaining footprint is not attributed to semantic bases/deltas. Two initial
baseline native samples (`heap` and `malloc_history`) exited 255 with “Process
exists but has not started -- it is launched-suspended”; they are unusable
samples, not zero allocations. All final-run native samples exited zero.

The compact census, coverage checks, binary SHA-256 hashes, source/corpus refs,
profiler exceptions, and eviction log lines are in
[`semantic-shared-overlays-2026-09-evidence.json`](semantic-shared-overlays-2026-09-evidence.json).
The paired binaries use production implementation content from `eae2261c4`
(before) and `8a5ece723` (after), against the same `eae2261c4` workload tree.

## Search-quality ranking fence

The complete search-quality gate passed as **`engine_unwired`** on an optimized
`stage` binary of the final implementation. Exact recall and the 26-row concept
replay matched their reference metrics, followed by the 49-row paged real-query
replay and its page-size invariance plans. This is unchanged quality, not a claim
that every concept retrieves its answer at rank one. The reference and
`manifest.sha256` were not changed.

```sh
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= \
  CARGO_PROFILE_STAGE_DEBUG=0 CARGO_PROFILE_STAGE_SPLIT_DEBUGINFO=off \
  cargo build --locked -p agent-file-tools --profile stage --bin aft -j 2

scripts/telemetry/cost-gate.sh --search-quality --mode evaluate \
  --descriptor target/overlay-evidence/descriptor.json \
  --base-ref eae2261c4 --head HEAD \
  --binary "$PWD/target/overlay-evidence/aft-ranking" \
  --score-output "$PWD/target/overlay-evidence/optimized-ranking/score.json" \
  --ready-timeout 1800
```

`aft-ranking` is a copy of the built stage executable. The descriptor is:

```json
{"slice_class":"engine_unwired","targeted_mechanism":"none","kind":"harness","fixtures":["crates/aft/tests/integration/tool_call_parity_test.rs"]}
```

In addition to the gate, an explicit byte comparison of `canonical_json(rows)`
checked every field of every ranked row, not just aggregate recall:

- Rows: **49**.
- Canonical row payload: **274,761 bytes**.
- Reference SHA-256:
  `37aaacf461fba56d0d023a5fbc7b8dc79cdc4cd09ac40306d1540b0aeadf8268`.
- Candidate SHA-256:
  `37aaacf461fba56d0d023a5fbc7b8dc79cdc4cd09ac40306d1540b0aeadf8268`.

Earlier unoptimized ranking attempts are not counted as passes: one exhausted
the outer shell deadline and later incomplete replays were stopped when
superseded by the final optimized run. The paged workload executes 24 serial
searches per included row (1,176 searches at the current 49 rows), making debug
replays unsuitable for the short outer deadline on this host. The final
optimized build and complete gate exited zero. Memory measurements remain the
paired debug comparison described above, not a cross-profile comparison.
