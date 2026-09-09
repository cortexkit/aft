# BQ0 search-quality harness

BQ0 pins all evidence to `30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17`. Moving that value requires regenerating `load-bearing-citations.json`, both campaign references, the census plan, corpus bundle, vectors, and manifest; `imports-resolve.sh` rejects a partial move.

## Authoring

Run from the repository root:

```sh
python3 benchmarks/aft-search/author_real_query.py \
  --census-dir .alfonso/data/aft-search-followup-census \
  --expected-digests benchmarks/aft-search/census-artifacts.sha256.json \
  --output benchmarks/aft-search/real-query-manifest.json \
  --plan-output benchmarks/aft-search/real-query-sample-plan.json
```

The command validates all four source digests, 6,469 unique identities, the five exact populations, and 300 retained labels. It uses numeric identity sorting and the frozen R35 BLAKE3/ChaCha8 shuffle. The mechanism estimates total 6,470 because each stratum projection is rounded independently; they are not an identity partition. `mechanism_record` in the pinned census `mechanism.py` produces the source frame consumed by that projection.

The bundle pruner takes only a repository and immutable SHA. It receives no query, opened file, label, or score. The committed manifest contains 43 rows whose labels resolve in the pinned aft tree; 257 retained cross-repository rows preserve their known provenance and are explicitly excluded as `repo_unowned_or_unavailable`. There is no silent omission or census dependency at scoring time.

## Modes and bootstrap

`cost-gate.sh --search-quality` defaults to `evaluate`. `verify` writes no reference. Initial `record-reference` is reserved for B0; later manifest maintenance must use `record-reference --manifest-changed`. A missing baseline is an exit-2 input fault, never a verify fallback. Consequently BQ0 does not claim ordinary gate acceptance until B0 commits `real-query-baseline.json` and `manifest.sha256`.

The CI mode exception is narrow: only a complete diff containing the manifest and no paths outside that manifest and `bundles/**` selects verify. A completed manifest/reference maintenance diff selects evaluate. Descriptor class is always derived from the same audited diff. A conflicting declaration is an input fault; an undescribed ranking diff is a regression.

## Offline vectors and profiles

`embedding_fixture_server.py` binds loopback only and serves exact keys from checked-in packs. Corpus keys are `(pinned SHA, SHA-256 chunk content, template version)` and query keys are `(SHA-256 query text, template version)`. Unknown keys return `vector_missing`; zero vectors are never substituted. `record_vectors.py` is a separate authoring command requiring an explicit endpoint and is not selected by CI.

`single_page` makes one `topK:100` request with no offset. `paged` requires an explicit schema declaration and a successful 0/100 probe, then permits offsets 0/100/200/300. The 10/4/1 invariance plans are legal public requests. Stop precedence is `page_cap` before `exhausted` before `ten_files`; `depth_cap` is rejected.

## Re-measure assumptions

The post-release estimator treats sampled label outcomes and measured-window D/N as independent. D/N is a binomial proportion, covariance is zero, and the delta variance is `q² Var(p_w) + p_w² Var(q)`. This is an explicit approximation, not a precision or improvement claim. Reports place 3,996/6,469 conditional and 3,996/20,183 projected comparisons side by side; the conditional interval is never compared with 0.198.

Required-check registration remains **NOT MET** until the repository owner supplies the external branch-protection PUT receipt.

## BQ0 repair delivery

The exact-recall fixtures target ripgrep, Flask, Fastify, and Turborepo rather
than the AFT tree, so this repair uses the external-corpus option. Run
`python3 benchmarks/aft-search/provision_corpus.py` once while network access is
allowed. It fetches each declared commit at depth one and records
`.bench/repos/provisioned.json`. `run_exact_recall.py` never fetches; offline
runs either validate those checkouts or exit 2 as
`corpus_missing:<name>:run=python3 benchmarks/aft-search/provision_corpus.py`.

`run_real_query.py` extracts the checked-in AFT bundle into a temporary tree,
uses empty temporary storage and model caches, starts the embedding fixture on
loopback, and invokes public `search` calls through standalone AFT's NDJSON
`tool_call` command. `AFT_BINARY_PATH` overrides the default release binary.
The fixture pack was captured from the actual semantic chunks at the pinned
product state; unknown texts still return `vector_missing` during gate runs.

The following is the complete 2026-09-09 record-reference dry-run transcript.
Absolute worktree prefixes are written as `$ROOT`; the executed checkout was
clean before ignored `.bench` and `target` runtime artifacts were created.

```text
$ scripts/telemetry/cost-gate.sh --search-quality --mode record-reference --dry-run
quality_command:python3 $ROOT/benchmarks/aft-search/run_exact_recall.py --corpus $ROOT/benchmarks/aft-search/corpus/corpus.toml --check-corpus
corpus_check:ok:$ROOT/benchmarks/aft-search/.bench/repos
quality_exit:0
quality_corpus:$ROOT/benchmarks/aft-search/.bench/repos
quality_binary:$ROOT/target/release/aft
quality_command:python3 $ROOT/benchmarks/aft-search/run_exact_recall.py --binary $ROOT/target/release/aft --corpus $ROOT/benchmarks/aft-search/corpus/corpus.toml --out $ROOT/benchmarks/aft-search/.bench/search-quality/exact.json --ready-timeout 600.0
## AFT exact-recall gate

| Repository | Family | Passed | Total | Recall | Exact markers |
| --- | --- | ---: | ---: | ---: | ---: |
| fastify | sentence | 2 | 2 | 1.000 | 2 |
| fastify | pair | 2 | 2 | 1.000 | 2 |
| flask | sentence | 2 | 2 | 1.000 | 2 |
| flask | pair | 2 | 2 | 1.000 | 2 |
| ripgrep | sentence | 2 | 2 | 1.000 | 2 |
| ripgrep | pair | 2 | 2 | 1.000 | 2 |
| turborepo | sentence | 2 | 2 | 1.000 | 2 |
| turborepo | pair | 2 | 2 | 1.000 | 2 |

Sentence rank-1: **1.000** (baseline 1.000)
Pair recall@10: **1.000** (baseline 1.000)
wrote $ROOT/benchmarks/aft-search/.bench/search-quality/exact.json
quality_exit:0
quality_command:python3 $ROOT/benchmarks/aft-search/run_concept_recall.py --output $ROOT/benchmarks/aft-search/.bench/search-quality/concept.json
quality_exit:0
quality_command:python3 $ROOT/benchmarks/aft-search/run_real_query.py --manifest $ROOT/benchmarks/aft-search/real-query-manifest.json --profile single_page --binary $ROOT/target/release/aft --schema $ROOT/packages/pi-plugin/src/tools/semantic.ts --exact-score $ROOT/benchmarks/aft-search/.bench/search-quality/exact.json --concept-score $ROOT/benchmarks/aft-search/.bench/search-quality/concept.json --reference $ROOT/benchmarks/aft-search/real-query-baseline.json --output $ROOT/benchmarks/aft-search/.bench/search-quality/score.json --ready-timeout 600.0
real_query_rows:43
real_query_score:$ROOT/benchmarks/aft-search/.bench/search-quality/score.json
real_query_score_sha256:ab320e82ffb95c1b179faa3220ad1df1847fa8f674a5c8918ec3ab8b49ba88f4
quality_exit:0
quality_command:python3 $ROOT/benchmarks/aft-search/search_quality.py --mode record-reference --manifest $ROOT/benchmarks/aft-search/real-query-manifest.json --reference $ROOT/benchmarks/aft-search/real-query-baseline.json --sidecar $ROOT/benchmarks/aft-search/manifest.sha256 --base-ref HEAD^ --head HEAD --score $ROOT/benchmarks/aft-search/.bench/search-quality/score.json --dry-run
{"dry_run":true,"identity_delta":{"added":["followup-census:584","followup-census:832","followup-census:3184","followup-census:4111","followup-census:4112","followup-census:4212","followup-census:7617","followup-census:7656","followup-census:7670","followup-census:7695","followup-census:7744","followup-census:7956","followup-census:8637","followup-census:8676","followup-census:8679","followup-census:8886","followup-census:9034","followup-census:9173","followup-census:9365","followup-census:9372","followup-census:10215","followup-census:10672","followup-census:10820","followup-census:11468","followup-census:12976","followup-census:13398","followup-census:13619","followup-census:14337","followup-census:14369","followup-census:14449","followup-census:14613","followup-census:14964","followup-census:15174","followup-census:16765","followup-census:17173","followup-census:17208","followup-census:17364","followup-census:18091","followup-census:18338","followup-census:18341","followup-census:18413","followup-census:19269","followup-census:19696"],"changed":[],"removed":[]},"new_reference_sha256":"273d6b840788b9cb78afc3370c8b33eb2f8f7af4dbb0802338bb1a97cead9c02","old_reference_sha256":null,"would_write":[{"path":"$ROOT/benchmarks/aft-search/real-query-baseline.json","sha256":"273d6b840788b9cb78afc3370c8b33eb2f8f7af4dbb0802338bb1a97cead9c02"},{"path":"$ROOT/benchmarks/aft-search/manifest.sha256","sha256":"9397b92b4601f61cccb23db917bf3fe0420f8414f1230786f8d9270ce4cedea6"}]}
quality_exit:0
exit_code=0
```

No reference or sidecar was written. Recording that pair remains B0's act.
