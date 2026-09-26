# aft_search recall audit (2026-09)

Measurement only. No ranking, routing or confidence code changed. The gate's
reference (`real-query-baseline.json`, `manifest.sha256`), the vector pack and
the slice descriptors are untouched.

An Athena panel reviewed every known `aft_search` problem and agreed that each
proposed ranking change (the router fix, query decomposition, confidence
changes, structural indexes, rerank) depends on measurements nobody had: where
a known answer is lost, whether routing more queries to semantic search would
break exact recall, and how often a `high` confidence label is wrong. This
report supplies those measurements.

## What was built

1. **Recall audit instrumentation** (`crates/aft/src/commands/semantic_search/recall_audit.rs`).
   Setting `AFT_SEARCH_RECALL_AUDIT=1` in the aft process environment adds a
   `recall_audit` object to every engine-ranked search reply. It records each
   lane's complete candidate order (exact, lexical, path lookup, and every
   semantic chunk before the one-row-per-file collapse), the whole ranked list
   with each entry's lane positions and admission disposition, and the
   candidate limits in force. When `AFT_SEARCH_RECALL_AUDIT_TARGETS` names a
   JSON array of project-relative paths, it also reports, per file: its
   trigram-index state (`indexed`, `unindexed`, `absent`), its lexical rank
   without the lane's discovery pool, and every one of its semantic chunks
   ranked against the whole store with no enumeration limit. On routes that
   run no semantic lane, the audit embeds the query once to report where
   semantic search *would* have ranked the targets.

   It is an environment switch, not a tool parameter: agents cannot request
   it and the shared daemon never sets it. It reads the ranking after the page
   and confidence label are computed.

2. **Runner** (`benchmarks/aft-search/run_recall_audit.py`) that replays three
   case sets with the audit on, one standalone binary and temporary storage
   directory per corpus (never the shared daemon), and classifies each known
   answer by the last stage it reached. Offline cases:
   `test_run_recall_audit.py`.

3. **Named report-only cases** (`benchmarks/aft-search/named-case-fixtures.json`,
   11 rows), each stating its expected answer or its expected "no answer" and
   how that was verified. The file is in `.aftignore`; the gate's evidence
   tree (`30d4a64f`) has no file of that name, so the gate's index is
   unchanged.

4. **Exact-recall replay with semantic search on**
   (`run_exact_recall.py --semantic`). Same fixtures, same invariants
   (sentence at rank 1; all content tokens in one file within the top 10),
   with the live local model. Report-only: exits 0 unless the run fails,
   because the checked-in baseline was recorded with semantic search off.

### The audit does not change results

The 43 real-query rows were replayed on one release build twice, audit off and
audit on, against the gate's pinned tree and vector pack, and every reply was
compared: result rows (file, lines, score, exact flag), the rendered text, and
the plan (shape, lanes, confidence, exact tier, depth). Eight such pairs were
run while the comparison was refined. The last three, run after it excluded
the two fields below, were identical in every compared field for all 43 rows.
The differences the earlier pairs surfaced, which the final comparison
excludes, are:

- The status bar line (`[AFT E? W? | ...]`) is appended to whichever reply
  follows a change in the health counters, a timing effect unrelated to the
  audit; which rows carried it changed from run to run.
- `embedding_calls`, `embedding_cache_hits` and `live_embed_calls` differ in
  the 9 rows routed without a semantic lane, because the audit embeds the
  query there to report semantic counterfactual ranks. That is the one
  observable side effect, and only when the switch is on.

## Method

- Binary: `aft 0.57.2`, release build of this branch.
- `real-query`: the 43 included rows of `real-query-manifest.json` on the
  gate's pinned evidence tree (`30d4a64f`, digest verified), run twice:
  - `pack`: the gate's own vector pack and fixture embedding server, so ranks
    match the gate (7617 at 3 and 7956 at 5, as in the last re-record). The
    pack holds hash-derived stand-in vectors, so semantic ranks in this mode
    say nothing about semantic relevance.
  - `local`: the live local model (fastembed all-MiniLM-L6-v2, 384-d), the
    model agents use. 2,591 lexical files, 27,476 semantic entries.
- `prefrontal`: the 4 rows of `prefrontal-search-fixtures.json` on prefrontal
  `bf1e86a8`, provisioned from the local checkout into
  `benchmarks/aft-search/.bench/`, live local model. 3,024 lexical files,
  31,191 semantic entries (the same counts as the prefrontal baseline).
- `named`: the 11 new rows, 7 on the AFT evidence tree and 4 on prefrontal,
  live local model.
- Every query: public `search` tool through `tool_call`, `topK` 50, one page,
  the row's recorded `includeTests`.
- "Page" is that 50-row page. The gate scores the paged profile to depth 400,
  so a row here can be `ranked_below_page` and still score in the gate.
- Real-query answers are file-level (the file the episode opened). Prefrontal
  and named answers are line ranges, so they are `found` only when a page row
  overlaps the answer's lines; the file-level rank is also recorded.
- The table shows the answer that got furthest when a row has several; the
  JSON record has every answer.

### Stages

| Stage | Meaning |
|---|---|
| `never_indexed` | Neither the trigram index nor the semantic store holds the file. |
| `not_produced` | Indexed, but no lane that ran scores the file at any depth. |
| `not_admitted` | A lane that ran scores it, but only beyond a candidate limit: the ranked list's block depth (200 positions per lane at a 50-row page), the lexical lane's discovery pool (files holding one of the three rarest query trigrams), or the semantic lane's 100-chunk enumeration limit. |
| `ranked_below_page` | In the ranked list, below the page. |
| `dropped_by_dedupe` | Its file is on the page through another chunk. The semantic lane did produce the answer's own chunk; the one-row-per-file collapse kept a different one. |
| `file_shown_other_span` | Its file is on the page through another span, and no lane produced the answer's own span as a unit. |
| `found` | On the page, at the answer's lines where the answer has lines. |

### Table columns

- **Exact**: position in the exact/symbol candidate list.
- **Lexical**: position in the lexical lane's order. `- (unpooled N)` means
  the lane never produced the file because it lacks all three rarest query
  trigrams, and it would score at N without that pool.
- **Semantic (store)**: best chunk of the answer ranked against the whole
  semantic store (overlapping the answer's lines for line-ranged answers).
  At most 100 is inside the semantic lane. `not run (N)` means the route ran
  no semantic lane and N is the counterfactual. `-` means the store holds no
  chunk for it (Markdown, lockfiles and other non-code files are not
  chunked).
- **Ranked list**: position of the file in the full ranked list.
- **Page**: page rank (line-level for line-ranged answers; `- (file N)` means
  only the file is on the page).
- **Confidence**: the reply's `plan.confidence`.
- **Top-5**: the answer is in the top 5 (line-level for line-ranged answers).

## 1. Where answers are lost

Summary by case set:

| Case set | Rows | found | ranked_below_page | not_admitted | dropped_by_dedupe | file_shown_other_span | not_produced / never_indexed | no answer expected |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| real-query, pack vectors | 43 | 27 | 8 | 8 | - | - | 0 | - |
| real-query, local model | 43 | 24 | 10 | 9 | - | - | 0 | - |
| prefrontal, local model | 4 | 1 | 0 | 0 | 1 | 2 | 0 (row level) | - |
| named, local model | 11 | 4 | 1 | 0 | 1 | 1 | 0 | 4 |

Observations the tables support:

- **No real-query answer is lost to indexing.** Every opened file is in the
  trigram index and is produced by at least one lane at some depth. The 16
  (pack) or 19 (local) lost rows split between the ranked list (below the
  page) and candidate limits.
- **Block depth is a real admission limit.** Four rows (584, 4112, 8676,
  19696) have the answer in the lexical lane at positions 221-289, past the
  200-position block depth used for a 50-row page, so it never enters the
  ranked list. The gate's paged profile reaches deeper blocks for these.
- **The lexical discovery pool cuts answers.** 8637, 19269, 10820 (both
  backends), 18413 (pack) and 9034, 9372 (local) lack all three rarest query
  trigrams; the lexical lane would score them at 55-1158 without the pool. 19269's answer
  (`STRUCTURE.md`) would be at lexical 55 and has no semantic chunk at all.
- **Below-page answers are the rerank candidates.** Eight to ten rows have
  the answer in the ranked list at 58-356. Those are the only rows a reranker
  over the list could move; the `not_admitted` rows are not in any rerank
  pool.
- **Line-level localization is its own loss.** Of the 11 line-ranged
  answerable prefrontal and named rows, 5 put the right file on the page but
  at another span (`dropped_by_dedupe` 2, `file_shown_other_span` 3). The dispatch arms
  are the clearest: the `ask.persist_answer` arms rank 1,445 (main.rs) and
  2,704 (dispatch.rs) among semantic chunks, so no lane produces them as a
  unit even though their files are on the page at 6 and 11. The
  `mark_auto_proceeded` arm ranks 2,383. This is a chunking and localization
  problem, not a candidate-recall one.
- **The prefrontal board-nudge answer is in a file the trigram index does not
  hold.** `manager_runtime.rs` is 1,171,103 bytes, over the 1 MiB index
  limit, so the exact and lexical lanes cannot see it (`not_produced`,
  trigram state `unindexed`). The query routes `code_literal`, so the
  semantic lane does not run either. Semantic search would have ranked
  `board_staleness_nudge_text` first and `run_board_staleness_pass` 16th
  among all 31,191 chunks.
- **Semantic counterfactuals for code_literal routes.** Where the route ran no
  semantic lane and the answer was lost, the local-model counterfactual rank
  is: `named-punct-size-limit` 7 (the answer is otherwise at list position
  87), `prefrontal-board-nudge` 1 and 16 (above), 8637 3,163, 18338 4,371.
  Among rows already found, 7956 would be semantic rank 1 and 11468 rank 5
  (page ranks 5 and 13). The two punctuated-prose rows are routed
  `code_literal` purely because of the comma or parenthesis.

### real-query (pack vectors, gate-faithful)

| Case | Class | Expected | Shape | Exact | Lexical | Semantic (store) | Ranked list | Page | Lost at | Confidence | Top-5 |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| `followup-census:832` | not_a_search_failure | `packages/aft-bridge/src/pool.ts` | short | - | 167 | 100 | 112 | - | ranked_below_page | low | no |
| `followup-census:3184` | not_a_search_failure | `crates/aft/src/hashline/snapshot/mod.rs` | natural_language | 47 | 117 | 162 | 43 | 43 | found | low | no |
| `followup-census:4111` | topk_cut | `README.md` | short | - | 149 | - | 152 | - | ranked_below_page | low | no |
| `followup-census:4112` | topk_cut | `README.md` | short | - | 221 | - | - | - | not_admitted | low | no |
| `followup-census:4212` | other | `crates/aft/src/commands/edit_match.rs` | short | - | 10 | 3256 | 11 | 11 | found | low | no |
| `followup-census:7617` | phrase_present_not_surfaced | `crates/aft/src/commands/configure.rs` | code_literal | 4 | 435 | not run (39) | 3 | 3 | found | low | yes |
| `followup-census:7656` | topk_cut | `crates/aft/src/commands/status.rs` | natural_language | 5 | 12 | 2416 | 5 | 5 | found | low | yes |
| `followup-census:7670` | wrong_lane_nl | `crates/aft/src/inspect/manager.rs` | natural_language | 1 | 1 | 222 | 1 | 1 | found | high | yes |
| `followup-census:7695` | not_a_search_failure | `crates/aft/src/main.rs` | natural_language | 3 | 5 | 312 | 3 | 3 | found | high | yes |
| `followup-census:7744` | wrong_lane_nl | `packages/pi-plugin/src/tools/bash.ts` | natural_language | 23 | 65 | 15 | 18 | 18 | found | high | no |
| `followup-census:7956` | index_stale_or_missing | `crates/aft/src/cli/profile.rs` | log_excerpt | - | 6 | not run (170) | 5 | 5 | found | low | yes |
| `followup-census:8637` | not_a_search_failure | `crates/aft/tests/integration/callgraph_test.rs` | code_literal | - | - (unpooled 138) | not run (55) | - | - | not_admitted | high | no |
| `followup-census:8676` | not_a_search_failure | `crates/aft/src/subc_format.rs` | short | - | 289 | 225 | - | - | not_admitted | low | no |
| `followup-census:8679` | phrase_present_not_surfaced | `crates/aft/src/subc_format.rs` | short | - | 19 | 19 | 1 | 1 | found | high | yes |
| `followup-census:8886` | topk_cut | `packages/opencode-plugin/test/load-matrix/load-matrix.ts` | short | 1 | 454 | 793 | 1 | 1 | found | high | yes |
| `followup-census:9034` | index_stale_or_missing | `crates/aft/tests/helpers/mod.rs` | natural_language | - | - (unpooled 156) | 57 | 242 | - | ranked_below_page | high | no |
| `followup-census:9173` | scope_mismatch | `packages/opencode-plugin/src/__tests__/bg-notifications.test.ts` | natural_language | 223 | - (unpooled 295) | 1709 | 207 | - | ranked_below_page | low | no |
| `followup-census:9365` | wrong_lane_nl | `crates/aft/src/subc/mod.rs` | natural_language | 2 | 5 | 36 | 2 | 2 | found | low | yes |
| `followup-census:9372` | not_a_search_failure | `crates/aft/src/effective_path.rs` | natural_language | - | - (unpooled 216) | 18 | 86 | - | ranked_below_page | high | no |
| `followup-census:10215` | not_a_search_failure | `packages/aft-bridge/src/index.ts` | code_literal | 3 | 6 | not run (17135) | 3 | 3 | found | high | yes |
| `followup-census:10672` | other | `packages/opencode-plugin/src/index.ts` | code_literal | 2 | 25 | not run (4006) | 2 | 2 | found | high | yes |
| `followup-census:10820` | topk_cut | `packages/aft-cli/src/index.ts` | natural_language | - | - (unpooled 1158) | 7722 | - | - | not_admitted | low | no |
| `followup-census:11468` | index_stale_or_missing | `crates/aft/src/commands/configure.rs` | code_literal | - | 13 | not run (166) | 13 | 13 | found | low | no |
| `followup-census:12976` | wrong_lane_nl | `STRUCTURE.md` | natural_language | - | 6 | - | 42 | 42 | found | low | no |
| `followup-census:13398` | index_stale_or_missing | `crates/aft/src/gh_shim.rs` | short | - | 54 | 81 | 37 | 37 | found | low | no |
| `followup-census:13619` | not_a_search_failure | `crates/aft/src/context.rs` | short | 1 | 16 | 16 | 1 | 1 | found | low | yes |
| `followup-census:14337` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 131 | 8 | 12 | 12 | found | high | no |
| `followup-census:14369` | index_stale_or_missing | `crates/aft/tests/integration/bash_background_persistence_test.rs` | natural_language | 370 | 223 | 504 | 356 | - | ranked_below_page | high | no |
| `followup-census:14449` | index_stale_or_missing | `STRUCTURE.md` | short | - | 103 | - | 111 | - | ranked_below_page | low | no |
| `followup-census:14613` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 75 | 6 | 2 | 2 | found | high | yes |
| `followup-census:14964` | index_stale_or_missing | `crates/aft/src/context.rs` | short | - | 22 | 35 | 12 | 12 | found | low | no |
| `followup-census:15174` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 75 | 6 | 2 | 2 | found | high | yes |
| `followup-census:16765` | wrong_lane_nl | `crates/aft/src/search_index.rs` | natural_language | - | 110 | 32 | 17 | 17 | found | low | no |
| `followup-census:17173` | not_a_search_failure | `crates/aft/src/context.rs` | short | - | 91 | 1 | 36 | 36 | found | high | no |
| `followup-census:17208` | index_stale_or_missing | `crates/aft/src/commands/configure.rs` | natural_language | 3 | 7 | 57 | 3 | 3 | found | high | yes |
| `followup-census:17364` | wrong_lane_nl | `crates/aft/tests/integration/inspect_engine_test.rs` | natural_language | 2 | 1 | 1395 | 2 | 2 | found | high | yes |
| `followup-census:18091` | other | `crates/aft/src/backup.rs` | natural_language | - | 11 | 257 | 39 | 39 | found | low | no |
| `followup-census:18338` | other | `crates/aft/tests/integration/inspect_tier2_scheduler_test.rs` | code_literal | - | 61 | not run (2035) | 67 | - | ranked_below_page | high | no |
| `followup-census:18341` | scope_mismatch | `crates/aft/tests/integration/inspect_tier2_reuse_test.rs` | identifier | - | 21 | not run (400) | 22 | 22 | found | high | no |
| `followup-census:18413` | other | `crates/aft/src/config.rs` | short | - | - (unpooled 151) | 407 | - | - | not_admitted | high | no |
| `followup-census:19269` | wrong_lane_nl | `STRUCTURE.md` | natural_language | - | - (unpooled 55) | - | - | - | not_admitted | low | no |
| `followup-census:19696` | not_a_search_failure | `Cargo.lock` | identifier | - | 245 | not run (-) | - | - | not_admitted | low | no |

### real-query (live local model)

| Case | Class | Expected | Shape | Exact | Lexical | Semantic (store) | Ranked list | Page | Lost at | Confidence | Top-5 |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| `followup-census:832` | not_a_search_failure | `packages/aft-bridge/src/pool.ts` | short | - | 167 | 1518 | 170 | - | ranked_below_page | low | no |
| `followup-census:3184` | not_a_search_failure | `crates/aft/src/hashline/snapshot/mod.rs` | natural_language | 47 | 117 | 851 | 43 | 43 | found | low | no |
| `followup-census:4111` | topk_cut | `README.md` | short | - | 149 | - | 150 | - | ranked_below_page | low | no |
| `followup-census:4112` | topk_cut | `README.md` | short | - | 221 | - | - | - | not_admitted | low | no |
| `followup-census:4212` | other | `crates/aft/src/commands/edit_match.rs` | short | - | 10 | 1771 | 13 | 13 | found | low | no |
| `followup-census:7617` | phrase_present_not_surfaced | `crates/aft/src/commands/configure.rs` | code_literal | 4 | 435 | not run (137) | 3 | 3 | found | low | yes |
| `followup-census:7656` | topk_cut | `crates/aft/src/commands/status.rs` | natural_language | 5 | 12 | 10 | 5 | 5 | found | low | yes |
| `followup-census:7670` | wrong_lane_nl | `crates/aft/src/inspect/manager.rs` | natural_language | 1 | 1 | 2 | 1 | 1 | found | high | yes |
| `followup-census:7695` | not_a_search_failure | `crates/aft/src/main.rs` | natural_language | 3 | 5 | 16 | 3 | 3 | found | high | yes |
| `followup-census:7744` | wrong_lane_nl | `packages/pi-plugin/src/tools/bash.ts` | natural_language | 23 | 65 | 3944 | 18 | 18 | found | high | no |
| `followup-census:7956` | index_stale_or_missing | `crates/aft/src/cli/profile.rs` | log_excerpt | - | 6 | not run (1) | 5 | 5 | found | low | yes |
| `followup-census:8637` | not_a_search_failure | `crates/aft/tests/integration/callgraph_test.rs` | code_literal | - | - (unpooled 138) | not run (3163) | - | - | not_admitted | high | no |
| `followup-census:8676` | not_a_search_failure | `crates/aft/src/subc_format.rs` | short | - | 289 | 938 | - | - | not_admitted | low | no |
| `followup-census:8679` | phrase_present_not_surfaced | `crates/aft/src/subc_format.rs` | short | - | 19 | 4 | 1 | 1 | found | high | yes |
| `followup-census:8886` | topk_cut | `packages/opencode-plugin/test/load-matrix/load-matrix.ts` | short | 1 | 454 | 198 | 1 | 1 | found | high | yes |
| `followup-census:9034` | index_stale_or_missing | `crates/aft/tests/helpers/mod.rs` | natural_language | - | - (unpooled 156) | 6787 | - | - | not_admitted | high | no |
| `followup-census:9173` | scope_mismatch | `packages/opencode-plugin/src/__tests__/bg-notifications.test.ts` | natural_language | 223 | - (unpooled 295) | 625 | 207 | - | ranked_below_page | low | no |
| `followup-census:9365` | wrong_lane_nl | `crates/aft/src/subc/mod.rs` | natural_language | 2 | 5 | 15 | 2 | 2 | found | low | yes |
| `followup-census:9372` | not_a_search_failure | `crates/aft/src/effective_path.rs` | natural_language | - | - (unpooled 216) | 352 | - | - | not_admitted | high | no |
| `followup-census:10215` | not_a_search_failure | `packages/aft-bridge/src/index.ts` | code_literal | 3 | 6 | not run (5655) | 3 | 3 | found | high | yes |
| `followup-census:10672` | other | `packages/opencode-plugin/src/index.ts` | code_literal | 2 | 25 | not run (577) | 2 | 2 | found | high | yes |
| `followup-census:10820` | topk_cut | `packages/aft-cli/src/index.ts` | natural_language | - | - (unpooled 1158) | 21545 | - | - | not_admitted | low | no |
| `followup-census:11468` | index_stale_or_missing | `crates/aft/src/commands/configure.rs` | code_literal | - | 13 | not run (5) | 13 | 13 | found | low | no |
| `followup-census:12976` | wrong_lane_nl | `STRUCTURE.md` | natural_language | - | 6 | - | 27 | 27 | found | low | no |
| `followup-census:13398` | index_stale_or_missing | `crates/aft/src/gh_shim.rs` | short | - | 54 | 571 | 58 | - | ranked_below_page | low | no |
| `followup-census:13619` | not_a_search_failure | `crates/aft/src/context.rs` | short | 1 | 16 | 13 | 1 | 1 | found | low | yes |
| `followup-census:14337` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 131 | 103 | 158 | - | ranked_below_page | high | no |
| `followup-census:14369` | index_stale_or_missing | `crates/aft/tests/integration/bash_background_persistence_test.rs` | natural_language | 370 | 223 | 3575 | 356 | - | ranked_below_page | high | no |
| `followup-census:14449` | index_stale_or_missing | `STRUCTURE.md` | short | - | 103 | - | 107 | - | ranked_below_page | low | no |
| `followup-census:14613` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 75 | 2 | 3 | 3 | found | high | yes |
| `followup-census:14964` | index_stale_or_missing | `crates/aft/src/context.rs` | short | - | 22 | 1 | 8 | 8 | found | low | no |
| `followup-census:15174` | index_stale_or_missing | `crates/aft/src/context.rs` | natural_language | - | 75 | 2 | 3 | 3 | found | high | yes |
| `followup-census:16765` | wrong_lane_nl | `crates/aft/src/search_index.rs` | natural_language | - | 110 | 20 | 18 | 18 | found | low | no |
| `followup-census:17173` | not_a_search_failure | `crates/aft/src/context.rs` | short | - | 91 | 75 | 58 | - | ranked_below_page | high | no |
| `followup-census:17208` | index_stale_or_missing | `crates/aft/src/commands/configure.rs` | natural_language | 3 | 7 | 33 | 3 | 3 | found | high | yes |
| `followup-census:17364` | wrong_lane_nl | `crates/aft/tests/integration/inspect_engine_test.rs` | natural_language | 2 | 1 | 683 | 2 | 2 | found | high | yes |
| `followup-census:18091` | other | `crates/aft/src/backup.rs` | natural_language | - | 11 | 1 | 2 | 2 | found | low | yes |
| `followup-census:18338` | other | `crates/aft/tests/integration/inspect_tier2_scheduler_test.rs` | code_literal | - | 61 | not run (4371) | 67 | - | ranked_below_page | high | no |
| `followup-census:18341` | scope_mismatch | `crates/aft/tests/integration/inspect_tier2_reuse_test.rs` | identifier | - | 21 | not run (1129) | 22 | 22 | found | high | no |
| `followup-census:18413` | other | `crates/aft/src/config.rs` | short | - | - (unpooled 151) | 83 | 144 | - | ranked_below_page | high | no |
| `followup-census:19269` | wrong_lane_nl | `STRUCTURE.md` | natural_language | - | - (unpooled 55) | - | - | - | not_admitted | low | no |
| `followup-census:19696` | not_a_search_failure | `Cargo.lock` | identifier | - | 245 | not run (-) | - | - | not_admitted | low | no |

### prefrontal (live local model)

| Case | Class | Expected | Shape | Exact | Lexical | Semantic (store) | Ranked list | Page | Lost at | Confidence | Top-5 |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| `prefrontal-consult-roster-for-class` | unclassified | `crates/prefrontal-core-module/src/consult/runtime.rs:7621-7647` | natural_language | 15 | 25 | 43 | 13 | - (file 13) | dropped_by_dedupe | low | no |
| `prefrontal-ask-persist-answer-dispatch` | op_string_dispatch_missed | `crates/prefrontal-core-module/src/main.rs:2117-2145` | short | - | 21 | 1445 | 6 | - (file 6) | file_shown_other_span | low | no |
| `prefrontal-spec-aggregator-repo-tools` | unclassified | `script/experiments/merge-tools-replay/tools.ts:129-171` | natural_language | 15 | 1 | 15 | 14 | 14 | found | high | no |

Per answer, from the JSON record:

- `prefrontal-board-nudge-lane-cap`: rooms.rs 71-74 and 3295-3299 are on the
  page only as the file (rank 32, another span). `run_board_staleness_pass`
  and `board_staleness_nudge_text` are `not_produced` (file over the index
  size limit; semantic not run; semantic counterfactual 16 and 1).
- `prefrontal-consult-roster-for-class`: `consult_roster_for_class` is
  semantic chunk 43 and the neighbouring `consult_roster_with_authoritative_width`
  chunk 48, both inside the lane, but the file's row (page rank 13) shows
  the file's best-scoring chunk (semantic rank 19) instead.
- `prefrontal-ask-persist-answer-dispatch`: see the dispatch observation
  above; the op registration line (dispatch.rs 11463) is chunk 88, also
  collapsed into the file's other row.
- `prefrontal-spec-aggregator-repo-tools`: `repoTool` at page 14,
  `repoTools` at 12, below other `[exact]` rows; the first five are unrelated
  files matched on the `read_file` fragment. The
  replay report under `.cortexkit/alfonso/reports/` is `never_indexed`
  (absent from both stores; prefrontal's `.gitignore` excludes it).

### named (live local model)

| Case | Class | Expected | Shape | Exact | Lexical | Semantic (store) | Ranked list | Page | Lost at | Confidence | Top-5 |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| `named-punct-exact-tier-order` | punctuated_prose | `crates/aft/src/commands/semantic_search/comparator.rs:94-174` | code_literal | 1 | 3 | not run (242) | 1 | - (file 1) | file_shown_other_span | high | no |
| `named-mixed-exact-phrase` | mixed_prose_exact_fragment | `crates/aft/src/commands/semantic_search/exact_lane.rs:50-71` | natural_language | - | 2 | 1 | 1 | 1 | found | low | yes |
| `named-dispatch-semantic-search` | op_string_dispatch | `crates/aft/src/commands/semantic_search/mod.rs:316-462` | short | - | 91 | 1 | 35 | 35 | found | low | no |
| `named-partial-anchor-include-tests` | partial_anchor_outranks_answer | `crates/aft/src/semantic_index.rs:3496-3591` | natural_language | - | 99 | 31 | 33 | 33 | found | low | no |
| `named-no-answer-payment-webhook` | validated_no_answer | no answer | natural_language | - | - | - | - | - | no_answer_expected | low | no |
| `named-no-answer-oauth-refresh` | validated_no_answer | no answer | natural_language | - | - | - | - | - | no_answer_expected | low | no |
| `named-mixed-board-lane-cap` | mixed_prose_exact_fragment | `crates/prefrontal-core-store/src/rooms.rs:3295-3299` | natural_language | 1 | 4 | 20 | 1 | - (file 1) | dropped_by_dedupe | low | no |
| `named-dispatch-mark-auto-proceeded` | op_string_dispatch | `crates/prefrontal-core-store/src/ask.rs:1750-1760` | short | - | 7 | 1 | 2 | 2 | found | low | yes |
| `named-no-answer-shopping-cart` | validated_no_answer | no answer | natural_language | - | - | - | - | - | no_answer_expected | low | no |
| `named-no-answer-dark-mode` | validated_no_answer | no answer | natural_language | - | - | - | - | - | no_answer_expected | low | no |

The named rows and their verification are in `named-case-fixtures.json`:

| Row | Corpus | Class | Query | Expected answer | Verified by |
|---|---|---|---|---|---|
| `named-punct-size-limit` | AFT evidence tree | punctuated_prose | when a file is larger than the size limit, does the search index skip it (and where is that check)? | `search_index.rs` `prepare_search_path` 2930-2957, the size check at 1436-1444, `DEFAULT_MAX_FILE_SIZE` 36 | reading the file at the pin |
| `named-punct-exact-tier-order` | AFT evidence tree | punctuated_prose | how are exact tier results ranked, and why | `comparator.rs` `compare_fields_1_to_6` 94-174, `r3_cmp` 217-282 | reading; the query is the unquoted form from the router asymmetry in the misroute investigation |
| `named-mixed-exact-phrase` | AFT evidence tree | mixed_prose_exact_fragment | exact_phrase: how the verbatim phrase is normalized before the exact pass compares it | `exact_lane.rs` 50-71 | reading |
| `named-mixed-board-lane-cap` | prefrontal | mixed_prose_exact_fragment | BOARD_LANE_CAP: what happens when a board.lanes update asks for more lanes than the cap | `rooms.rs` refusal 3295-3299, constant 71-74 | reading |
| `named-dispatch-semantic-search` | AFT evidence tree | op_string_dispatch | semantic_search command handler | `main.rs` arm 973-979, `handle_semantic_search` 316-462 | reading the command match |
| `named-dispatch-mark-auto-proceeded` | prefrontal | op_string_dispatch | ask.mark_auto_proceeded handler | `dispatch.rs` arm 12602-12610, registration 11469, store write `ask.rs` 1750-1760 | reading; no function carries the dotted name |
| `named-partial-anchor-include-tests` | AFT evidence tree | partial_anchor_outranks_answer | where is include_tests applied before top-k selection in the semantic lane | `semantic_index.rs` `search_filtered` 3496-3591, predicate `mod.rs` 1269-1271 | reading; `include_tests` occurs in hundreds of files |
| `named-no-answer-payment-webhook` | AFT evidence tree | validated_no_answer | stripe payment webhook signature verification | none | whole-word grep for each word and phrase; the only 'signature verification' hits are a manifest signature in `gh_shim.rs`, read and confirmed unrelated |
| `named-no-answer-oauth-refresh` | AFT evidence tree | validated_no_answer | OAuth2 refresh token rotation for user login sessions | none | grep: oauth2, 'refresh token', 'token rotation', 'login session' absent; `refresh_token` only in a pytest output fixture |
| `named-no-answer-shopping-cart` | prefrontal | validated_no_answer | shopping cart checkout with a discount coupon code | none | grep: shopping, cart, coupon absent; checkout and discount hits read and confirmed unrelated (git and connection checkouts, review prose, an API-price comment) |
| `named-no-answer-dark-mode` | prefrontal | validated_no_answer | dark mode theme toggle in the settings page | none | grep: 'dark theme', 'theme toggle', 'color scheme' absent; the one 'dark mode' hit is 'go-dark mode' (an agent snooze) |

What the named rows show today:

- Both punctuated-prose rows route `code_literal` and run no semantic lane.
  `named-punct-size-limit`'s answer is at list position 87 while the semantic
  counterfactual ranks its `prepare_search_path` chunk 7th of 27,476.
  `named-punct-exact-tier-order` puts `comparator.rs` first through an E2
  window at line 82, just above the answer's lines, with confidence `high`.
- `named-mixed-exact-phrase` is found at rank 1. `named-mixed-board-lane-cap`
  puts `rooms.rs` first through an exact row at line 3449; the refusal it asks
  about is semantic chunk 20 and is collapsed into that row.
- `named-dispatch-semantic-search` finds `handle_semantic_search` at page 35
  (semantic chunk 1, but the fused list puts TypeScript files and benchmark
  report Markdown first); the `main.rs` arm is semantic chunk 1,327. `named-dispatch-mark-auto-proceeded`
  finds the store write at 2 and the registration line at 4, and the arm
  itself is chunk 2,383.
- `named-partial-anchor-include-tests`: the five top rows are `[exact]` rows
  in files that mention `include_tests`; `search_filtered` is at 33.
- All four no-answer rows report `low`.

## 2. Exact recall with semantic search enabled

`run_exact_recall.py --semantic`, same 16 fixtures and invariants as the
gate, semantic search on with the live local model (fastify 1,870, flask
1,951, ripgrep 3,502, turborepo 14,662 semantic entries). Record:
`benchmarks/aft-search/results/exact-recall-semantic-2026-09.json`.

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

Sentence rank-1 **1.000**, pair recall@10 **1.000** (semantic-off baseline:
1.000 and 1.000). Every expected file is at rank 1 with the `[exact]` marker.

Which rows the semantic lane actually ran on matters more than the totals:

| Family | Router shape | Semantic lane ran | Rows | Rank 1 |
|---|---|---|---:|---:|
| pair | short | yes | 8 | 8 |
| sentence | natural_language | yes | 4 | 4 |
| sentence | code_literal | no | 4 | 4 |

- For the 12 rows that already route to a semantic plan, adding the live
  semantic lane does not displace the exact answer: all 12 stay at rank 1.
- The 4 sentences that contain a comma or parenthesis (the ones the
  code-literal misroute investigation lists) still route `code_literal`, so
  this mode runs them without a semantic lane and their result equals the
  semantic-off gate. These are exactly the rows a router fix would move to a
  semantic plan, so today's replay does not certify that fix; it certifies
  the 12 rows that are already there. After a router change, rerunning
  `run_exact_recall.py --semantic` is the measurement for those 4, and the
  rows' `plan.lanes_run` field shows whether semantic search ran.

## 3. Confidence calibration

`high` is the reply's `plan.confidence`. "Top-5 hit" is the answer in the top
5 (file-level for real-query rows, line-level for line-ranged rows, with the
file-level figure in brackets).

| Case set | high: rows | high: top-5 hit | low: rows | low: top-5 hit |
|---|---:|---:|---:|---:|
| real-query, pack vectors | 20 | 10 (50%) | 23 | 5 (22%) |
| real-query, local model | 20 | 10 (50%) | 23 | 6 (26%) |
| prefrontal, local model | 1 | 0 [0] | 3 | 0 [0] |
| named with an answer, local model | 1 | 0 [1] | 6 | 2 [4] |
| named, validated no answer | 0 | - | 4 | - |

- Half of the real-query rows labelled `high` do not have the opened file in
  the top 5, on both backends and on the same ten rows: 7744, 8637, 9034,
  9372, 14337, 14369, 17173, 18338, 18341, 18413. Three of them (8637, 9034,
  9372) are not in the ranked list at all under the local model.
- `prefrontal-spec-aggregator-repo-tools` is `high` with its answer at 14,
  below `[exact]` rows on unrelated files.
- `named-punct-exact-tier-order` is `high` with the right file at rank 1 but
  the row pointing just above the answer's lines.
- None of the four validated no-answer rows is `high`. Four rows are too few
  to call that a property; they establish a baseline for the class.

The per-row labels and top-5 flags are in the tables above and in
`results/recall-audit-2026-09.json` (`rows[].plan.confidence`,
`rows[].answer_in_top5`, `rows[].file_in_top5`; `summary` holds the counts).

## Caveats

- Semantic ranks in the `pack` run come from hash-derived stand-in vectors.
  Use the `local` run for anything about semantic relevance.
- "Page" is one 50-row page; the gate's paged profile reads deeper.
- Line-level stages are strict: `named-punct-exact-tier-order` counts as
  `file_shown_other_span` although its row is 12 lines above the answer.
- The audit covers engine-ranked replies only. Regex and grep routes carry
  none; no row here took one.
- Each local-model group ran once. The pack-vector replay was run in the
  eight audit-off/audit-on pairs above.

## Reproduce

From the repository root, after `cargo build --release -p agent-file-tools`:

```bash
python3 benchmarks/aft-search/provision_corpus.py
python3 benchmarks/aft-search/provision_evidence.py
python3 benchmarks/aft-search/provision_corpus.py --corpus benchmarks/aft-search/corpus/prefrontal.toml
cd benchmarks/aft-search
python3 run_recall_audit.py --real-query-backend both \
  --out results/recall-audit-2026-09.json --markdown .bench/recall-audit/tables.md
python3 run_exact_recall.py --semantic --ready-timeout 5400 \
  --out results/exact-recall-semantic-2026-09.json
```

prefrontal is fetched from `git@github.com:cortexkit/prefrontal.git`. Without
access to it, fetch the pinned commit from a local checkout into
`benchmarks/aft-search/.bench/repos-prefrontal/prefrontal` (with `origin` set
to that URL) before running `provision_corpus.py`, which then only records it.
