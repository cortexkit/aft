# Search hot-path work-count harness

Use the **dev profile** on the same machine for before/after comparisons. Wall
clock values include host scheduling and must not be presented as release
latencies. Work counts are the primary evidence.

```bash
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo build -p agent-file-tools --bin aft -j 2
python3 benchmarks/aft-search/run_hot_path.py --binary target/debug/aft \
  --corpus "$PWD" \
  --revision 840f9a0d3e28699bc5a4b37d49464cd7c86e833d \
  --out benchmarks/aft-search/.bench/hot-path-before.json
python3 benchmarks/aft-search/run_hot_path.py --binary target/debug/aft \
  --corpus "$HOME/Work/OSS/opencode" --revision 5716f8ba60e79ec60ec485b6e5291c0b0bc1f252 \
  --out benchmarks/aft-search/.bench/hot-path-opencode-before.json
bash benchmarks/aft-search/run_hot_path_counts.sh "$PWD" --revision 840f9a0d3
bash benchmarks/aft-search/run_hot_path_counts.sh "$HOME/Work/OSS/opencode" --revision 5716f8ba60e79ec60ec485b6e5291c0b0bc1f252
```

The Python runner snapshots each real corpus into a temporary, non-worktree
root and starts standalone AFT with a separate fresh `TemporaryDirectory`
storage root. This matters: AFT worktrees borrow read-only artifacts, so an
isolated empty storage directory cannot supply their base index. Git-root
corpora use `git archive --revision`; other directories are copied. Use a pinned
revision across before/after runs, and run the two corpora separately when their
revision IDs differ. It runs fixed natural-language, quoted code-literal,
identifier, common-plus-rare-token, regex and log-excerpt queries, recording each
complete response, actual lane plan and per-request elapsed time. Literal and
log-excerpt cases assert their routed shapes; the latter exercises anchoring.
`--case` selects a subset without changing a query. `--semantic` enables the installed embedding
backend; without it the standalone run measures lexical/exact/anchored paths.
First requests and subsequent memo hits are separate rows, not pooled averages.

The Rust runner indexes real files of at most 1 MiB using the ignore-aware
walker, then measures the lexical, exact and anchored lanes independently.
`HOT_PATH` JSON lines report allocations, posting materializations, source read
attempts, retained-literal regex compilations, score evaluations and sort input
sizes. The regex counter excludes the single query-splitting matcher and lazy
identifier tokenization matchers. Counters
are test-only; ordinary builds have no added counter calls. Posting counts are
materializations, not physical device reads. Sort input counts include all
instrumented lexical rank sorts plus final canonical sorting. Read counters
exclude corpus setup, metadata probes and unrelated parser internals.

Two additional ignored tests measure missing/distant-token exactness and a
controlled 200-file, 40-functions-per-file semantic corpus with 384-dimensional
deterministic vectors. Those vectors are **not** a ranking-quality substitute.
The semantic test measures search, refreshes of 1 and 100 files (unchanged text,
so embedding reuse is exercised), then borrowed-base filtering. Process-wide
allocation counting includes Rayon workers and reports cumulative requested
bytes, not peak/live heap. Run with `--test-threads=1` and do
not mix these ignored tests with unrelated work in the same test process.

The independent ranking gates must use the actual dev binary:

```bash
export AFT_SEARCH_BENCH_RPC_TIMEOUT=600
AFT_BINARY_PATH="$PWD/target/debug/aft" scripts/telemetry/cost-gate.sh \
  --search-quality --mode record-reference --dry-run
python3 benchmarks/aft-search/run_exact_recall.py --binary "$PWD/target/debug/aft" \
  --out benchmarks/aft-search/.bench/hot-path-exact.json
AFT_BINARY_PATH="$PWD/target/debug/aft" python3 benchmarks/aft-search/run_concept_recall.py \
  --output benchmarks/aft-search/.bench/hot-path-concept.json
```

`AFT_SEARCH_BENCH_RPC_TIMEOUT` is an opt-in transport wait floor for the benchmark
clients. It does not change any engine query budget, shape, score or row. The
existing 30-second status/60-second query waits expired on the dev baseline
under host load above 190; they are not appropriate latency assertions here.

Read the benchmark README's macOS FSEvents warning before interpreting semantic
row differences. A changing row is not permission to update the baseline.

## Source checks before optimization

- PERF-01: confirmed per-trigram calls to the complete lexical ranker, followed
  by an all-trigram rank and path-set restriction. Membership cardinality ties
  use the trigram value, not the ranker's stable input order.
- PERF-02: confirmed full sort/dedup before the 3,200-candidate cap. Any partial
  selection must preserve the winning duplicate's evidence and selected-pool
  exhaustion count, not merely its path.
- PERF-03: the union is also needed for definition evidence: the exact verifier
  accepts a symbol whose name equals **any** content token. Intersecting all
  token postings would discard valid definitions even when E2 cannot match.
- PERF-04: confirmed repeated window joins/token-set construction after the
  whole-file substring precheck.
- PERF-05: confirmed missing-token lexical window scans. The exact precheck
  cannot be reused verbatim: it requires two tokens, whereas lexical exactness
  currently accepts a single token. Rejecting single-token windows would change
  results.
- PERF-06: confirmed independent source reads in lexical exactness, exact corpus
  verification and anchored verification. A cache must retain each lane's
  admission/decoding behavior and must not hide mutations between reads without
  explicitly defining a request snapshot contract.
- PERF-07/08: confirmed full per-window occurrence filtering and repeated
  escaped case-insensitive literal matcher compilation for every candidate.
- PERF-13/14: confirmed corpus-sized score scratch storage and repeated borrowed
  path joins/predicate calls per chunk. Comparator index tie-breaking and
  cancellation checks every 64 entries must remain unchanged.
- PERF-15/17: confirmed full-entry reuse/removal scans and cloned refresh delta.
  Both the refreshed index and returned delta own their entries; removing the
  clone without changing ownership would break the apply/update contract.
- PERF-16: admission is based on **successful** collection, not merely requested
  paths. Failed changes free capacity for new files. An early cap based on
  requested counts would change which files are admitted.

## Comparing runs and reading results

`HOT_PATH_MEASURED.md` is the final counts/latency table; the preliminary source
checks and first measurements are retained separately in `HOT_PATH_RESULTS.md`.
The machine-readable counts, recall rows and standalone parity live in
`hot-path-*.json`.

```bash
python3 benchmarks/aft-search/compare_hot_path.py before.json after.json
python3 benchmarks/aft-search/compare_hot_path_counts.py before.log after.log --out counts.json
```

The standalone comparator removes only known terminal health/repetition
footers. It preserves all result rows, ranking, snippets, evidence labels,
counts, partial-result warnings and continuation messages. It reports every
case that differs and fails if any differ. The supplemental regex case is a
known baseline-unstable budgeted fallback on AFT; its actual before-versus-before
row diff is recorded in `HOT_PATH_FALLBACK_INSTABILITY.md`, not suppressed by
the comparator. The task owner authorized treating this proven pre-existing
fallback instability separately, while requiring the requested ranked lanes,
exact/concept recall and search-quality gate to remain unchanged.

The work-count comparator normalizes only temporary corpus-root prefixes before
hashing lane results. The measurement tests assert nonempty corpora and an
8000-chunk semantic fixture, so skipped indexing cannot look like free work.
