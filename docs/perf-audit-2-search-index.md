# Search-index, grep, glob and watcher-filter performance audit

Locations below refer to base commit `52f09206136a11e1cd18ebf8348d58200bae6e55`,
under `crates/aft/src/`. This is a bounded performance slice, not a claim that
every item in the external audit has been repaired. Deferred rows are static
confirmations, **not** measured performance improvements.

## Measured changes

Counters are thread-local, attached to the actual filesystem/control-resolution
call sites, and do not charge concurrent tests' work. Fixtures contain pinned
mtimes and ordinary source files. Cancellation's item-count experiment freezes
only the elapsed-time test hook; a separate test exercises the 50ms probe and
immediate explicit cancellation. Runtime-drain probes attempt a read lock at the
real file-preparation entry, so moving preparation inside the write lock makes
the lock counter fail without timing a competing thread.

| Finding / base location | Verdict | Work before → after | Test |
| --- | --- | --- | --- |
| #1; `executor/mod.rs:678,749-755`, `search_index.rs:3518`, grep checkpoints | Confirmed; fixed | 100,000 liveness probes → 391 for 100,000 checkpoints; per-call token clone → borrowed TLS token; lifecycle mutex is consulted only at probes | `perf_audit_cancellation_probe_budget`; `cancellation_time_probe_and_explicit_signal_are_prompt` |
| #2; `commands/glob.rs:165,290`, `search_index.rs:5966-6009` (also build/refresh :164) | Confirmed; indexed single/multi-root sort fixed | 1,000 sort stats + 1,000 per-match canonicalizations → 0 + 0 on a 1,000-file indexed glob; presence checks remain 100 | `perf_audit_glob_only_touches_returned_page` |
| #26; `watcher_filter.rs:1365-1382,314-327` | Confirmed; fixed | 1,002 paths: global-excludes resolution 2,002 → 1; HEAD metadata captures 1,002 → 1; resolved control-path spellings reused across both passes | `perf_audit_watcher_resolves_once_per_batch` |
| #25; `runtime_drain.rs:2817-2829`, `search_index.rs:1876-1921` (also :168/:196) | Confirmed; watcher write-lock I/O fixed | File preparations while holding the search-index write lock: 16 → 0; 16 real preparations still executed | `perf_audit_watcher_search_io_outside_write_lock` |
| #25; `search_index.rs:2715,2776-2794` (also :167) | Confirmed growth; bounded without disk compaction | One base file edited 256 times: 257 file slots → 2; delta-only IDs reclaimed after removing all of their postings | `perf_audit_delta_slots_stay_bounded` |
| `search_index.rs:3919-3947,7742-7757` (also :172) | Confirmed; trigram ingestion fixed, shared generated classification partially deferred | 256 nonempty text files: 768 opens → 256; bounded full read replaces preview reopen; discarded generated probe omitted for trigram ingestion | `perf_audit_corpus_opens_once`; `corpus_reader_preserves_generated_and_binary_eligibility` |
| `search_index.rs:5688`, build calls :1663/:4209 | Confirmed for build; fixed | 256-file ingestion enumeration: 256 sort stats + 256 canonicalizations → 0 + 0 | `perf_audit_build_walk_skips_mtime_sort` |
| `search_index.rs:7221-7245` | Confirmed; no-filter case fixed | 1,000 no-filter paths: 1,000 normalized/glob-key constructions → 0 | `perf_audit_empty_filters_skip_path_keys` |

The first six counter regressions were run against the original implementation
and all six failed with the pre-change counts above. Every counter test was also
mutation-red individually: each isolated run reported exactly its named test
failed and no other failures. The index was staged before mutation; the combined
mutation diff was four files, 29 insertions and 11 deletions, and restoration
produced an empty unstaged `git diff --stat`. The mutations reinstated per-call
probes, preview/generated reopens, build-time mtime sorting, append-only IDs,
no-filter path keys, disk glob ordering, per-path control resolution, and I/O
inside the watcher write lock respectively.

## Confirmed residuals and corrected claims

| Finding / base location | Verdict / evidence | Before → after | Test / follow-up |
| --- | --- | --- | --- |
| #2 fallback double sort; `glob.rs:357-386`, `grep_executor.rs:758`, glob :165 | Confirmed, deferred. Walk order and final cross-root order are both computed. Parent-folder fan-out also lacks indexed-mtime transport in this slice. | Not measured; unchanged fallback path | Existing grep/glob suites; optimize fallback ordering separately |
| #25 runtime thresholds; `search_index.rs:2776-2794` | Confirmed that thresholds only set flags. Reusing delta IDs addresses unbounded edit history, not compaction of a growing live delta or Arc copy-on-write cloning. | Live-delta compaction not measured; unchanged thresholds | Existing disk-compaction and snapshot-coherence tests |
| Shared corpus reader `search_index.rs:3946` | Partial. Callers that actually consume `generated` retain the existing generated-policy probe; changing inspect's private classifier/export is outside this slice. | Three opens → two for shared text-corpus reads; only trigram ingestion has a one-open counter proof | Eligibility parity test; not claiming a shared-reader one-open proof |
| Build/reconcile “always sort”; `search_index.rs:7347` | Overstated: `verify_file_mtimes` already passes `sort_by_mtime=false`. Build and borrowed reconciliation used the sorted helper and now do not. | Already 0 sort work in warm verification | `warm_verify_matches_serial_reconciliation_for_both_strategies` |
| Filter allocations with globs, `search_index.rs:7432-7435` | Confirmed normalization remains in scope checks and nonempty-filter matching. | Not measured; unchanged | Windows scope/grep/glob tests |
| `grep_executor.rs:389,411` | Confirmed: `has_file_in_scope_with_filters` is a short-circuiting linear scan, not necessarily a full traversal on every query. Deferred. | Not measured; unchanged | Existing grep scope-disclosure tests |
| `search_index.rs:3429-3440`, grep :260-277 | Confirmed posting-list materialization and per-root candidate computation. AND terms already sort by posting count and stop on empty intersection. Deferred. | Not measured; unchanged | Candidate and multi-root query-byte parity tests |
| `search_index.rs:7047-7156` (also build/refresh :169) | Confirmed ignore-rule discovery walk and git-common-dir spawn; load/build/refresh each invoke it. The exact “2–3 walks per load” depends on load outcome. Deferred. | Not measured; unchanged | Ignore-rule discovery and refresh suites |
| Ignore change rebuild, `search_index.rs:2670-2677`, runtime :2236 (also :170) | Confirmed full-rebuild path on changed fingerprint/rule files; preserving correct corpus membership is essential. Deferred. | Not measured; unchanged | Existing ignore-refresh tests |
| Mtime-only rewrite / serial stale ingest, `search_index.rs:7387-7391,1967` (also :171) | Confirmed `ContentFresh` sets changed and stale updates are serial; caller may persist the changed index. Deferred. | Not measured; unchanged | Warm reconciliation tests |
| `search_index.rs:2252,2265,5126-5133,5867` | Confirmed per-record `stream_position` bounds checks and symlink metadata. Parent canonicalization is already memoized, so counting every canonicalization independently would overstate work. Deferred. | Not measured; unchanged | Corrupt-cache and path-containment tests |
| `grep_executor.rs:959-967,785-818` | Confirmed: stopped scan callback returns without terminating the walker. Deferred; a future iterator-level stop must disclose incomplete coverage. | Not measured; unchanged | Fallback bound/disclosure tests |
| `search_index.rs:5353-5364` (also :173) | Confirmed BTreeMap entry operation per emitted trigram; not an allocation per byte (only new keys allocate). Deferred. | Not measured; unchanged | Trigram-mask parity tests |
| Exact pass `search_index.rs:1017,1068-1100` | Confirmed serial direct reads and text/path processing. “Three opens each” is inaccurate for direct `read_searchable_text`, which uses one `fs::read`; the triple-open corpus reader was separate. Deferred. | Not measured; unchanged | Existing exact-lane tests |
| `search_b2/embed_counter.rs:70-74,121-139` | Out of assigned scope; no verdict or fix in this slice. | Not measured | Owning search-ranking slice |
| CRC `search_index.rs:4407,5135-5154` (also :179) | Confirmed postings CRC reread. It is an integrity check; do not drop it without a streaming-equivalence proof. Deferred. | Not measured; unchanged | Cache checksum tests |
| `grep_executor.rs:687-702` | Confirmed two walks for source-first fallback. Deferred. | Not measured; unchanged | Source-first/exact fallback tests |
| Compaction hashing/clones `search_index.rs:4141-4150,4310-4323` (also :179) | Confirmed remap hash lookups and owned file-table copies. Streaming cold build's ID map is identity; persistence needs remapping after deletions. Deferred. | Not measured; unchanged | Disk round-trip/compaction tests |
| Sorted postings removal `search_index.rs:1851` (also :180) | Confirmed linear retain despite sorted IDs. Deferred. | Not measured; unchanged | Delta insertion/removal parity tests |
| Git log fields `search_index.rs:1547,2096,4307,4335` (also :179) | Confirmed repeated HEAD probes on some build/load paths. Deferred. | Not measured; unchanged | Git refresh tests |
| Multi-root canonicalization `grep_executor.rs:582`, `glob.rs:346-355` | Confirmed per-match merge identity canonicalization. Deferred; alias deduplication must survive optimization. | Not measured; unchanged | Multi-root merge tests |
| Lowercasing/line starts `search_index.rs:3697,3786,7759-7767`, grep :1025/:1117 | Confirmed case-fold buffers and eager line-start construction in relevant arms. Deferred. | Not measured; unchanged | Unicode and long-line tests |
| Blocking glob read `glob.rs:276-279` | Confirmed; unlike grep, snapshot acquisition uses a blocking read lock. Deferred. | Not measured; unchanged | Glob suites |
| Sort/dedup and superseded checks `search_index.rs:3521,3532,3557-3560` | Confirmed duplicate active checks and final sort/dedup; base and delta union order must still be considered. Deferred. | Not measured; unchanged | Direct base-decode parity tests |
| `gitignore_state.rs:410-411,426-433` | Confirmed two `is_dir` calls for walker entries already known to be files. Deferred (Low). | Not measured; unchanged | Gitignore membership tests |
| Device boundary `grep_executor.rs:629-641` | Confirmed boundary query before cheap directory-name exclusions. Deferred (Low). | Not measured; unchanged | Foreign-mount walk tests |
| Degraded IDs `search_index.rs:3487-3492` | Confirmed rebuilding and sorting active IDs. Deferred (Low). | Not measured; unchanged | Degraded grep tests |
| `readonly_artifacts.rs:241-250` | Confirmed 10ms polling with a 300s deadline. Deferred (Low); replacing it must retain cancellation/deadline checks. | Not measured; unchanged | Borrowed artifact tests |
| Path allocation sort `search_index.rs:6061-6069` | Confirmed multiple owned path keys in sort decoration. Deferred (Low); the expensive filesystem work is removed for indexed glob, not all key allocations. | Not measured; unchanged | Mtime sort parity tests |

## Compatibility and verification

`search_index.rs` is inside `RANKING_FENCE_PREFIXES`. The accompanying descriptor
declares `engine_unwired`: it requests byte-identical quality evaluation, not a
ranking waiver. No ranking formulas, routing files, semantic-index files, public
tool arguments, cache format, or configuration keys were changed.

The glob counter fixture compares the entire returned path page as serialized
JSON bytes and its rendered text to independently constructed newest-first
expectations. The delta fixture compares grep matches against a fresh serial
index while retaining an earlier snapshot. Existing tool-call parity, ranking,
scope, ignore-control, Unicode, cache, and list-envelope suites supply broader
behavior coverage. Files missing from disk are still checked only on the page,
with existing refill/budget/unknown-I/O semantics.

An early executor-suite run found two genuine compatibility regressions: the
first running checkpoint was inheriting admission's liveness delay. Resetting
the probe window on the pending-to-running state transition preserved both
existing tests without changing their assertions. The final executor run passed
99 tests (one ignored). The offline quality self-test passed its goldens and
112 tests (one platform-specific skip). Final gate results and any environmental
limitations are recorded in the delivery declaration.

The final search-index library gate passed 124 tests (three intentional ignores),
the grep-executor gate passed 18 (one manual perf probe ignored), glob passed
four, watcher-filter passed 37, runtime-drain passed 57, and all eight counter
regressions passed after mutation restoration. Rust formatting passed with
`rustfmt 1.10.0-stable`; tests used Cargo/Rust 1.99.0. The shared machine's compile
queue prevented the Windows cross-check and native binary gate from finishing
within their 120-minute limits. Neither is reported as passed. The later
engine/list-envelope/integration/watcher-integration/semantic commands in that
serial gate chain were consequently not reached. At the reviewer's direction,
no replacement builds were started; those gates and the byte-identical quality
evaluation are left to the train gate. The descriptor is committed for that
evaluation, but the self-test alone is not evidence of engine result parity.
