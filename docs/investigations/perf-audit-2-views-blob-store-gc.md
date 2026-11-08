# Performance audit 2: views, blob store and GC

## Delivered changes and measurement boundaries

Reviewed against base `54a3d93e230e23e580c40a0164d5cfe2bdc9561d`.
Audit locations below refer to the original report; evidence locations refer to
the implementation after these changes. The evidence report itself is untouched.

The reconciliation and semantic pin-growth findings are fixed, along with two
embedding-waste defects found from the added live evidence. The remaining findings are
triaged below, **not claimed fixed or dynamically measured**. In particular,
this is not a claim that all view-mode performance problems have been removed.

The parent chose strict reconciliation rather than event-only reconciliation:
there is no authoritative watcher coverage/gap state in this driver. Its added
requirement was an in-memory git-index-style stat cache: size, nanosecond mtime,
ctime and inode, with a racy-clean fallback. The implementation also checks the
device identity. Observation time is rounded down to seconds to conservatively
cover coarse filesystem timestamps. Hits retain their original observation
time; delivered file intents always force source reads. Platforms without the
Unix ctime/inode evidence continue reading content. No persistent schema,
layout, manifest encoding, or successful search output changed.
Stat evidence and extracted entries are published as one mutex-protected cache
cut, so concurrent strict loads cannot combine stamps from one walk with
content from another. Reads are bracketed by matching stat/device evidence.

The parent separately approved a partial-progress behavior correction: persist
and admit completed whole-file embedding batches before requesting the next
batch, stopping on the first backend/store error. A failed fill now reports its
real progress rather than discarding successful earlier batches. HTTP packing
may change at file boundaries. Partial resume **inside one large file remains
deferred**; its whole-file payload cannot be persisted until all chunks succeed.

Tests use isolated storage, deterministic hash-derived model vectors and 2,048
Rust source files (one function/chunk per file). There is no LM Studio traffic.
Counters measure actual source reads, content-digest input bytes, attachment
construction, configured membership walks, source stats, pin key-file writes
and bytes, actual touch transaction starts, and embedding texts. Membership
walking and its ignore-file I/O are not included in the source-read counter.
Pin byte accounting is test-only and records successful `write` byte counts.
Lock residence time, total SQLite transactions, parser calls and total fsyncs
were not instrumented; key-file fsyncs remain zero, as they already were before
this change. No elapsed-time performance claim is made.

### Measured before / after

| Operation / finding | Before | After |
| --- | --- | --- |
| One AFT edit in a filled 2,048-file semantic checkout (audit 470) | 4 walks; 8,192 source reads; 282,312 content-digest bytes; 8,192 semantic attachments; 0 explicit source stats | 1 walk; 1 source read; 40 content-digest bytes; 1 semantic attachment; 2,049 source stats (one per member plus the changed file's post-read check) |
| Embedding texts for that edit (audit 470) | **1** | **1** |
| Semantic fill of 2,048 distinct keys (audit 472) | 2,048 pin writes; 136,381,440 pin bytes; 0 key-file fsyncs | 1 pin write; 133,120 pin bytes; 0 key-file fsyncs |
| Fold/materialization of 2,048 distinct keys (audit 471) | 2,049 pin writes; 136,514,560 pin bytes; 2,048 touch transactions; 0 key-file fsyncs | 2 pin writes; 266,240 pin bytes; 1 touch transaction; 0 key-file fsyncs |
| Unreported six-byte, same-size edit with restored mtime and moved ctime | No stat cache at baseline: source is read | 1 walk; 2 stats; 1 read; 6 content-digest bytes; changed content discovered without an intent |
| Matching stamp at the racy-clean observation boundary | No stat cache at baseline: source is read | 1 walk; 2 stats; 1 read; 6 content-digest bytes (boundary injected, no sleeps) |

The byte counter above counts the driver's `DiskState::of_bytes` digest only.
Each measured semantic attachment also hashes those bytes for its content and
key, so the three reconciliation hashing passes correspond to 846,936 source
bytes before and 120 after. Those additional passes are inferred from the
attachment implementation, not separately instrumented. The fill still reads
the changed file to verify its queued content before embedding it.

**The edit-triggered full reload was real, but unchanged-file re-embedding was
not reproduced:** both baseline and optimized runs sent exactly one changed
text. This result does not explain persistent LM Studio traffic by itself.

### Added live evidence: embedding responses versus stored blobs

The reported daemon observation was about 1 MB/s of LM Studio responses at
160–180% CPU but only 272 KB growth in family `e274ab0872bb490b`'s semantic main
database over a minute. Threads were sampled in `aft-semantic-view` /
`embed_texts`; logs included `embedded_keys=3 texts=37 calls=1 installed=3`.

The intended generated path is
`<storage>/blobs/v2/<family>/semantic.sqlite` (`blob_store/v2.rs:426-429`). It is
not a checked-in file or an absent compatibility surface. SQLite WAL growth
must also be included when comparing network output to database file growth.
Vectors are stored as binary f32 values (`semantic_index.rs:9131-9160`), not
the JSON response encoding. One key is an entire file run containing potentially
many chunks: 37 texts for 3 keys is not itself evidence of duplicate work.

Production family derivation uses the repository root-commit identity for a
top-level checkout (`search_index.rs:6151-6170,6296-6312`); linked worktrees share
that key. Configure supplies that family independently of the checkout scope
(`commands/configure.rs:5890-5895`). Semantic keys use bytes, **relative** path
and producer, not checkout root (`views/semantic.rs:106-122`). The fixture
asserts all three owners open the same store path; the existing real-Git
worktree/session integration test also passes. Ordinary stored/resident reuse
and dropped-admission reuse were already correct.

Two defects were reproduced and fixed:

1. **Completed-claim race** (`views/semantic.rs:592-630`): another view can finish
   after a store miss but before `arena.claim`. The newly acquired claimant
   formerly embedded an already resident/stored key. It now rechecks both
   sources while holding its claim. A deterministic hook completes the other
   view at that exact boundary (no timing sleeps).
2. **Later-batch failure loses earlier runs** (`views/semantic.rs:654-750`):
   `embed_view_files` used to process the entire fill atomically, so an error
   in a later request threw away earlier successful file runs. Complete
   file-boundary batches now reach store and fill admission before the next
   request. Unattempted files are counted as deferred. Planned keys are still
   protected in one write; incremental admission does not restore quadratic
   growing-pin writes. Touch transactions now follow completed batches.

| Scenario | Before | After |
| --- | --- | --- |
| Two views, first completes between miss and claim | 2 texts / 2 calls / 1 stored row; outer worker re-embeds 1 key | 1 text / 1 call / 1 stored row; outer embeds 0 keys |
| Three one-chunk files, second backend batch fails | 2 sent texts / 2 calls / **0** keys installed or stored | 2 sent texts / 2 calls / **1** key installed and stored; retry sends only the remaining 2 texts (4 total including failed request) |
| Three 2,048-file checkouts, eight same-content edit rounds, no-op writes, then a dropped admission and retry | Already correct: 2,057 texts / 41 batched calls / 2,057 stored rows / 607,839 payload bytes | Identical counts; second and third checkout edits send **zero** texts/calls; dropped-admission retry and sibling cost zero calls |
| Same three-worktree/eight-edit schedule; backend fails every seventh batch | All-or-nothing mutant fails before request 2: completed texts=64 but stored rows=0. Not reported as a completed baseline run. | 2,056 successful texts = 2,056 new keys = 2,056 stored rows; 321 texts belong to failed requests; 2,377 total sent texts / 46 batched calls. **No stored content revision is requested again on retry**; siblings make zero calls. |

`model_calls` counts batch callbacks, not file keys: 2,048 one-chunk files need
32 successful 64-text calls. Failed requests necessarily add calls/texts, but
the counted periodic-failure fixture proves already stored content does not.
Whole-file keys can legitimately change while a chunk's template text stays
the same (for example a body-only edit); the retry guard is scoped to each
content revision, not globally to text equality across different keys.

These defects can waste work, but the test evidence does **not** establish that
either alone caused the live 200x ratio. Cross-process embedding claims are
not provided by the existing in-process arena; persistent store-write failures,
backend validation/retry behavior, and partial progress within a large file
still need live failure logs or separately scoped instrumentation.

### Output equivalence

`large_checkout_edit_counts_source_work_and_preserves_outputs` compares ranked
semantic rows (including `f32::to_bits()` scores) to an independent cold legacy
index, then compares canonical manifest JSON bytes, every referenced stored blob
payload, and semantic rows before/after an explicit strict load. The
materialization counting test independently checks the same manifest/blob
reload equivalence and fill-versus-fold rows. Existing views and per-checkout
integration suites cover lexical/trigram, callgraph, restart, sibling reuse,
ignore/membership changes, GC and publication races. No search-ranking or
routing file was changed.

The optimized refresh seeds from its already pinned installed generation,
reconciles once, and publishes without loading siblings. Folding uses the
revision-fenced reconciled disk facts. If a delivered edit races the builder,
the loader reconciles again; obsolete installs still fail the revision fence.
Cold first load continues selecting a sibling seed. Newly embedded keys are
protected as a batch before *any* puts, and materialization candidates are all
protected before the batched touch; missing rows remain pending/failed.

## Full finding ledger

`NM` means no dynamic before/after measurement, not zero work. Deferred rows
are unchanged and require a separate counted fixture and mutation proof before
any optimization can be called verified. Static loop counts describe code
structure only, not observed host work.

| Audit location / finding | Verdict and current evidence (`crates/aft/src/` unless noted) | Before / after or reason for deferral |
| --- | --- | --- |
| 470: edit calls full semantic load twice, walking twice each | **Fixed.** `views/semantic_runtime.rs:205-232`, `views/first_load.rs:238-294,813-847,1081-1174`. Installed-owner refresh, revision-fenced fold and stat-first strict reconciliation. | Measured table above. No unchanged-file embeddings before or after. |
| 471 (also summary row 16): semantic materialize per-entry pin/touch | **Fixed.** `views/semantic.rs:1299-1361`. Candidates are pinned and touched together; missing rows cannot be admitted. | 2,049 → 2 pin writes; 136,514,560 → 266,240 bytes; 2,048 → 1 touch transactions. |
| 472: semantic fill per-key pin and put | **Fixed for pin growth**, put transactions **deferred** under 473. `views/semantic.rs:654-662`. Protect all planned embedding keys before puts. Earlier pin syscall/fsync work did not fix these callers. | 2,048 → 1 pin writes; 136,381,440 → 133,120 bytes; key fsyncs 0 → 0. |
| 473: transaction per blob, v1 global put barrier; migration caller | **Deferred, confirmed.** `blob_store/v2.rs:459-512`, `blob_store/mod.rs:535-568`, `views/assembly.rs:434-452`, `views/trigram.rs:620-622`, `migration/per_checkout.rs:1751-1753`. | NM / NM. A batch-put design must preserve per-key quarantine/conflict outcomes, semantic model conflicts, partial errors and GC epoch stamping. Pin/touch batching does not remove put transactions. |
| 474: publication barrier covers checkpoints, sync and closure | **Deferred, confirmed with stale checkpoint wording.** `views/mod.rs:888-938` holds the blob barrier throughout these steps; `:1174` now uses PASSIVE rather than FULL checkpoint. | NM / NM. The barrier proves durability-before-reference; narrowing it requires a concurrent-put/publication durability fixture, not removing a lock on static evidence. |
| 475: autocommit path-status deletes per HEAD file | **Deferred, confirmed.** `views/assembly.rs:490-496`, `path_status/mod.rs:186-191`, `refresh/mod.rs:263-277`. | NM / NM; statically one clear per eligible path, including no-op deletes. Needs a transaction API for mixed status outcomes and crash/annotation-age parity. |
| 476: enforcement measures twice, unconditional family sweep and oldest-row listing before budget check | **Deferred, confirmed.** `views/eviction.rs:661,714-726`; `gc/family.rs:340-358`. | NM / NM. Under-budget passes also reclaim interrupted evictions/dead sessions and advance sweep state; a safe early exit needs fixtures for those obligations. |
| 477: v1 GC unindexed oldest order and autocommit delete per blob | **Deferred, confirmed.** `gc/mod.rs:199-237`. | NM / NM. Index addition is a store schema change (requires parent decision); delete batching needs crash and age-floor tests. |
| 478: manifest fingerprint deep clone and JSON per publish/load | **Deferred, confirmed.** `views/manifest_v2.rs:297-301,321-337`; `views/materialization.rs:170-174`. | NM / NM. Borrowed serialization/cached canonical bytes must retain exact flattening/order and invalidate on every mutable manifest access; no serializer or format changes included. |
| 479: seed reads every sibling's full manifest although one wins | **Fixed on edited refresh/fold**, **deferred on cold load**. `views/first_load.rs:128-170` still scans cold candidates; `:238-247` avoids seed selection on refresh. | Edited refresh/fold does not call `seed` anymore. Cold sibling-load count NM / NM; exact-HEAD preference depends on manifest headers and corrupt-candidate fallback. |
| 480: reconstruct LiveDelta per path; directory intent loops every member | **Deferred, confirmed.** `views/first_load.rs:1034-1052,2095-2123`. | NM / NM. Batch intent recording needs simultaneous revision, pending intent and watcher-state parity; no intent API change included. |
| 481: semantic overlay does membership/path work and ignore reads before cache hit | **Deferred, confirmed.** `views/semantic.rs:885-917,995-1067,1216-1246`. | NM / NM. Early cache hits must still discover unreported ignore edits and report newly included paths as gaps. Requires an ignore-specific cache validation fixture. |
| 482: 10ms query-wait polls with O(N) gap scan under mutex | **Deferred, confirmed.** `views/query_wait.rs:23-39`; `views/first_load.rs:1323-1395` (`installed_state`). | NM / NM. Event-based waits need wake-on-fill/install/edit with one total deadline, including lost-wakeup and last-probe tests. |
| 483: trigram watcher event strictly walks whole root under delta owner | **Deferred, confirmed.** `views/live_delta.rs:132-139` calls reconcile. | NM / NM. Ignore membership is recalculated intentionally; event-local membership cannot assume watcher coverage absent authoritative gap state. Semantic stat reuse does not alter this path. |
| 484: trigram query rebuilds membership and scans segment files; pending intents force strict walk | **Deferred, confirmed.** `views/trigram.rs:132-221,226-276,413-420`. | NM / NM. Cache/inverted overlay improvements must retain direct-scan fallback, named read gaps and intent-before-pruning semantics. |
| 485: periodic child pointer opens and HEAD/ref/packed-refs reads | **Deferred, confirmed.** `views/parent/worker.rs:477-514,535-560`. | NM / NM. HEAD freshness and packed-ref replacement need an identity/racy-stat test equivalent to the checkout cache, including linked worktrees; no parent worker change included. |
| 486: one blob connection mutex; digest verification per get; usage full scan | **Deferred, confirmed.** `blob_store/v2.rs:389,1030-1057,1073-1090`. | NM / NM. Digest verification is an integrity boundary, and connection splitting has epoch/transaction lifetime consequences. No verification weakening. |
| 487: trigram re-reads payloads just stored to build segment | **Deferred, confirmed for initial segment construction.** `views/trigram.rs:620-652`, `views/segment_store.rs:357-375`. Existing shared segment bypasses this rebuild at `trigram.rs:635-650`. | NM / NM. Retaining payloads avoids reads but increases peak memory and must handle conflicting immutable payloads; needs initial-versus-shared-segment fixture. |
| 488: entire segment is read/decoded for identity comparison; no mmap | **Deferred, confirmed.** `views/segment_store.rs:398,447-517`. | NM / NM. Mmap changes resource and Windows file-replacement lifetimes; byte/header-only verification cannot silently weaken corruption detection. |
| 489: current-generation connection and directory relist per stale generation under transaction | **Deferred, confirmed.** `views/generation.rs:102-108,168-182`. | NM / NM. The ownership transaction and final recheck protect publisher references; batching requires live-pin/publisher race evidence. |
| 490: assembly duplicates manifest and parses base again | **Deferred, confirmed.** `views/assembly.rs:180,243,419,555`. | NM / NM. The report owns a fallback manifest on partial preparation failure; lifetime/ownership cleanup needs a large cold/diff fixture. |
| 491: view open initializes pointer; current-generation opens connection; manifests uncached | **Deferred, confirmed.** `views/mod.rs:737-754,796-804,982-994`; assembly base load at `views/assembly.rs:180`. | NM / NM. Existing readers already use a read-only pointer path; cache must respect cross-process CAS and foreign pin-then-verify reads. |
| 492: `/proc/stat` btime read per owner liveness check | **Deferred, statically confirmed; Linux runtime not reproduced on this macOS host.** `root_cache.rs:1114-1121`. | NM / NM. Linux-only boot-time/liveness cache measurement is required; do not infer it from macOS tests. |
| 493: members loaded and linearly searched per lookup | **Deferred, confirmed.** `gc/family.rs:731-734`, `views/registry.rs:511-515`, `views/eviction.rs:629-632` (audit's eviction line moved). | NM / NM. Scoped SQL reads are plausible, but need multi-member protection/deregistration parity and counted row decoding. |
| 494: filesystem listings while holding registry IMMEDIATE barrier | **Deferred, confirmed.** `views/eviction.rs:384-411`. | NM / NM. Registration must not land between protection census and removal; moving listings out needs a revalidation protocol. |
| 495: child SearchIndex deep clone each batch | **Deferred, confirmed.** `views/parent/worker.rs:305-309,378-381`. | NM / NM. Reader snapshots need immutability; structural sharing/mutable generations require query concurrency and memory accounting tests. |
| 496: JSON surface parse for every file on incremental materialization | **Deferred, confirmed.** `views/materialization.rs:1254-1269,1291`. | NM / NM. Unselected surfaces participate in dispatch/resolution correctness; only a selected-row strategy backed by cold/diff parity can remove them. |
| 497: RelPath clone for FillMap lookup | **Deferred, confirmed.** `views/readiness.rs:101-108`. | NM / NM. Borrowed tuple-key lookup or reorganized map should be measured with path allocations and admission/trim parity, not a wall-time-only microbenchmark. |
| 498: sync each file and directory copied in storage migration | **Deferred, confirmed.** `migrate_storage.rs:731,740`. | NM / NM. This is one-time crash-safe migration; sync reduction needs forced-kill restart proofs, not a steady-state refresh optimization. |
| Elsewhere 183: two key Strings and collision payload comparison in dedup | **Deferred, confirmed.** `refresh/mod.rs:61-71`. | NM / NM. Conflict detection remains mandatory; key allocation reduction needs a counted fanout fixture and bytewise-order proof. |
| Elsewhere 251: per-path alias query prepare and seed transaction in assembly | **Deferred, confirmed.** `alias/mod.rs:340-347,392-414`; assembly caller `views/assembly.rs:449-452`. | NM / NM. Batch alias seeding must preserve independently proven Git OIDs and immutable-conflict refusal; no alias schema/API change included. |
| Elsewhere 255: process-global SQLite filesystem guard on opens/checkpoints | **Partly already fixed, remainder deferred.** Open serialization persists at `db/lifecycle.rs:484,493,506`. `IdentityConnection::open` releases its guard on return (`db/file_identity.rs:268-271`); `views/generation.rs:264-284` checkpoints under a per-path mutex, not that global guard. | Dynamic lock hold times NM / NM. No lifetime-global guard in the current checkpoint path; do not claim all SQLite opening contention is gone. |

## Red tests and mutation evidence

Before implementation, the large edit test failed with `walks: 4, reads: 8192,
content_hash_bytes: 282312, attachments: 8192`; expected one walk. Before pin
batching, both large semantic tests failed on write counts (2,048 vs 1 and
2,049 vs 2). No existing test's contract was rewritten.

All mutations used the staged live implementation as the index baseline,
confirmed an empty working diff, carried a temporary mutation marker, showed a non-empty
diff, ran the named test, and were restored with `git checkout -- <paths>` plus
`touch`, after which `git diff --stat` was empty. Each named mutant run selected
one test and failed only that test; there were no other selected tests.

| Control | Exact failing test (prefix omitted: `views::`) | Captured failure |
| --- | --- | --- |
| Restore refresh/fold loads, post-publication walks and neutralize large fixture stat reuse | `semantic_runtime::tests::large_checkout_edit_counts_source_work_and_preserves_outputs` | `walks: 4, stats: 16384, reads: 8192, content_hash_bytes: 282312, attachments: 8192`; `left: 4 right: 1`; 1 failed |
| Ignore ctime in reusable stamp | `first_load::stat_tests::stat_reconcile_detects_unreported_same_size_backdated_edit` | `reads: 0, content_hash_bytes: 0`; content `18c52a7ed981fbff` vs expected `69da6dbe23d41a9a`; 1 failed |
| Remove racy-clean rejection | `first_load::stat_tests::stat_reconcile_reads_racily_clean_file_even_when_stamp_matches` | `reads: 0, content_hash_bytes: 0`; `left: 0 right: 1`; 1 failed |
| Restore singleton pin protection for embedding batch | `semantic_runtime::tests::large_semantic_fill_batches_pin_writes` | `writes=2048 bytes=136381440 key syncs=0`; `left: (2048, 0) right: (1, 0)`; 1 failed |
| Restore singleton protection/touches for materialization | `semantic_runtime::tests::large_semantic_materialization_batches_pins_and_touches` | `writes=2049 bytes=136514560 key syncs=0 touch transactions=2048`; `left: (2049, 0) right: (2, 0)`; 1 failed |
| Remove post-claim resident/store recheck | `semantic_runtime::tests::family_fill_rechecks_completed_work_after_acquiring_claim` | `texts=2 calls=2 outer_embedded=1 stored_rows=1`; `left: 2 right: 1`; 1 failed |
| Neutralize resident/store reuse on the repeated-edit fixture | `semantic_runtime::tests::repeated_worktree_edits_embed_only_new_family_keys_even_after_dropped_admission` | second checkout `embedded_keys`: `left: 2048 right: 0`; 1 failed |
| Restore whole-fill all-or-nothing embedding | `semantic_runtime::tests::completed_embedding_batches_survive_a_later_batch_failure` | `texts=2 calls=2 embedded_keys=0 installed=0 rows=0`; `left: (0, 0) right: (1, 1)`; 1 failed |
| Restore whole-fill all-or-nothing embedding under periodic failures | `semantic_runtime::tests::repeated_worktree_edits_with_periodic_backend_failures_never_repeat_stored_texts` | `completed batches were not persisted before the next request`; `left: 0 right: 64`; 1 failed |

The ctime and racy controls were applied in the same mutation batch but executed
in separate single-test invocations. The ctime fixture has an old, non-racy
mtime; the racy fixture has an unchanged ctime. Thus each exercises its intended
guard, not the other guard's failure. The refresh/stat mutation batch was 13
insertions / 13 deletions across `first_load.rs` and `semantic_runtime.rs`;
the protection mutation batch was 8 insertions / 9 deletions in `semantic.rs`.
The claim/reuse mutation batch was 6 insertions / 4 deletions in `semantic.rs`;
the whole-fill mutation was 2 insertions / 1 deletion in `semantic.rs`. Both
were restored to an empty working diff before final gates.

## Gates

Tool versions: `cargo 1.99.0 (5f94df478 2026-08-27)`,
`rustc 1.99.0 (b940084d7 2026-09-28)`,
`rustfmt 1.10.0-stable (b940084d7e 2026-09-28)`.

- `cargo fmt --all -- --check`: exit 0 (silent-on-success gate).
- `cargo test -p agent-file-tools --lib views:: -- --nocapture`: 172 passed,
  6 ignored, 0 failed (measurement/child-process-only ignored tests).
- `cargo test -p agent-file-tools --lib pins::`: 4 passed, 0 failed.
- `cargo test -p agent-file-tools --lib blob_store::v2::`: 5 passed, 0 failed.
- `cargo test -p agent-file-tools --lib large_semantic_ -- --nocapture`:
  2 passed, 0 failed after final counter-only edits.
- `cargo test -p agent-file-tools --lib repeated_worktree_edits_with_periodic_backend_failures_never_repeat_stored_texts -- --nocapture`:
  1 passed, 0 failed; covered again by the final views suite.
- `cargo test -p agent-file-tools --bin aft`: 121 passed, 0 failed. The initial
  `view_publication` filter matched 0 tests and is **not** counted as a pass;
  it was replaced with the full binary suite.
- `cargo test --features test-hooks -p agent-file-tools --test integration per_checkout`: 81 passed,
  5 ignored child-process entrypoints, 0 failed.
- Windows cross-check skipped at the parent's instruction; the train owns it.
- Scoped diagnostics inspections returned PARTIAL while rust-analyzer was
  indexing/running its cargo check; successful lib/binary/integration
  compilation provides typechecking. No authoritative clean LSP claim is made.
  macOS lib/integration linking warns that `__eh_frame` exceeds compact-unwind
  encoding capacity; no test or compile gate failed because of that warning.

No manifests/lockfiles, package installation, generated schemas, ranking fences,
or configuration keys were changed.
