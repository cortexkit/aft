# Semantic-search work audit

Base: `54a3d93e230e23e580c40a0164d5cfe2bdc9561d`. The static audit's line
numbers were checked against this tree, not assumed to identify current code.
No file under `crates/aft/src/views/` was changed. Both production files are
inside `RANKING_FENCE_PREFIXES`; the descriptor declares `engine_unwired` and
requires byte-identical behavior, not merely non-regressing recall.

## Landed reductions

These are deterministic counts, not timing claims. Fixtures contain 8,000
chunks or exact candidates; the semantic fixtures have 200 files (40 chunks
per file). Allocation counts are from the existing thread-local allocator.
Decoder dispatch counts use an actual `Read` implementation under the production
decoder, not a prediction based on vector length.

| Finding | Current confirmation | Verdict | Before → after measured work |
|---|---|---|---|
| Exact memo deep copies | `commands/semantic_search/memo.rs:303`, publication in `get_or_verify` | fixed | 10-row hit: **16,013 → 11 allocations** in the initial code. Full 8,000-row hit: **24,004 → 8,001** under the equivalent Arc-wrapped deep-copy control (one allocation is the control's Arc wrapper; its 10-row count is 16,014). Immutable payload sharing removes the descriptor/map/candidate copies, not freshness checks. |
| Private semantic include filter per chunk | `semantic_index.rs:6341`, `search_filtered` private branch | fixed | **8,000 → 200 predicate calls**, **16,153 → 560 allocations** with an allocating path predicate. Both sides score **7,960** eligible vectors and sort **50** candidates. Shared-base filtering was already once per file. |
| Four-byte vector reader dispatch | `semantic_index.rs:8206`, `from_reader_after_version` | fixed | Dimension 384: **3,072,000 → 8,000 vector read requests**; complete decoder **3,177,204 → 113,204 reader calls**. One reused byte buffer is bounded to 16 KiB by the existing dimension limit. All float and norm bits and serialized bytes match. Short reads and vector-EOF errors remain supported. |

Tests: `exact_memo_hit_clones_only_the_served_page`,
`private_semantic_filter_runs_once_per_file_and_keeps_result_bytes`,
`semantic_decode_reads_one_vector_at_a_time_and_preserves_bits`, and
`semantic_decode_preserves_short_reads_and_vector_eof_errors`.

The first three counting tests failed before their fixes. Each also failed under
an old-code mutation after implementation; the short-read/EOF control remained
green. For each named counting invocation there was exactly one failure (using
the fully qualified memo name avoids the compatibility module's second copy of
that test). The indexed working state was staged before mutation; its unstaged
diff was empty, became `2 files changed, 17 insertions(+), 19 deletions(-)`, and
was empty again after restoring and touching the two paths.

## Remaining claims, including duplicates outside the semantic section

`N` means chunks, `F` files, `S` symbols, `B` embedding batches. Counts marked
**measured** were executed here; complexity counts marked **static** describe
confirmed loops, not benchmark measurements. **Unmeasured** means the claim was
triaged but no before/after performance claim is made. Deferred items are not
silently described as fixed. References below are to the base tree (unchanged
symbols keep those positions, except small shifts after edited sections).

| Finding | Current evidence | Verdict / reason | Before → after work |
|---|---|---|---|
| Exact lane requests the whole memo page; re-reads/hashes exact files | `mod.rs:4233,4268`; `memo.rs:267-278` | deferred: a smaller upstream set can change global exact ordering and later-page membership; removing digest checks changes content-binding guarantees | **measured**, unchanged: 10 candidates = 10 reads/hashes; 8,000 candidates across 200 files = 8,000 reads/hashes. |
| Brute-force scalar scoring, per-entry vectors and N-sized intermediate arrays | `semantic_index.rs:6350-6366,6395`; `dot_product` | deferred: exhaustive scoring remains necessary for exact results. SIMD/reassociation is not bit-identical; contiguous storage and bounded streaming selection need separate ordering/cancellation proofs | **measured**, unchanged: 8,000 scores / normal query; 8,000 scores and 32,422 allocations / full borrowed recall ranking. No dot-product arithmetic changed. |
| Full sort before semantic top-K | `search_filtered:6409-6417` | already fixed: selection precedes sorting | **measured**, unchanged: 8,000 scores, only 50 sorted. |
| Include filter on borrowed chunks | `search_filtered:6369-6388` | already fixed for the base; private counterpart fixed above | **measured**, unchanged for borrowed files: 200 calls / 8,000 chunks. |
| PathLookup walks gitignore corpus and sorts by mtime | `mod.rs:4356-4369` | deferred: replacing a current-disk walk with indexed paths can lose unindexed/new files; ignore freshness needs its own cache invalidation contract | **static**, unchanged: one corpus walk per selected PathLookup lane, per-file token normalization. Dynamic walk/stat counts unmeasured. |
| Up to eight index scans for split file cosines | `mod.rs:1261-1281` | deferred: batching must preserve the per-file top-1 selection, ties and cancellation prefixes, not replace it with a different float max | **static**, unchanged: up to 8N eligibility visits. Dynamic split-request counts unmeasured. |
| Model mutex across cache lookup / remote embed | `mod.rs:6649-6677` | deferred: releasing it requires changing model/cache ownership and same-query concurrency; retain backend lifecycle behavior | unmeasured; no lock/concurrency change. |
| Lexical candidates fully read and normalized | `mod.rs:4183-4204,3204-3239` | deferred: whole-file phrase evidence is still required. Earlier repeated three-line window work was already removed, but file-level normalization remains | prior frozen-corpus evidence in `HOT_PATH_MEASURED.md`; dynamic public-query copy counts unmeasured here. |
| Nearest-name offset lookup rescans file prefix per symbol | `nearest_names.rs:172,315-326` | deferred: a line-offset table is feasible, but needs independent UTF-8/CRLF/EOF and allocation-count controls | **static**, unchanged: O(S × file bytes), at most 50 files. Dynamic symbol-byte counts unmeasured. |
| Anchored canonicalization, split regex compilation, sequential reads | `anchored_lane.rs:164,558,666-681` | deferred for canonicalization/split regex/read sequencing. Per-file literal-run compilation was already fixed by `CompiledRuns` | prior real-corpus compilation/read counts in `HOT_PATH_MEASURED.md`; new split-regex/canonicalize counts unmeasured. |
| Multiple watcher refresh passes / reuse-map copies (duplicate of audit's cache section) | `semantic_index.rs:5019,6012,6055,6091,6174` | already fixed for private clone-before-removal: payloads move in one retain pass. Shared-base projection, counts and other passes remain deferred; an immutable base cannot be moved out of other roots | **measured**, unchanged: 1-file and 100-file private refresh each visit 8,000 entries and clone 0 reuse payloads; source reads 1 / 100. Full-cap 100-file deferral reads only 1 file. |
| `indexed_file_count` constructs a path set (outside-section duplicate) | `semantic_index.rs:4855-4857` | deferred: counting without a union must still handle base/local overlap and tombstones | **measured**, unchanged: 200 files, 428 allocations / borrowed count. |
| `fork_for_refresh` clones growing delta | `semantic_index.rs:4464` | deferred: sharing mutable delta payloads needs copy-on-write ownership across worker/apply/persist paths | **measured**, unchanged: 40-chunk overlay fork, 208 allocations; immutable 8,000-chunk seed stays shared. |
| Decoder copies strings twice / allocates paths per chunk | `semantic_index.rs:8198-8209,8382-8393` | deferred (vector part fixed above): preserve lossy UTF-8 semantics and path admission; a string-allocation control remains needed | unmeasured string/path counts; cache format and admission unchanged. |
| Borrowed semantic.bin read/hash three times (duplicate of cache section) | `borrowed_artifact_identity:3964-3993`; `read_from_disk_borrow_tolerant_inner:7743,7779,7793` | deferred: the second identity sample protects against artifact replacement during load; removing it requires descriptor-based identity verification, not mtime trust | **static**, unchanged: cold miss has two whole-artifact hashes plus decode; registry hit has one identity hash. Bytes/read counts unmeasured. |
| Segment-log retain per segment | `apply_segment_log:7482` | deferred: one-pass replay must preserve retain-then-append order across all replacement segments and torn tails | **static**, unchanged: up to 64 retain passes; realistic multi-segment visits unmeasured. |
| `delta_for_paths` scans/project-allocates all entries (duplicate of cache section) | `semantic_index.rs:6817-6829` | deferred: file-local addressing must retain entry order, local/base precedence and persisted path semantics | **measured**, unchanged: one requested file / 8,000 borrowed chunks gives 40 delta entries and 16,215 allocations. |
| Strict borrowed-path verification / PathBuf per chunk (outside-section finding) | `semantic_index.rs:4667-4689` | deferred: hash verification is required across checkouts; reducing projection work is separable but needs file-identity coverage | **measured**, unchanged: 8,000 chunks / 200 files; 200 hashes, 17,038 allocations. |
| Warm semantic hash-all before another verification (outside-section finding) | `configure.rs:5182-5184,5285`; `semantic_index.rs:6454,5692-5699` | deferred: `is_file_stale` is strict; resolver already has a verified-generation early return (`configure.rs:5150-5156`), so the claim is not universal to every warm load. Changing cold/borrowed strictness is unsafe | **static**, unchanged: up to F preflight strict checks followed by strategy-selected F checks; root-scoped end-to-end counts unmeasured. |
| New verification ThreadPool per semantic refresh (outside-section finding) | `cache_freshness.rs:304-337`; `semantic_index.rs:5693-5698` | deferred: shared infrastructure outside these production edits; retain bounded hashing rather than introducing the global Rayon pool | **static**, unchanged: one dedicated pool per bounded verification of more than two files. Dynamic pool-start counts unmeasured. |
| Cold build error discards earlier successful batches (outside-section finding) | `semantic_index.rs:5257-5276`; fallible `execute_build_embedding_batch` | deferred: adaptive retry already handles many transient failures within a batch, but a terminal error still discards the prefix. Durable resumability needs fingerprint/row-text/cancellation ownership decisions | **static**, unchanged: successful prefix B can be re-embedded after terminal failure; injected multi-batch retry-call counts unmeasured. |
| ONNX length bucketing / tensor and mask copies | `local_embed.rs:305-327,375,399` | deferred: reordering batch composition can change model output bits (the harness documents live-model nondeterminism); no SIMD/model arithmetic change is landed | unmeasured with real ONNX; deterministic vector-pack replay is not evidence for model-inference parity. |
| ONNX session/tokenizer/query worker per model/root, half-core intra-op | `semantic_index.rs:1148-1159,2130-2135`; `local_embed.rs:128-196` | deferred: cgroup quotas are already considered, but root model sharing changes lifetime and contention; requires real runtime work counts | unmeasured; no model/thread ownership change. |
| Serial HTTP batches, duplicate resident chunk text | `semantic_index.rs:5257-5276,5157-5164` | deferred: parallel backend submission alters AIMD/cancellation/backend load; ownership-only text reduction needs separate adaptive-shrink tests | **static**, unchanged: B sequential embedding batches; allocation/embedding-call counts unmeasured for a realistic cold corpus. |
| `embed_text` retained just for reuse | `EmbeddingEntry` / `build_chunk_reuse_map:4995-5010` | deferred: full string confirms hash collision safety and is already part of persisted cache format; removing it would change format/reuse semantics | **static**, unchanged: one retained string per chunk. Dynamic retained-byte count unmeasured. |
| Reranker / display reads same file repeatedly; trigram fallback | `rerank/mod.rs:569,581`; `mod.rs:3891-3955` | deferred: per-request source sharing is feasible, but byte-offset normalization and content-binding must agree across producers | **static**, unchanged: candidate text can read a file twice before later display; dynamic read counts unmeasured. |
| Exact verification normalizes each symbol before whole-file miss rejection | `exact_lane.rs:604-641` | deferred: definition evidence can exist without phrase presence, so a blanket phrase early return is incorrect; needs separately scoped evidence tests | **static**, unchanged: S symbol normalizations; dynamic normalized-byte counts unmeasured. |
| Lexical top-3200 full sort / detect-language comparisons | `lexical_lane.rs:133-143`; `comparator.rs:228-245` | deferred: dedup precedes depth truncation, so naive partial selection can lose unique candidates; stable comparator caching needs its own control | prior frozen-corpus score/sort evidence in `HOT_PATH_MEASURED.md`; new comparator classification counts unmeasured. |
| Re-observe, clone, sort and rebuild tier blocks | `blocks.rs:428,446,481,586-600` | deferred: tier-scoped attribution is intentionally frozen at each depth; sharing observations must not expose future lanes | **static**, unchanged: re-observation per escalated tier; dynamic candidate-copy counts unmeasured. |
| Synapse copies text / hex formatting / JSON per float | `synapse_embed.rs:329,977,933,1014,875` | deferred: not exercised by the OpenAI-compatible vector replay; needs keyed paged-daemon fixtures with realistic dimensions | unmeasured Synapse transport counts; no backend serialization change. |
| Cancellable HTTP query spawns an OS thread | `semantic_index.rs:1910-1941` | deferred: this preserves prompt cancellation around blocking reqwest; pooling requires abandoned-request admission and lifetime controls | **static**, unchanged: one worker per cancellable exchange, zero on cache hits; thread starts unmeasured. |
| Fuzzy O(H × N), normalized copies, joined windows | `fuzzy_match.rs:87,269,420` | already fixed for per-comparison Unicode normalization and unconditional early copies: exact/rstrip/trim return before pass 4. Remaining line scan/window joining deferred to edit-matching ownership | **static**, unchanged: normalization once per line in pass 4; O(H × N) comparisons and eligible reflow joins remain. Dynamic comparison counts unmeasured. |
| Per-query confidence JSON / plan-table map | `confidence.rs:320`; `plan_table.rs:207`; `blocks.rs` engine construction | deferred: constructors still build deterministic values. A shared immutable instance is feasible, but this slice does not include a constructor-count control | unmeasured parse/map-allocation counts; no thresholds/weights changed. |
| Reused-vector and apply-delta clones | `semantic_index.rs:5116,6190` | deferred: duplicate byte-identical chunks can reuse one old vector; worker and live apply payloads currently each require ownership | **static**, unchanged: clones per reused/output chunk; dynamic vector-clone counts unmeasured. |
| Redundant exact passes / canonicalize candidates / definition passes | `mod.rs:4223-4282,4383-4393,4324` | deferred: scopes and exact forms differ intentionally, and absence checks include tests; do not coalesce by query text alone | **static**, unchanged: one pass per distinct scope/form; full routed pass/canonicalize counts unmeasured. |
| External exact double stat / serial reads | `external_exact.rs:345-371` | deferred: index-negative admission depends on stat freshness, then reader admission rechecks size; consolidation needs a freshness-preserving reader contract | **static**, unchanged: up to two pre-read stat checks plus reader admission; dynamic syscalls unmeasured. |
| Degraded grep reads 1000 files before result cap | `mod.rs:6202-6248` | deferred: bulk parallel reads remain (not the old sequential loop); changing read windows changes time-budget partial-result behavior | **static**, unchanged: up to the collected 1000 files read before capped scan; timed injected-I/O counts unmeasured. |

## Search-quality replay and reference drift

The provisioned corpus and real vector pack were used with empty replay caches.
The unchanged scalar dot-product and export multiplier are deliberately retained.

The normal checked-in-reference invocation completed 16 exact-recall fixtures,
26 concept fixtures and **93 real-query rows**, but its predicate rejected
`real_query.followup-census:910001`. Investigation found **10 pre-existing
summary-text-only differences**: current main adds symbol/snippet lines that the
reference omits. All ranked paths, per-row metrics and summary metric blocks
already matched the checked-in reference. No reference, manifest, vector pack,
or presentation waiver was changed to make that invocation pass.

To distinguish baseline drift from these fixes, all three production reductions
were reverted in the staged working state, an old-code release binary was built,
and the same full replay was executed independently. It exhibited exactly the
same 10 summary differences. **All 93 complete row documents (including
summary text), all summary metrics and exact/concept family results matched
the optimized replay byte-for-byte.** Restoring the indexed implementation and
touching the files left an empty unstaged diff. A final full cost-gate replay
against that independently generated, manifest-bound old-code reference passed
with `quality_exit:0` and `real_query_behavior:equal`. This is not a rebaseline
of the checked-in reference; the default reference still needs a separate
presentation-only repair by its owner.

| Family | hit@1 before = after | hit@5 before = after | MRR@10 before = after |
|---|---:|---:|---:|
| Real query (93) | 0.3010752688172043 | 0.5268817204301075 | 0.3948924731182796 |
| Concept (26) | 0.5384615384615384 | 0.7692307692307693 | 0.6394230769230769 |
| Exact (16) | 1.0 | 1.0 | 1.0 |

The compact hash receipt is `semantic-audit-2-parity.json`. Full ignored replay
artifacts, logs and both binaries are under `.bench/semantic-audit-2/` in this
benchmark directory. Canonical row hashes cover actual full row documents from
two different compiled binaries, not a projection of ranked paths alone.

## Gates

- Rust: cargo/rustc 1.99.0; rustfmt 1.10.0-stable.
- `cargo fmt --all -- --check`: exit 0.
- `cargo test -p agent-file-tools --lib -- semantic_index::tests`: 137 passed, 4 ignored.
- `cargo test -p agent-file-tools --lib -- commands::semantic_search::`: 296 passed, 5 ignored.
- `cargo test -p agent-file-tools --lib -- search_index::memo::`: 3 passed.
- Both ignored work-count tests were explicitly run with `--ignored --nocapture --test-threads=1`: 1 passed each.
- `cargo test -p agent-file-tools --test engine`: 204 passed.
- `cargo test -p agent-file-tools --test semantic`: 49 passed, 3 ignored.
- `cargo test -p agent-file-tools --test list_envelope`: 172 passed.
- `cargo build --release -p agent-file-tools`: Finished. The first combined gate hit the 30-minute command cap during release compilation; process/binary freshness inspection showed no own compiler left and a stale binary. The build was resumed separately with a longer cap and finished in 14m35s. No simultaneous own heavy build was run.
- Python 3.9.6: three full cost-gate replays (optimized / old-code / optimized against bound old-code reference), each 93 rows; last gate exit 0. Loopback fixture-server broken pipes occurred on shutdown/cancelled fixture requests without changing score rows.
- Scoped `aft_inspect` was partial because rust-analyzer was still indexing. Authoritative Rust compilation and the scoped suites above passed.
- Local Windows compile deliberately skipped as requested; no TypeScript or package manifests changed.
