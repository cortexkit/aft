# Qwen3-Embedding query-instruction A/B (2026-09)

## Decision

AFT now supports `semantic.query_instruction = "auto" | "off" | "<literal task>"`, but the shipped default is **`off`**. Both instructed arms slightly reduced concept-family MRR, so they did not meet the predeclared default-on criterion even though real-query MRR and the no-vocabulary diagnostic improved.

Explicit `auto` resolves case-insensitive Qwen3-Embedding model IDs to the model card's verbatim retrieval task:

```text
Instruct: Given a web search query, retrieve relevant passages that answer the query
Query: <query>
```

Source: [Qwen3-Embedding-0.6B usage tips](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B#usage-tips). Other model families, and fastembed regardless of model spelling, receive no instruction. The model-card and code-specific tasks were close: the code-specific arm moved only five queries relative to the model-card arm (four +1 ranks, one -1 rank). That is treated as noise, so `auto` uses model-card bytes to match other CortexKit seats.

## Method

`benchmarks/aft-search/run_query_instruction_ab.py` follows the production-lane body-cap harness: Bionic at `http://localhost:1234/v1`, model `text-embedding-qwen3-embedding-0.6b`, committed concept fixtures, committed exact-recall corpora, and the committed real-query bundle/manifest. Documents, scorers, query sets, top-k contracts, and the embedding model stayed fixed. Only query text differed:

1. `off`: raw query (current behavior)
2. `model-card`: the verbatim task above
3. `code-search`: `Given a code search query, retrieve relevant source code, symbols, and documentation`

The no-vocabulary diagnostic is pure cosine over semantic index rows, independently of hybrid ordering. It applies the exact lane's E2 content-token rules (ASCII identifier tokens, lowercased, minimum three characters, same stopword set) to each query and indexed row text. It reports the share of dense top-10 rows sharing zero content tokens and the expected-file relevance rate inside that band. NumPy is used only for the benchmark's bounded matrix scoring; AFT's production scorer and fixtures are untouched.

Reproduction:

```sh
cargo build -p agent-file-tools --release --bin aft
python3 benchmarks/aft-search/provision_corpus.py
python3 -m venv benchmarks/aft-search/.bench/query-instruction-ab/venv
benchmarks/aft-search/.bench/query-instruction-ab/venv/bin/pip install numpy
benchmarks/aft-search/.bench/query-instruction-ab/venv/bin/python \
  benchmarks/aft-search/run_query_instruction_ab.py
```

## Results

Retrieval metrics are MRR@10 and hit@1. Exact recall also remained 1.0 for both sentence rank@1 and pair recall@10 in every arm.

| Family | Arm | MRR@10 | hit@1 | no-vocabulary share of dense top-10 | relevance inside no-vocabulary band |
|---|---:|---:|---:|---:|---:|
| concept | off | 0.308078 | 0.214286 | 0.621429 | 0.091954 |
| concept | model-card | 0.304904 | 0.214286 | 0.521429 | 0.219178 |
| concept | code-search | 0.306888 | 0.214286 | 0.525000 | 0.204082 |
| exact-recall | off | 1.000000 | 1.000000 | 0.412500 | 0.121212 |
| exact-recall | model-card | 1.000000 | 1.000000 | 0.368750 | 0.152542 |
| exact-recall | code-search | 1.000000 | 1.000000 | 0.412500 | 0.196970 |
| real-query | off | 0.181589 | 0.116279 | 0.639535 | 0.018182 |
| real-query | model-card | 0.189212 | 0.116279 | 0.506977 | 0.022936 |
| real-query | code-search | 0.196964 | 0.139535 | 0.502326 | 0.023148 |

Across all 87 queries and 870 dense top-10 rows, the no-vocabulary share fell from 0.591954 (`off`) to 0.486207 (model-card) and 0.493103 (code-search). Relevance inside that band rose from 0.056311 to 0.108747 and 0.111888 respectively. The off arm therefore exhibits the same noise-admission failure mode observed by Magic Context, though AFT's hybrid retrieval metrics mask much of the pure-dense improvement on keyword-shaped queries.

| Arm | query-embed latency p50 (ms) | p95 (ms) |
|---|---:|---:|
| off | 19.972 | 208.286 |
| model-card | 30.139 | 850.101 |
| code-search | 29.391 | 201.566 |

The model-card p95 includes host-contention outliers during a shared-machine run; its p50 and direct isolated real-query measurements (18.097 ms p50 versus 11.644 ms off) are the useful cost signal. The prefix adds roughly twenty tokens and about 6-10 ms in the uncontended direct-query measurements.

## Per-query movers

`None` means the expected file fell outside the family's measured top-k. Rows absent below were unchanged.

| Comparison | Direction | Family | Query | Before | After |
|---|---|---|---|---:|---:|
| off → model-card | improved | real-query | relative absolute path serialize portability | None | 9 |
| off → model-card | improved | real-query | status snapshot session id tracked_files checkpoints json serialization | 4 | 2 |
| off → model-card | improved | concept | how are imports organized after edits | 10 | 9 |
| off → model-card | improved | real-query | daemon socket ConnectionReset ConnectionAborted Windows terminated EOF | 3 | 2 |
| off → model-card | worsened | concept | process group already terminated | 10 | None |
| off → model-card | worsened | real-query | persisted callgraph store is unavailable in this read-only worktree (`14613`) | 4 | 5 |
| off → model-card | worsened | real-query | persisted callgraph store is unavailable in this read-only worktree (`15174`) | 4 | 5 |
| off → model-card | worsened | real-query | todos category JobOutcome Pending did not complete | 10 | None |
| off → code-search | improved | concept | how are imports organized after edits | 10 | 8 |
| off → code-search | improved | real-query | relative absolute path serialize portability | None | 9 |
| off → code-search | improved | real-query | daemon socket ConnectionReset ConnectionAborted Windows terminated EOF | 3 | 1 |
| off → code-search | improved | concept | bash long running reminder config | 8 | 7 |
| off → code-search | improved | concept | where does lsp diagnostic polling wait for server responses | 7 | 6 |
| off → code-search | improved | real-query | status snapshot session id tracked_files checkpoints json serialization | 4 | 3 |
| off → code-search | worsened | concept | process group already terminated | 10 | None |
| off → code-search | worsened | real-query | persisted callgraph store is unavailable in this read-only worktree (`14613`) | 4 | 5 |
| off → code-search | worsened | real-query | persisted callgraph store is unavailable in this read-only worktree (`15174`) | 4 | 5 |
| off → code-search | worsened | real-query | todos category JobOutcome Pending did not complete | 10 | None |
| model-card → code-search | improved | concept | bash long running reminder config | 8 | 7 |
| model-card → code-search | improved | concept | where does lsp diagnostic polling wait for server responses | 7 | 6 |
| model-card → code-search | improved | concept | how are imports organized after edits | 9 | 8 |
| model-card → code-search | improved | real-query | daemon socket ConnectionReset ConnectionAborted Windows terminated EOF | 2 | 1 |
| model-card → code-search | worsened | real-query | status snapshot session id tracked_files checkpoints json serialization | 2 | 3 |

## Index and cache invariants

The query instruction is formatted only in `embed_query_cached`; build/document text is unchanged. The semantic fingerprint deliberately omits the query-only setting, and unit coverage compares fingerprints for `auto`, `off`, and a literal task. Query cache keys are the final text sent to the provider, so changing instruction mode cannot reuse a bare-query vector. Fixture provider models do not match Qwen3-Embedding and therefore preserve parity fixture bytes.
