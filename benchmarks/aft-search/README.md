# AFT search eval harness

Manual retrieval benchmarks for `aft_search`. The original in-tree fixture suite
still measures AFT on this repository. The external suite adds Vera's published
21-task corpus so we can report Recall@1/5/10, MRR@10, and nDCG@10 against the
same pinned repos Vera uses.

## Setup

Build the release binary first:

```bash
cargo build --release -p agent-file-tools
```

Provision the Vera-compatible corpus and pinned real-query evidence from the
repository root:

```bash
python3 benchmarks/aft-search/provision_corpus.py
python3 benchmarks/aft-search/provision_evidence.py
```

`provision_corpus.py` reads `corpus/corpus.toml`, fetches each immutable commit
with depth one into `.bench/repos/<name>/`, and writes
`.bench/repos/provisioned.json`. `provision_evidence.py` materializes the pinned
AFT commit under `.bench/repos/aft-evidence-<sha>/`, using the local Git object
when available and fetching that SHA from origin otherwise. Its digest is the
SHA-256 of a canonical map from every projected relative path to that file's
SHA-256. `.bench/` is ignored by git and must not be committed. The runners
validate the exact-recall record and the complete evidence-tree digest before
starting AFT.

## In-tree benchmark

The existing AFT fixtures are unchanged in `fixtures.json` and still use
file-path expected results (`expected_top_files`). From `benchmarks/aft-search`:

```bash
uv run python run.py
```

Equivalent explicit invocation:

```bash
uv run python run.py \
  --binary ../../target/release/aft \
  --project-root ../.. \
  --out baseline.json
```

The runner starts `aft`, sends a user-tier `configure` document with search and
semantic search enabled, waits for the semantic index to be ready, runs
`semantic_search(top_k=5)` for every fixture, and writes the baseline-shaped JSON
report. A local embedding model and ONNX Runtime are required.

## Exact-recall gate

`exact-recall-fixtures.json` contains two deterministic sentence samples and two
2-3 content-token co-occurrence samples from comments or strings in each pinned
corpus repository. Sentence fixtures require the containing file at rank 1;
co-occurrence fixtures require it within the top 10. The report records the
result's `[exact]` classification separately; the marker does not affect recall.

The exact-recall runner deliberately disables semantic search, so the nightly
cost gate can exercise lexical retrieval and exact-tier fusion without an
embedding backend. The existing concept benchmark remains separate because the
nightly runner does not currently install its model/runtime prerequisites.

```bash
python3 provision_corpus.py
python3 run_exact_recall.py \
  --binary ../../target/release/aft \
  --out results/exact-recall.json
```

The command compares against `exact-recall-baseline.json` and exits nonzero if
sentence rank-1 or pair recall@10 drops below the checked-in baseline. The
nightly workflow appends the same table to its job summary and uploads the JSON
beside the index-cost artifacts.

## Real-query quality gate

After the exact-recall corpus has been provisioned once, this command provisions
or repairs the pinned real-query evidence tree, builds `target/release/aft`
unless `AFT_BINARY_PATH` names a binary, runs exact recall and concept recall,
replays every included real-query row, writes an ignored score under `.bench/`,
and passes that score to the predicate:

```bash
scripts/telemetry/cost-gate.sh --search-quality --mode record-reference --dry-run
```

The real-query replay verifies and consumes the provisioned pinned tree, starts
the loopback-only embedding fixture server with empty temporary caches, and calls the
public `search` tool through standalone AFT's `tool_call` NDJSON command. The
`single_page` profile sends one request at the product's maximum `topK` without
an offset. The `paged` profile requires a declared, working offset, sends enough
maximum-size pages to cover the frozen 400-result scoring depth, and separately
executes page-size invariance plans that retrieve the same 100 rows. The harness
sets that maximum once as `PAGE_SIZE`; a test compares it with the checked-in
`aft_search.topK.maximum` schema so a later product-cap change fails by name
instead of turning every replay row into an invalid request. Every request
forwards the row's recorded `includeTests` value. When the pinned product's
semantic chunk format intentionally changes, refresh the checked-in
allowlist in an authoring environment with
`python3 benchmarks/aft-search/capture_real_query_vectors.py --allow-vector-authoring`;
normal gate execution never derives a vector on a miss.

Run the independently named cases with, for example:

```bash
cd benchmarks/aft-search
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_missing_checkout_is_provisioned_from_local_repository_with_manifest_digest
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_mutated_evidence_tree_is_rejected_with_mismatch_fault
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_runner_is_byte_deterministic_on_the_same_tree
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_single_page_request_grammar_rejects_an_offset
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_paged_profile_covers_frozen_depth_and_runs_invariance_requests
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_stop_token_precedence_is_page_cap_then_exhausted_then_ten_files
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_missing_exact_recall_corpus_names_provision_command
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_recorded_include_tests_changes_ranked_paths
```

### Re-recording the reference

`real-query-baseline.json` and its `manifest.sha256` sidecar are the byte-equality
reference: the `engine_unwired` class asserts every ranked row is byte-equal to
it. Moving that pair is an audited act, never a repair to make a red gate green.
Re-record only after the cause of the difference is established, on one binary,
with `cost-gate.sh --search-quality --mode record-reference`, and say in the
commit message which change moved the rows and why it was the right direction.

Re-records so far:

- 2026-09-18, after ranking slice 3, and 2026-09-20, after the `includeTests`
  coercion moved the capability schema hash. Both are written up in
  `docs/investigations/search-grep-sweep-2026-09-17.md`.
- 2026-09-21, after the answer-key corpus exclusion in commit `0b14e8b2d`. That
  commit stopped the benchmark's own fixtures and recorded reports from being
  indexed for the real-query replay, which changed the candidate pool and moved
  26 of the 43 rows; see the corpus hygiene section below. Cause established by
  running the replay twice on one binary, once with and once without the
  evidence-tree ignore list, against the same manifest, vector pack and pinned
  tree digest: without it every row is byte-equal to the old reference, with it
  the score reproduces the numbers CI reported. Re-recorded on a release build of
  `aft 0.56.2`: 43 rows, `paged`, MRR@10 0.187984 -> 0.188760, census-weighted
  MRR 0.151837 -> 0.152462. Exact recall and concept recall were re-measured in
  the same run and are unchanged at 1.000; the ignore list is copied only into
  the real-query evidence tree, and those two families score against the pinned
  external clones and the offline vector pack instead.

## Search-fusion quality sub-benchmark

`run-fusion-quality` is a focused investigation harness for hybrid fusion
ranking. It runs the existing in-tree `fixtures.json` plus
`identifier-fusion-fixtures.json`, asks `aft_search` for a wide top-100
candidate pool, and applies bench-only offline rerankers (RRF, exact-identifier
first, and uncapped identifier lexical score) without changing production Rust.
From `benchmarks/aft-search`:

```bash
python3 run-fusion-quality \
  --binary ../../target/release/aft \
  --project-root ../.. \
  --out results/search-fusion-quality.json \
  --summary results/search-fusion-quality-summary.tsv
```

The script auto-detects the managed ONNX Runtime at
`~/.local/share/cortexkit/aft/onnxruntime/1.24.4/` when `ORT_DYLIB_PATH` is not
set (on Windows, `%LOCALAPPDATA%\cortexkit\aft\onnxruntime\1.24.4\onnxruntime.dll`).
It writes a detailed JSON run manifest plus a TSV aggregate summary used by
`.alfonso/reports/search-fusion-quality.md`.

## Corpus hygiene: the benchmark's own answer key

`fixtures.json`, `identifier-fusion-fixtures.json`, `baseline.json` and the
recorded reports each contain every fixture's query **and** its
`expected_top_files`. When they are part of the indexed corpus they become
self-fulfilling exact hits for their own queries, crowd the top of the result
list, and push down the file the fixture is actually looking for.

`.aftignore` in this directory keeps them out of the index AFT builds, matching
the exclusion `run-fusion-quality` already applies to its bench-only
exact-match oracle. `run_real_query.py` copies the same list into the runtime
copy of the evidence tree, after the pinned digest has been verified, so the
digest is unaffected.

Changing what the harness indexes is a ranking change, even when the query sets
do not intersect. The evidence-tree copy was added on the reasoning that no
real-query row shares a query with a fusion fixture, so it could not matter; that
does not follow. Non-intersecting queries say nothing about the corpus, and
removing documents changes the candidate pool every query competes in. It moved
26 of the 43 real-query rows. Twelve of them had an excluded file inside their
own recorded result list; the other 14 never named one and moved through the
pool alone. One row's metrics moved, because `results/aft-vera-suite-baseline.json`
had been ranked above the file that episode opened and pushed it out of the top
five; the other 25 rows changed bytes in the deeper recorded list without moving
a metric, which is why the reference asserts byte equality rather than metrics.
The change landed under a `non_ranking` descriptor, which does not compare rows,
so the drift went unmeasured until the next `engine_unwired` descriptor refused
on it. A commit that adds or removes an ignore entry, or otherwise changes which
files reach the index, belongs under a descriptor class that compares rows and
carries the re-recorded reference with it.

The deflation this prevents runs in the dangerous direction: a suppressed
baseline makes every candidate ranking change look better than it is. Any new
file in this directory that carries `expected_top_files` has to be added to
`.aftignore`; `test_harness_integrity.py` fails until it is.

`baseline.json` records where its numbers came from in a `measured_on` block,
the way the cost-gate baselines do, including whether the answer-key files were
excluded from the indexed corpus at capture time.

## Running on Windows

All stages except the real-query replay run on Windows.

- The NDJSON clients read the aft process's stdout on a reader thread rather
  than with `select.select()`, which on Windows accepts sockets only and fails
  a pipe with `WinError 10038`.
- AFT returns absolute paths in the `\\?\C:\...` verbatim form.
  `normalize_result_path` strips that prefix, without which every comparison
  against a relative `expected_top_files` entry misses and the run reports a
  clean 0.000 on a healthy index.
- `.gitattributes` pins the benchmark JSON to LF, so the vector pack still
  matches its `embedding_pack_sha256` under `core.autocrlf=true`.
- The real-query replay refuses to start and exits **3**: its vector pack and
  baseline are Unix-captured, and the text AFT embeds bakes the OS-native
  relative path into every chunk header, so a Windows run cannot reproduce
  them. Exit 3 is distinct from the exit 2 used for ordinary input faults.

The platform-independent cases live in `test_harness_integrity.py` and run
everywhere:

```bash
cd benchmarks/aft-search
python3 -m unittest -v test_harness_integrity
```

## Method notes

**Patches must be installed before the harness is imported.** The runners do
`from run import normalize_result_path`, which binds the name at import time.
Driving one of them from an external script and patching
`run.normalize_result_path` afterwards leaves the harness holding the original
function, and the run looks like an ordinary 0.000 rather than a failed patch.
The same applies to anything else imported by name. For the same reason a fix
belongs in the primitive rather than in a caller: `run_search_quality.py`
spawns each stage as its own `python <script>.py`, so a second client class
running as `__main__` never sees a patch applied to the first.

**A zero is refused, not reported.** The baseline and fusion-quality runners
refuse to write a report in which no fixture matched at all while the semantic
index held entries. A well-formed all-unmatched report is indistinguishable
from a catastrophic ranking regression, and the likelier cause is that result
paths never compared equal to expected paths.

## External Vera-comparable benchmark

After setup, run:

```bash
uv run python run_external.py
```

To write the committed baseline path explicitly:

```bash
uv run python run_external.py --out results/aft-vera-suite-baseline.json
```

The external runner:

1. Reads `corpus/corpus.toml` and `external-fixtures.json`.
2. Starts one `aft` process per corpus repo with `project_root` set to that clone.
3. Configures `search_index`, `semantic_search`, `experimental_search_index`, and
   `experimental_semantic_search` with a per-run temporary `storage_dir`.
4. Waits up to 600 seconds for the search and semantic indexes to report `ready`.
5. Runs `semantic_search(top_k=10)` for each task in that repo.
6. Scores results with line-range overlap by default.
7. Writes `results/aft-vera-suite-<timestamp>.json` unless `--out` is supplied.

Use `--relevance-mode file-only` only when you intentionally want Vera's
file-path-only mode. The committed baseline uses the stricter default
`line-overlap` mode.

## Metrics and result JSON

`metrics.py` supports both fixture formats:

- In-tree fixtures: file-path match against `expected_top_files`.
- Vera fixtures: `ground_truth[{file_path,line_start,line_end,relevance}]` with a
  prediction relevant only when the file matches and the returned line range
  overlaps the ground-truth range by at least one line.

External result files mirror Vera's report shape at the top level:

- `tool_name`, `timestamp`, `version_info`: reproducibility metadata, including
  binary SHA, repo SHAs, top-k, relevance mode, and reranker status.
- `per_task`: one row per task with ground truth, top results, latency,
  `zero_results`, and `retrieval_metrics`.
- `per_category`: category aggregates for intent, symbol lookup, cross-file,
  disambiguation, and config tasks.
- `aggregate`: overall retrieval metrics and latency p50/p95.

Primary numbers to compare are Recall@1, Recall@5, Recall@10, MRR@10 (`mrr` in
the JSON), nDCG@10, and latency p50/p95.

## What Vera reports vs what we report

| System | Corpus | Reranker | Comparable MRR@10 |
| --- | --- | --- | --- |
| Vera v0.7.0 hybrid | Vera 21-task suite | Cross-encoder reranker on | 0.91 |
| Vera hybrid no-rerank | Vera 21-task suite | Off | 0.34 |
| AFT `aft_search` | Same pinned 21-task suite | Off | See `aggregate.retrieval.mrr` |

Vera's default published number includes a cross-encoder reranker; AFT currently
reports hybrid lexical+semantic retrieval without a reranker. For a fair product
comparison, use Vera's no-reranker baseline from
`Vera/benchmarks/results/final-suite/vera_hybrid_norerank_results.json`.

## Attribution

The external task definitions in `external-fixtures.json` are vendored from
Vera's `eval/tasks/*.json` corpus. The metric formulas in `metrics.py` mirror
Vera's `eval/src/metrics.rs` and are reimplemented independently here rather
than copied wholesale.

## Docker

Run the existing in-tree benchmark without host AFT/Bun/Rust installs:

```bash
bun run bench:aft-search
# or
make run-aft-search
```

The Docker image builds AFT from this checkout and writes `results/aft-search-docker.json` by default. For the external Vera-compatible corpus:

```bash
AFT_SEARCH_MODE=external AFT_SEARCH_OUT=results/aft-vera-docker.json bun run bench:aft-search
```

The compose file mounts `results/` and `.bench/` so reports and cloned corpora persist outside the container.
