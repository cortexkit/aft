# Paging block-freeze investigation (2026-09)

## Decision

This is outcome **(a), a product admission bug**, but the controlled experiment does not reproduce a request-page-size bug. The four `topK:100` pages and eight `topK:50` pages are byte-identical when both shapes run through the supplied oracle binary. The 14-row benchmark drift is instead the result of coupling the semantic lane's fixed candidate-enumeration bound to the newly reduced public `topK` ceiling. The old reference used a binary that admitted 100 semantic candidates; the capped head admitted 50. That changes fusion before the page is cut, including ranks 1-10.

The public cap remains 50. The engine fix is to restore a page-size-independent semantic enumeration constant of 100 while leaving request validation and the public schema unchanged.

## Controlled reproduction

I imported `load_inputs`, `materialized_bundle`, `fixture_endpoint`, `NdjsonClient`, and `_normalize_results` from `benchmarks/aft-search/run_real_query.py`. One materialized bundle, one loopback embedding fixture server, one storage directory, and one AFT process served both shapes. The binary was `/tmp/card-wt/target/release/aft`, reporting `aft 0.56.1`; its SHA-256 was `d47bf2c868c983ce03183b4e7f8c1f3769d1e8eb2681994b7c81f13edae82356` and its source identity is `651311547c3b`.

For every response I concatenated the pre-collapse result records in request order. Each record contained the normalized path, public score fields, source, exact/hybrid flags, `structuredContent.results[].lane_positions`, evidence descriptor, and ranked tuple. Request-only annotations (`page_offset`, `page_top_k`, and page-local/global rank) were excluded from the byte comparison because they identify the cut rather than the result. All requests for a row reported one snapshot generation. Both shapes visited retrieval depths 200 and 400.

### Same-binary 4x100 versus 8x50

Ranks are one-based. `—` means no differing record exists. Because the ordered raw payloads are identical, the path sets and order are also identical. There is no divergence inside the first 100 or at ranks 100-400.

| row | first differing rank | 4x100 page | 8x50 page | path-set relation | raw ordered stream |
| --- | ---: | --- | --- | --- | --- |
| followup-census:4112 | — | — | — | identical | identical, 400 records |
| followup-census:4212 | — | — | — | identical | identical, 400 records |
| followup-census:7670 | — | — | — | identical | identical, 99 records |
| followup-census:7695 | — | — | — | identical | identical, 301 records |
| followup-census:7744 | — | — | — | identical | identical, 400 records |
| followup-census:8679 | — | — | — | identical | identical, 240 records |
| followup-census:9173 | — | — | — | identical | identical, 302 records |
| followup-census:9372 | — | — | — | identical | identical, 130 records |
| followup-census:13398 | — | — | — | identical | identical, 400 records |
| followup-census:13619 | — | — | — | identical | identical, 235 records |
| followup-census:14964 | — | — | — | identical | identical, 203 records |
| followup-census:17364 | — | — | — | identical | identical, 203 records |
| followup-census:18091 | — | — | — | identical | identical, 239 records |
| followup-census:19269 | — | — | — | identical | identical, 168 records |

The oracle's eight-page collapsed `ranked_paths` also match the checked-in four-page reference for all 14 rows. Thus the claim that these two shapes differed on the same bytes is false; either the capped binary was used despite `AFT_BINARY_PATH`, or outputs from the pre-cap and capped binaries were compared.

## Reproducing the 14 changed rows

A release build of the unmodified capped head (`93c369a36c1c`) had SHA-256 `bd2721862c35228b4c4ebdc6f273f6bd7d592503e9ad086929df9dc71677d04d`. Its eight `topK:50` pages reproduce all 14 reported changes when compared with the oracle/reference stream. Every first difference is in page zero and ranks 1-10, not at depth 100 or later, and every complete path set differs. The values in parentheses are `score; source; lane positions`.

| row | first rank | oracle 4x100 page/result | capped-head 8x50 page/result | sets |
| --- | ---: | --- | --- | --- |
| 4112 | 10 | `0+100` `commands/inspect.rs` (1.641364; lexical; lex 22, sem 48) | `0+50` `M004-SUMMARY.md` (1.675422; lexical; lex 9) | differ |
| 4212 | 1 | `0+100` `run_tool_call.rs` (3.993785; lexical; lex 3, sem 68) | `0+50` `commands/apply_patch.rs` (4.121042; lexical; lex 0) | differ |
| 7670 | 5 | `0+100` `inspect/scanners/dead_code.rs` (3.623484; lexical; lex 9, sem 50) | `0+50` `inspect/manager.rs` (3.930663; lexical; lex 0) | differ |
| 7695 | 9 | `0+100` `lsp/client.rs` (4.665798; lexical; lex 1, sem 49) | `0+50` `opencode-plugin/src/config.ts` (3.705462; lexical; lex 109, sem 4) | differ |
| 7744 | 5 | `0+100` `opencode-plugin/src/tools/reading.ts` (4.448266; lexical; lex 5, sem 53) | `0+50` `pi-plugin/src/tools/bash.ts` (4.090433; lexical; lex 64, sem 13) | differ |
| 8679 | 7 | `0+100` `main.rs` (2.134618; lexical; lex 12, sem 65) | `0+50` `STRUCTURE.md` (2.289889; lexical; lex 3) | differ |
| 9173 | 6 | `0+100` `format.rs` (3.653145; lexical; lex 4, sem 60) | `0+50` `inspect/manager.rs` (2.922800; lexical; lex 123, sem 8) | differ |
| 9372 | 8 | `0+100` `callgraph_store/mod.rs` (3.963685; lexical; lex 12, sem 41) | `0+50` `breaker-state-machine-audit.md` (4.752658; lexical; lex 0) | differ |
| 13398 | 5 | `0+100` `aft-bridge/src/bridge.ts` (1.667811; lexical; lex 12, sem 80) | `0+50` `bash-task-erasure-census-2026-08-30.md` (1.748213; lexical; lex 4) | differ |
| 13619 | 4 | `0+100` `subc/manifest.rs` (3.973273; lexical; lex 4, sem 59) | `0+50` `cache_freshness.rs` (4.184057; lexical; lex 0) | differ |
| 14964 | 5 | `0+100` `lsp/client.rs` (2.544981; lexical; lex 5, sem 60) | `0+50` `commands/lsp_inspect.rs` (2.719259; lexical; lex 1) | differ |
| 17364 | 10 | `0+100` `tests/integration/subc_bridge_test.rs` (2.280768; lexical; lex 51, sem 70) | `0+50` `tests/integration/inspect_engine_test.rs` (3.168156; lexical; lex 0) | differ |
| 18091 | 9 | `0+100` `opencode-plugin/src/notifications.ts` (1.739076; lexical; lex 9, sem 72) | `0+50` `executor/mod.rs` (1.370692; lexical; lex 116, sem 19) | differ |
| 19269 | 7 | `0+100` `config.rs` (1.951737; lexical; lex 13, sem 53) | `0+50` `lsp/client.rs` (1.802695; lexical; lex 35, sem 32) | differ |

Paths above are shortened only for readability; the comparison used complete normalized paths. The rank movement on 4212, 7744, and 17364 accounts for the reported metric changes.

## Source mechanism

The cap commit changed one constant binding, not the block builder:

1. `crates/aft/src/subc_translate.rs:26-34` defines `SEARCH_MAX_TOP_K = 50` as the public agent request limit. `translate_search` validates `topK` against that limit at `subc_translate.rs:2056-2069` and independently copies `offset` at `subc_translate.rs:2070-2072`. Offset translation neither multiplies by nor otherwise depends on `topK`.
2. `crates/aft/src/commands/semantic_search/mod.rs:129` binds the engine's private `MAX_TOP_K` to that public limit. This is the erroneous coupling.
3. The semantic lane uses that value as its enumeration/admission cap. The external path requests `MAX_TOP_K + 1`, tests availability, and truncates at `MAX_TOP_K` at `mod.rs:1475-1497`. The normal path does the same at `mod.rs:3075-3134`. Lowering the public page ceiling therefore removed semantic candidates 51-100 from every request, even deep-offset requests. Fusion then lost their semantic contributions and reordered the head.
4. The block code itself is page-size independent. `blocks.rs:397-403` selects the initial 200/400/... depth tier from `offset + topK`; `blocks.rs:415-473` reconstructs every prior frozen block before slicing; `blocks.rs:475-504` admits each scored lane by that tier depth; and `blocks.rs:506-601` freezes each identity once. The controlled streams demonstrate those rules hold.
5. The identifier-only fallback at `mod.rs:2456-2488` selects a 200/400/... exact bound from `offset + topK`, but both tested shapes cross the same tier boundaries and 13 of the 14 rows are not identifier-shaped. It cannot explain this family.
6. `exact_lane.rs:279-307` only uses its `offset`/`top_k` pair to serve a memoized verified set. The engine caller at `mod.rs:2326-2341` deliberately requests `0, usize::MAX`, so public page size does not bound exact admission.
7. `paging.rs:184-233` and `trailer.rs:59-91` select stop/total metadata after the block reply exists. They cannot change candidate admission or fusion.

This also explains both invariance observations. After the cap, sizes 10, 25, and 50 all admitted the same 50 semantic candidates, so they agreed over 100. Before the cap, sizes 10, 25, 50, and 100 all admitted the same 100 candidates, so the old checks agreed. Comparing an old 100-admission reference with a new 50-admission run made the change look correlated with page size even though controlled 50 and 100 requests on the oracle agree through depth 400.

## Required fix and verification

Keep `SEARCH_MAX_TOP_K = 50` for public validation, but introduce a distinct fixed semantic enumeration limit of 100 for both normal and external semantic-index searches. A regression must concatenate one synthetic corpus to depth 400 at page sizes 10, 25, 50, and a test-only 100, compare serialized stability streams byte-for-byte, and prove that a semantic candidate beyond public rank 50 remains admitted. The 100-page path belongs only to the engine test; no public schema or translator limit changes.

After the fix, the full 43-row eight-page head replay must be compared with the old four-page reference. `ranked_paths`, metrics, and the common portion of `page_zero_ranked_paths` should be unchanged. A size-50 page-zero list is expected to be the first 50 entries of the old size-100 page-zero list, not byte-equal in length.
