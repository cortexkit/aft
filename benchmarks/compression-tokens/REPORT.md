# Compression Token Counting Spike

## Methodology

This spike measures AFT bash output compression fixtures through the real Rust compression dispatch (`compress_with_registry`) using built-in TOML filters and Rust compressors. A Cargo integration test writes `data/spike-output.json`; this Bun script tokenizes each original/compressed pair with `ai-tokenizer` Claude encoding and compares it with byte-ratio estimates.

- Fixtures: 26 realistic bash outputs across git, build/test, lint, filesystem, deploy/container, plus one generic fallback sample.
- Option A: precise Claude token counts using `ai-tokenizer@^1.0.6`.
- Option B: byte approximation using both 3.5 bytes/token and code-leaning 4.0 bytes/token.
- IPC-cost proxy: elapsed time to tokenize all original and compressed fixture texts in-process with `ai-tokenizer`.

## Overall Aggregate

| fixtures | A saved | B saved 3.5 | B drift 3.5 % | B saved 4.0 | B drift 4.0 % | tokenization ms |
| --- | --- | --- | --- | --- | --- | --- |
| 26 | 11324 | 9811.1 | -13.4 | 8584.8 | -24.2 | 6.22 |

## Per-tier Breakdown (3.5 bytes/token)

| tier | n | A saved | B saved | overall drift % | mean fixture drift % | median fixture drift % | p95 abs drift % | max abs drift % | calibrated bytes/saved-token |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| rust modules | 15 | 1354 | 1288.6 | -4.8 | -23.6 | -11.8 | 71.4 | 71.4 | 3.34 |
| toml filters | 10 | 5610 | 4408.9 | -21.4 | -43.5 | -44.6 | 71.4 | 71.4 | 2.75 |
| generic | 1 | 4360 | 4113.7 | -5.6 | -5.6 | -5.6 | 5.6 | 5.6 | 3.30 |

## Per-tier 4.0 Variant

| tier | n | B saved 4.0 | overall drift 4.0 % |
| --- | --- | --- | --- |
| rust modules | 15 | 1127.5 | -16.7 |
| toml filters | 10 | 3857.8 | -31.2 |
| generic | 1 | 3599.5 | -17.4 |

## Per-fixture Measurements

| fixture | tier | command | orig bytes | compressed bytes | A pre | A post | A saved | B3.5 saved | B3.5 drift | B4.0 saved | B4.0 drift |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| git/status.txt | rust modules | git status --short --branch | 214 | 213 | 72 | 71 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| git/log-oneline.txt | rust modules | git log --oneline --decorate -25 | 560 | 559 | 167 | 166 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| git/diff.txt | rust modules | git diff -- crates/aft/src/compress/mod.rs | 997 | 996 | 339 | 338 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| git/fetch.txt | rust modules | git fetch origin main | 495 | 122 | 160 | 44 | 116 | 106.6 | -8.1% | 93.3 | -19.6% |
| git/push.txt | rust modules | git push origin feature/compress-metrics | 623 | 105 | 195 | 31 | 164 | 148.0 | -9.8% | 129.5 | -21.0% |
| build-test/cargo-test.txt | rust modules | cargo test | 1335 | 259 | 434 | 77 | 357 | 307.4 | -13.9% | 269.0 | -24.6% |
| build-test/cargo-build.txt | rust modules | cargo build --release | 501 | 500 | 194 | 193 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| build-test/npm-install.txt | rust modules | npm install | 639 | 312 | 205 | 82 | 123 | 93.4 | -24.0% | 81.8 | -33.5% |
| build-test/pnpm-install.txt | rust modules | pnpm install | 540 | 180 | 160 | 54 | 106 | 102.9 | -3.0% | 90.0 | -15.1% |
| build-test/pytest.txt | rust modules | pytest -q | 1602 | 877 | 386 | 220 | 166 | 207.1 | 24.8% | 181.3 | 9.2% |
| lint/eslint.txt | rust modules | eslint . --format stylish | 619 | 546 | 187 | 169 | 18 | 20.9 | 15.9% | 18.3 | 1.4% |
| lint/biome-check.txt | rust modules | biome check . | 900 | 61 | 262 | 16 | 246 | 239.7 | -2.6% | 209.8 | -14.7% |
| lint/tsc-noemit.txt | rust modules | tsc --noEmit | 658 | 424 | 189 | 127 | 62 | 66.9 | 7.8% | 58.5 | -5.6% |
| lint/eslint-clean.txt | rust modules | eslint src --format stylish | 0 | 0 | 0 | 0 | 0 | 0.0 | n/a | 0.0 | n/a |
| lint/tsc-clean.txt | rust modules | tsc --noEmit --pretty false | 0 | 19 | 0 | 8 | -8 | -5.4 | -32.1% | -4.8 | -40.6% |
| filesystem/find.txt | toml filters | find . -maxdepth 3 -type f | 7500 | 5019 | 2550 | 1707 | 843 | 708.9 | -15.9% | 620.3 | -26.4% |
| filesystem/ls-la.txt | toml filters | ls -la | 8982 | 6919 | 3771 | 2904 | 867 | 589.4 | -32.0% | 515.8 | -40.5% |
| filesystem/tree.txt | toml filters | tree -a -L 3 | 5963 | 3268 | 1706 | 938 | 768 | 770.0 | 0.3% | 673.8 | -12.3% |
| filesystem/du-sh.txt | toml filters | du -sh node_modules target .git benchmarks | 55 | 54 | 26 | 25 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| filesystem/df-h.txt | toml filters | df -h | 584 | 583 | 240 | 239 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| deploy-container/docker-ps.txt | toml filters | docker ps | 6220 | 456 | 2426 | 131 | 2295 | 1646.9 | -28.2% | 1441.0 | -37.2% |
| deploy-container/kubectl-pods.txt | toml filters | kubectl get pods -A | 10215 | 7797 | 3560 | 2731 | 829 | 690.9 | -16.7% | 604.5 | -27.1% |
| deploy-container/gh-run-list.txt | toml filters | gh run list --limit 20 | 718 | 717 | 173 | 172 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| deploy-container/terraform-plan.txt | toml filters | terraform plan | 1141 | 1135 | 262 | 258 | 4 | 1.7 | -57.1% | 1.5 | -62.5% |
| deploy-container/helm-list.txt | toml filters | helm list -A | 677 | 676 | 198 | 197 | 1 | 0.3 | -71.4% | 0.3 | -75.0% |
| deploy-container/journalctl.txt | generic | journalctl -u aft-worker -n 80 | 18524 | 4126 | 5694 | 1334 | 4360 | 4113.7 | -5.6% | 3599.5 | -17.4% |

## Recommendation

**Recommendation: A.** Ship Option A (precise ai-tokenizer counts), with a size cap/fallback for very large blobs.

Decision rule evaluation:

- B aggregate drift at 3.5 bytes/token: -13.4% (fails the <5% aggregate criterion).
- Total tokenization time for 26 fixtures: 6.22ms (does not pass the >50ms IPC-cost criterion).
- Any tier over 15% aggregate drift: yes.

Calibrated byte ratios from this fixture set (saved bytes / precise saved tokens):

| tier | calibrated bytes/saved-token |
| --- | --- |
| rust modules | 3.34 |
| toml filters | 2.75 |
| generic | 3.30 |
