# Exact-tier retrieval measurements

Measured against base `5001d938ffc190ee029156bc4f3e0c6c418a178e` and the final exact-tier implementation on this branch. The concept family uses the seven ripgrep tasks from `external-fixtures.json`, `top_k=10`, and the runner's standard line-overlap relevance.

## Concept family (three independent runs)

| Binary | Run | MRR@10 | R@10 |
| --- | ---: | ---: | ---: |
| Before | 1 | 0.571 | 0.690 |
| Before | 2 | 0.571 | 0.690 |
| Before | 3 | 0.571 | 0.690 |
| After | 1 | 0.619 | 0.690 |
| After | 2 | 0.619 | 0.690 |
| After | 3 | 0.619 | 0.690 |

The observed noise band (maximum minus minimum over the three repetitions) was 0.000 for both metrics on each binary. R@10 did not regress; MRR@10 increased by 0.048.

## Exact recall before

The corrected scorer treats only rank as pass/fail. `Exact markers` is reported independently and does not influence recall.

| Repository | Family | Passed | Total | Recall | Exact markers |
| --- | --- | ---: | ---: | ---: | ---: |
| fastify | sentence | 2 | 2 | 1.000 | 0 |
| fastify | pair | 0 | 2 | 0.000 | 0 |
| flask | sentence | 2 | 2 | 1.000 | 0 |
| flask | pair | 0 | 2 | 0.000 | 0 |
| ripgrep | sentence | 2 | 2 | 1.000 | 0 |
| ripgrep | pair | 0 | 2 | 0.000 | 0 |
| turborepo | sentence | 2 | 2 | 1.000 | 0 |
| turborepo | pair | 1 | 2 | 0.500 | 0 |

Sentence rank-1: **1.000**. Pair recall@10: **0.125**.

## Exact recall after

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

Sentence rank-1: **1.000**. Pair recall@10: **1.000**.
