# Semantic chunk token census (2026-09)

Generated 2026-09-12 by the ignored `semantic_chunk_census` integration test. Token counts include the tokenizer's special tokens.

## Production lane's real ceiling

The production lane is the OpenAI-compatible Bionic server at `http://localhost:1234/v1/embeddings` (formerly LM Studio), model `text-embedding-qwen3-embedding-0.6b`. AFT's 400-signature-char / 15-body-line / 300-body-byte / 1,600-total-char guards were written for that server's former llama.cpp 512-token physical-batch limit; the Qwen3-Embedding-0.6B model itself advertises a 32k context. These read-only probes called `curl POST /v1/embeddings` directly, without AFT. Inputs are uncapped embed texts from real functions in the AFT corpus. The exact Qwen3-Embedding-0.6B `tokenizer.json` was absent from the local Hugging Face and fastembed caches, so the token column is explicitly estimated as `characters / 3.5`.

| Probe | Real function source(s) | Characters | Qwen tokens (estimated) | HTTP | Error body | Latency ms | Returned vectors | Full 1024 dimensions? |
|---|---|---:|---:|---:|---|---:|---:|---|
| ~400-token row | `benchmarks/trigram-ab-latency.py::bench_query` | 1401 | 400 | 200 | — | 185.3 | 1 × 1024 | yes |
| ~700-token row | `crates/aft/src/callgraph.rs::resolve_rust_imported_call` | 2451 | 700 | 200 | — | 34.3 | 1 × 1024 | yes |
| ~1100-token row | `benchmarks/src/reporter.ts::printReport` | 3849 | 1100 | 200 | — | 68.6 | 1 × 1024 | yes |
| ~2100-token row | `crates/aft/src/subc/mod.rs::route_bind_does_not_recompute_fleet_health` | 7357 | 2102 | 200 | — | 95.0 | 1 × 1024 | yes |
| ~4200-token row | `crates/aft/src/callgraph_store/mod.rs::refresh_files_profiled_with_workspace_crate_prefix_cache` | 14319 | 4091 | 200 | — | 182.1 | 1 × 1024 | yes |
| 8 × ~700-token rows | `crates/aft/src/callgraph.rs::resolve_rust_imported_call<br>crates/aft/src/commands/configure.rs::configure_artifact_loads_start_only_from_post_ack_maintenance<br>crates/aft/src/gh_shim.rs::activation_retains_every_accepted_manifest_with_exact_bytes<br>crates/aft/src/inspect/manager.rs::assert_fresh_refresh_reuses_projection<br>crates/aft/tests/integration/aft_search_contract_test.rs::identifier_ready_reports_no_more_available_when_under_top_k_without_caps<br>crates/aft/src/alias/mod.rs::seed_proven_alias<br>crates/aft/src/bash_background/persistence.rs::resolve_uninitialized_task_layout<br>crates/aft/src/bash_background/registry.rs::pending_pattern_matches_for_session` | 2447–2455 | 699–701 | 200 | — | 303.4 | 8 × 1024 | yes |

Conclusion: the server's effective per-row ceiling today is at least ~4200 estimated Qwen tokens per row (the largest probe succeeded); lifting AFT's caps would work through the measured ~4,200-token row on the CURRENT lane without any backend change, and the successful 8 × ~700-token batch shows there is no legacy 512-token aggregate batch limit.

## Method

The census used `callgraph::walk_project_files` followed by `is_semantic_indexed_extension`, matching the semantic snapshot's gitignore/global-ignore/`.aftignore` filtering and supported semantic extensions. Files over `MAX_SEMANTIC_FILE_BYTES` (4 MiB) were left at zero chunks, as in production. Each eligible file was parsed once by the production tree-sitter parser and converted twice by the production semantic chunker: today's default caps (signature 400 chars, body 15 lines / 300 bytes, total 1,600 chars) and `ChunkCaps` with all four values set to `usize::MAX`. Large corpora were processed as non-overlapping file-range shards and the cached row records were checked for contiguous, gap-free coverage before aggregation; this bounds parser/tokenizer memory without changing the sorted file set.

Tokenizer: `Alibaba-NLP/gte-modernbert-base` revision `e7f32e3c00f91d699e8c43b53106206bcc72bb22`; `tokenizer.json` SHA-256 `6c8aaa9a542084f2457eab775d4eeb51f92a70c0fd9de28d5edb0ddec3c08d30`; source `~/.cache/huggingface/hub/models--Alibaba-NLP--gte-modernbert-base/snapshots/e7f32e3c00f91d699e8c43b53106206bcc72bb22/tokenizer.json` (downloaded because no local copy was present). Truncation was disabled before encoding.

## Input snapshots

| Corpus | Root | Git HEAD | State | Semantic files | >4 MiB | Read/parse errors |
|---|---|---:|---|---:|---:|---:|
| aft | `~/Work/Projects/CortexKit/aft` | `967a9b2dd8bbfbdbf8d17c66742d2cec2a305952` | clean | 1519 | 0 | 0 |
| magic-context | `~/Work/Projects/CortexKit/magic-context` | `9dc6b4ae009b264a0f7ac2c1b2c0169dbcdfbdfe` | clean | 1508 | 0 | 0 |
| opencode | `~/Work/OSS/opencode` | `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252` | clean | 3781 | 0 | 0 |
| rails | `~/Work/OSS/rails` | `7d3b098615d01563bdab9909cbc2048ab5012e54` | clean | 3716 | 0 | 0 |
| kubernetes | `~/Work/OSS/kubernetes` | `c3af316a72bcbe06056e3827f81a922e0ce78f86` | clean | 24566 | 0 | 0 |
| elasticsearch | `~/Work/OSS/elasticsearch` | `6000a8b4fbdcf85cb2dfd6d9eac63d98a59e603c` | clean | 35005 | 0 | 0 |

Missing corpora: none.

## Uncapped token distribution

| Corpus | Rows | Total tokens | p50 | p90 | p99 | Max | >512 | >1024 | >2048 | >4096 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| aft | 31767 | 8270093 | 159 | 542 | 1467 | 15303 | 11.207% | 2.377% | 0.475% | 0.094% |
| magic-context | 21132 | 6201966 | 145 | 572 | 2077 | 37350 | 11.759% | 3.819% | 1.027% | 0.350% |
| opencode | 25590 | 8283502 | 105 | 387 | 3867 | 73519 | 7.167% | 3.411% | 1.673% | 0.895% |
| rails | 55774 | 17012349 | 126 | 400 | 3679 | 153106 | 7.943% | 4.246% | 2.024% | 0.864% |
| kubernetes | 221880 | 63070955 | 156 | 512 | 2211 | 97867 | 9.986% | 3.504% | 1.125% | 0.372% |
| elasticsearch | 590808 | 178791347 | 121 | 510 | 3098 | 162205 | 9.930% | 4.063% | 1.681% | 0.692% |
| **pooled** | 946951 | 281630212 | 131 | 506 | 2828 | 162205 | 9.835% | 3.863% | 1.515% | 0.605% |

## Rotation and candidate-ceiling cost

Tokens at a candidate ceiling are `sum(min(uncapped row tokens, ceiling))`; row count is unchanged.

| Corpus | Today's rows | Today's tokens | Uncapped tokens | At 1024 | At 2048 | At 4096 | At 8192 |
|---|---:|---:|---:|---:|---:|---:|---:|
| aft | 31767 | 4043500 | 8270093 | 7688624 | 8035149 | 8180723 | 8238654 |
| magic-context | 21132 | 2531005 | 6201966 | 5085506 | 5508463 | 5761607 | 5949073 |
| opencode | 25590 | 2613170 | 8283502 | 4649261 | 5252079 | 5879104 | 6479789 |
| rails | 55774 | 6368747 | 17012349 | 11118665 | 12817234 | 14321452 | 15549205 |
| kubernetes | 221880 | 29674238 | 63070955 | 52300048 | 56800452 | 59690236 | 61644450 |
| elasticsearch | 590808 | 67967653 | 178791347 | 129276051 | 144662763 | 157573803 | 167430765 |
| **pooled** | 946951 | 113198313 | 281630212 | 210118155 | 233076140 | 251406925 | 265291936 |

## Threshold shares by chunk kind

| Corpus | Kind | Rows | >512 | >1024 | >2048 | >4096 |
|---|---|---:|---:|---:|---:|---:|
| aft | symbol | 30725 | 11.587% | 2.457% | 0.491% | 0.098% |
| aft | file summary | 1042 | 0.000% | 0.000% | 0.000% | 0.000% |
| magic-context | symbol | 20113 | 12.355% | 4.012% | 1.079% | 0.368% |
| magic-context | file summary | 1019 | 0.000% | 0.000% | 0.000% | 0.000% |
| opencode | symbol | 23055 | 7.955% | 3.787% | 1.856% | 0.993% |
| opencode | file summary | 2535 | 0.000% | 0.000% | 0.000% | 0.000% |
| rails | symbol | 52266 | 8.476% | 4.531% | 2.160% | 0.922% |
| rails | file summary | 3508 | 0.000% | 0.000% | 0.000% | 0.000% |
| kubernetes | symbol | 203202 | 10.904% | 3.826% | 1.228% | 0.406% |
| kubernetes | file summary | 18678 | 0.000% | 0.000% | 0.000% | 0.000% |
| elasticsearch | symbol | 555827 | 10.555% | 4.319% | 1.786% | 0.735% |
| elasticsearch | file summary | 34981 | 0.000% | 0.000% | 0.000% | 0.000% |
| **pooled** | symbol | 885188 | 10.521% | 4.133% | 1.621% | 0.647% |
| **pooled** | file summary | 61763 | 0.000% | 0.000% | 0.000% | 0.000% |

## Threshold shares by language

| Corpus | Language | Rows | >512 | >1024 | >2048 | >4096 |
|---|---|---:|---:|---:|---:|---:|
| aft | Bash | 114 | 6.140% | 0.000% | 0.000% | 0.000% |
| aft | CUDA | 4 | 0.000% | 0.000% | 0.000% | 0.000% |
| aft | Go | 19 | 0.000% | 0.000% | 0.000% | 0.000% |
| aft | Groovy | 14 | 0.000% | 0.000% | 0.000% | 0.000% |
| aft | JavaScript | 24 | 8.333% | 8.333% | 4.167% | 0.000% |
| aft | Metal | 3 | 0.000% | 0.000% | 0.000% | 0.000% |
| aft | Pascal | 2 | 0.000% | 0.000% | 0.000% | 0.000% |
| aft | Python | 769 | 14.174% | 4.811% | 1.040% | 0.130% |
| aft | Rust | 21034 | 14.206% | 2.691% | 0.490% | 0.071% |
| aft | TOML | 610 | 0.164% | 0.164% | 0.000% | 0.000% |
| aft | TSX | 80 | 10.000% | 2.500% | 2.500% | 2.500% |
| aft | TypeScript | 9086 | 4.887% | 1.618% | 0.407% | 0.132% |
| aft | YAML | 8 | 12.500% | 0.000% | 0.000% | 0.000% |
| magic-context | Bash | 56 | 5.357% | 0.000% | 0.000% | 0.000% |
| magic-context | Python | 113 | 37.168% | 17.699% | 6.195% | 1.770% |
| magic-context | Rust | 5494 | 21.715% | 5.970% | 0.910% | 0.291% |
| magic-context | TOML | 169 | 0.592% | 0.000% | 0.000% | 0.000% |
| magic-context | TSX | 474 | 12.236% | 6.962% | 3.797% | 2.110% |
| magic-context | TypeScript | 14822 | 8.015% | 2.874% | 0.958% | 0.310% |
| magic-context | YAML | 4 | 0.000% | 0.000% | 0.000% | 0.000% |
| opencode | Bash | 2 | 0.000% | 0.000% | 0.000% | 0.000% |
| opencode | JavaScript | 15 | 0.000% | 0.000% | 0.000% | 0.000% |
| opencode | TOML | 22 | 9.091% | 0.000% | 0.000% | 0.000% |
| opencode | TSX | 5286 | 11.143% | 5.486% | 2.365% | 0.719% |
| opencode | TypeScript | 20030 | 6.151% | 2.896% | 1.513% | 0.954% |
| opencode | YAML | 235 | 4.681% | 1.277% | 0.000% | 0.000% |
| rails | JavaScript | 1376 | 4.070% | 0.799% | 0.509% | 0.000% |
| rails | Ruby | 53378 | 8.172% | 4.401% | 2.100% | 0.901% |
| rails | SCSS | 339 | 1.180% | 0.590% | 0.000% | 0.000% |
| rails | YAML | 681 | 1.175% | 0.881% | 0.147% | 0.147% |
| kubernetes | Bash | 1192 | 20.134% | 8.809% | 3.440% | 1.174% |
| kubernetes | C | 17 | 5.882% | 0.000% | 0.000% | 0.000% |
| kubernetes | Go | 207559 | 10.256% | 3.584% | 1.108% | 0.322% |
| kubernetes | Python | 37 | 13.514% | 0.000% | 0.000% | 0.000% |
| kubernetes | TOML | 10 | 0.000% | 0.000% | 0.000% | 0.000% |
| kubernetes | YAML | 13065 | 4.776% | 1.768% | 1.186% | 1.095% |
| elasticsearch | Bash | 51 | 3.922% | 0.000% | 0.000% | 0.000% |
| elasticsearch | C | 44 | 2.273% | 0.000% | 0.000% | 0.000% |
| elasticsearch | C++ | 411 | 18.491% | 8.029% | 1.703% | 0.000% |
| elasticsearch | Groovy | 1847 | 9.583% | 4.006% | 1.624% | 0.487% |
| elasticsearch | Java | 570580 | 9.826% | 4.075% | 1.707% | 0.708% |
| elasticsearch | JavaScript | 2 | 0.000% | 0.000% | 0.000% | 0.000% |
| elasticsearch | Python | 24 | 4.167% | 0.000% | 0.000% | 0.000% |
| elasticsearch | TOML | 2714 | 0.405% | 0.258% | 0.074% | 0.074% |
| elasticsearch | YAML | 15135 | 15.388% | 4.235% | 1.011% | 0.238% |
| **pooled** | Bash | 1415 | 17.809% | 7.420% | 2.898% | 0.989% |
| **pooled** | C | 61 | 3.279% | 0.000% | 0.000% | 0.000% |
| **pooled** | C++ | 411 | 18.491% | 8.029% | 1.703% | 0.000% |
| **pooled** | CUDA | 4 | 0.000% | 0.000% | 0.000% | 0.000% |
| **pooled** | Go | 207578 | 10.255% | 3.584% | 1.108% | 0.322% |
| **pooled** | Groovy | 1861 | 9.511% | 3.976% | 1.612% | 0.484% |
| **pooled** | Java | 570580 | 9.826% | 4.075% | 1.707% | 0.708% |
| **pooled** | JavaScript | 1417 | 4.093% | 0.917% | 0.565% | 0.000% |
| **pooled** | Metal | 3 | 0.000% | 0.000% | 0.000% | 0.000% |
| **pooled** | Pascal | 2 | 0.000% | 0.000% | 0.000% | 0.000% |
| **pooled** | Python | 943 | 16.649% | 6.045% | 1.591% | 0.318% |
| **pooled** | Ruby | 53378 | 8.172% | 4.401% | 2.100% | 0.901% |
| **pooled** | Rust | 26528 | 15.761% | 3.370% | 0.577% | 0.117% |
| **pooled** | SCSS | 339 | 1.180% | 0.590% | 0.000% | 0.000% |
| **pooled** | TOML | 3525 | 0.426% | 0.227% | 0.057% | 0.057% |
| **pooled** | TSX | 5840 | 11.216% | 5.565% | 2.483% | 0.856% |
| **pooled** | TypeScript | 43938 | 6.518% | 2.624% | 1.097% | 0.567% |
| **pooled** | YAML | 29128 | 10.207% | 3.025% | 1.061% | 0.618% |

## Today's body-line truncation

| Corpus | Symbol rows | Body exceeded 15 lines | Share cut |
|---|---:|---:|---:|
| aft | 30725 | 10955 | 35.655% |
| magic-context | 20113 | 6809 | 33.854% |
| opencode | 23055 | 5226 | 22.668% |
| rails | 52266 | 10944 | 20.939% |
| kubernetes | 203202 | 59966 | 29.511% |
| elasticsearch | 555827 | 141070 | 25.380% |
| **pooled** | 885188 | 234970 | 26.545% |

## Decision input

The smallest rotated ModernBERT ceiling covering at least 99% of rows in each corpus is: aft needs 2048, magic-context needs 4096, opencode needs 4096, rails needs 4096, kubernetes needs 4096, elasticsearch needs 4096. For the three fleet corpora, a full re-embed at each corpus's selected ceiling costs aft: 31767 rows / 8035149 tokens at 2048; magic-context: 21132 rows / 5761607 tokens at 4096; opencode: 25590 rows / 5879104 tokens at 4096.
