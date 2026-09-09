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
