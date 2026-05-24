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

Clone the Vera-compatible corpus from this directory:

```bash
cd benchmarks/aft-search
uv run python setup_corpus.py
```

`setup_corpus.py` reads `corpus/corpus.toml`, clones each repo into
`.bench/repos/<name>/`, hard-resets it to the pinned commit, prints file/byte
sizes, and is idempotent. `.bench/` is ignored by git and must not be committed.
If a repo cannot be cloned, the script reports it and continues with the rest.

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

The runner starts `aft`, sends `configure` with search and semantic search
enabled, waits for the semantic index to be ready, runs `semantic_search(top_k=5)`
for every fixture, and writes the baseline-shaped JSON report.

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
