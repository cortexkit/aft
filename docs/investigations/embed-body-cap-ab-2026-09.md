# Embed-text body-cap A/B (2026-09)

## Decision

Do **not** change the default from the current 300-character / 15-line body cap.
On the production Bionic/Qwen lane, longer symbol bodies cost substantially more index-build time and embedding input without producing a consistent retrieval gain:

- 1,000-character bodies changed concept MRR@10 from 0.311550 to 0.309793 and real-query MRR@10 from 0.181589 to 0.180879.
- 2,500-character bodies changed concept MRR@10 to 0.311154 and real-query MRR@10 to 0.182817.
- Exact recall and hit@1 were unchanged in every arm.
- The 1,000-character arm used 55.0% more estimated embedding tokens and 18.5% more aggregate build wall time. The 2,500-character arm used 89.7% more tokens and 32.0% more build time.

The small mixed rank movements do not justify the cost. The implementation therefore leaves `semantic.max_input_tokens` absent by default; any default flip remains a separate decision and commit.

## Production-lane precondition

The chunk census established that the current Bionic server accepts at least a ~4,200-token row and an eight-row batch of ~700-token rows. The A/B therefore ran against the production lane rather than stopping at the former 512-token llama.cpp limit:

- endpoint: `http://localhost:1234/v1/embeddings`
- backend: `openai_compatible`
- model: `text-embedding-qwen3-embedding-0.6b`
- returned dimension: 1,024
- execution: standalone release `aft`, never the daemon
- storage: a distinct temporary storage directory for every arm/corpus pair

The endpoint returned a `usage` object, but both `prompt_tokens` and `total_tokens` were zero. Token totals below are consequently labeled estimates and use `embed_text characters / 3.5`.

## Arms and cap derivation

The control omitted `semantic.max_input_tokens`, preserving the exact existing caps: signature 400 characters, body 15 lines / 300 characters, total 1,600 characters. Remote arms set a whole-row token budget. Their total cap is `floor(tokens × 3.5)` and their body cap is the remainder after the 400-character signature cap and measured header reserve.

A census over the same six corpora used by the September chunk census measured the maximum `name/file/kind/name` header at **457 characters**. The maximum was:

`elasticsearch:x-pack/plugin/inference/src/yamlRestTest/resources/rest-api-spec/test/inference/47_semantic_text_knn.yml::knn query against incompatible dense_vector and semantic_text fields using query vectors returns the matching semantic vectors and failures for incompatible dims`

| Arm | `max_input_tokens` | Signature chars | Body lines | Effective body chars | Total chars |
|---|---:|---:|---:|---:|---:|
| 300 control | absent | 400 | 15 | 300 | 1,600 |
| ~1,000 | 531 | 400 | unbounded | 1,001 | 1,858 |
| ~2,500 | 960 | 400 | unbounded | 2,503 | 3,360 |

The one- and three-character overages are integer rounding from `ceil((requested body + 400 + 457) / 3.5)`. File-summary chunk construction is independent of these symbol-body caps and remained unchanged.

## Retrieval and cost results

Each arm used the identical committed query inputs and top-k policy: 28 concept fixtures at top 10, 16 exact-recall fixtures at their committed sentence top 5 / pair top 10, and 43 B0 real-query rows with the committed paged profile and top 100 requests. No fixture or scorer was edited.

| Arm | Concept MRR@10 | Concept hit@1 | Exact MRR@10 | Exact hit@1 | Sentence rank@1 | Pair recall@10 | Real MRR@10 | Real hit@1 | Build wall | Est. tokens | Rows | `semantic.bin` total | Query p50 | Query p95 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 300 control | 0.311550 | 0.214286 | 1.000000 | 1.000000 | 1.000000 | 1.000000 | 0.181589 | 0.116279 | 1,659.645 s | 8,888,806 est. | 81,168 | 368.93 MiB | 219.203 ms | 1,365.147 ms |
| ~1,000 | 0.309793 | 0.214286 | 1.000000 | 1.000000 | 1.000000 | 1.000000 | 0.180879 | 0.116279 | 1,967.501 s | 13,778,166 est. | 81,168 | 385.25 MiB | 217.508 ms | 1,339.301 ms |
| ~2,500 | 0.311154 | 0.214286 | 1.000000 | 1.000000 | 1.000000 | 1.000000 | 0.182817 | 0.116279 | 2,190.311 s | 16,865,566 est. | 81,168 | 395.56 MiB | 215.796 ms | 1,268.790 ms |

Build wall time and index totals sum the six independently built corpus indexes in an arm: the AFT concept corpus, four pinned exact-recall repositories, and the B0 real-query corpus. The equal row count confirms that cap selection changed row text, not corpus coverage. `semantic.bin` growth is modest relative to token growth because every row retains the same 1,024-dimensional vector.

The aggregate query latency covers every request in all three families. As expected, longer indexed rows did not make query embedding slower; the modest p95 decrease is normal server/run variation, not a claimed improvement. Family latency details were:

| Arm | Concept p50 / p95 | Exact p50 / p95 | Real-query p50 / p95 |
|---|---:|---:|---:|
| 300 control | 1,501.199 / 3,274.732 ms | 220.131 / 16,125.675 ms | 218.019 / 737.702 ms |
| ~1,000 | 1,695.819 / 2,144.507 ms | 212.467 / 13,948.586 ms | 217.025 / 642.136 ms |
| ~2,500 | 1,590.216 / 1,839.748 ms | 218.546 / 15,620.645 ms | 214.783 / 638.372 ms |

## Largest 300 → 1,000 per-query movements

Only four rows improved and five worsened; there were fewer than 20 non-zero movements in either direction. A miss is shown as `—` and is ranked one position after the MRR@10 scoring window when computing the displayed direction.

### Improved

| Family | Query | Expected file(s) | Rank 300 | Rank 1,000 | Change |
|---|---|---|---:|---:|---:|
| concept | how are imports organized after edits | `crates/aft/src/commands/organize_imports.rs`; `crates/aft/src/imports/mod.rs` | 9 | 7 | +2 |
| concept | how does semantic indexing persist across configure runs | `crates/aft/src/commands/configure.rs`; `crates/aft/src/semantic_index.rs` | 8 | 7 | +1 |
| concept | process group already terminated | `crates/aft/src/bash_background/registry.rs`; `crates/aft/src/bash_background/watchdog.rs` | 9 | 8 | +1 |
| real query | status snapshot session id tracked_files checkpoints json serialization | `crates/aft/src/commands/status.rs` | 4 | 3 | +1 |

The improvements are all explanatory or multi-token queries where additional implementation prose can add matching context. None reached hit@1.

### Worsened

| Family | Query | Expected file(s) | Rank 300 | Rank 1,000 | Change |
|---|---|---|---:|---:|---:|
| concept | bash long running reminder config | `crates/aft/src/commands/configure.rs`; `packages/pi-plugin/src/config.ts`; `crates/aft/src/config.rs` | 7 | 9 | -2 |
| concept | how are configure warnings delivered asynchronously | `crates/aft/src/commands/configure.rs`; `packages/aft-bridge/src/bridge.ts` | 5 | 7 | -2 |
| concept | where does lsp diagnostic polling wait for server responses | `crates/aft/src/lsp/manager.rs`; `crates/aft/src/context.rs` | 6 | 7 | -1 |
| real query | stale diagnostics mark_file_diagnostics_stale | `crates/aft/src/context.rs` | 8 | 9 | -1 |
| real query | todos category JobOutcome Pending did not complete | `crates/aft/tests/integration/inspect_engine_test.rs` | 10 | — | -1 |

Longer bodies also add broadly related implementation terms to competing rows. The symmetric small losses and gains explain why aggregate quality is effectively flat.

## Reproduction

```bash
cargo build --release -p agent-file-tools
python3 benchmarks/aft-search/provision_corpus.py
cd benchmarks/aft-search
python3 run_embed_body_cap_ab.py \
  --binary ../../target/release/aft \
  --base-url http://localhost:1234/v1 \
  --model text-embedding-qwen3-embedding-0.6b \
  --output .bench/embed-body-cap-ab/results.json
```

The runner records fixture hashes, per-query ranks/results, per-corpus fingerprints and index costs, and the top movements in its ignored JSON artifact. It parses each persisted V7 `semantic.bin` to count rows and the exact embedded-text character total. The production endpoint's unusable zero-valued usage counters are recorded alongside the estimate label. The complete measured artifact had SHA-256 `bc5ba2d27a23ecf13c72ba8c5ee5e5b879adc9f896e72f4c822c86709c7eb91b`.
