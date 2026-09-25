# Baseline: prefrontal aft_search misses as benchmark rows (2026-09-25)

Rows: `benchmarks/aft-search/prefrontal-search-fixtures.json`, 4 scored rows and 1 unsupported specimen.
Corpus: `benchmarks/aft-search/corpus/prefrontal.toml`, pinning prefrontal at `bf1e86a885e26698afba39c810e52e66fc0992e6` (origin/main).
Runner: `benchmarks/aft-search/run_prefrontal_search.py`, which calls the public `search` tool (topK 50, one page, `includeTests: false`) with the live local model (fastembed all-MiniLM-L6-v2, 384-d) on a fresh index: 3,024 lexical files and 31,191 semantic entries.
Record: `benchmarks/aft-search/results/prefrontal-search-baseline.json`, measured with `aft 0.57.2` (binary sha256 `b8b25e18…fa60`, built from `79ca1f0ee`). Three runs returned the same ranked lists.

"Line rank" is the first result whose file matches a known answer and whose line span overlaps its range. "File rank" is the first result in a known-answer file, at any line.

## Per row

| Row | Class | Line rank | File rank | Plan confidence |
|---|---|---|---|---|
| board nudge + lane cap | generated_or_census_outranks_source | absent in top 50 | 33 (`rooms.rs`, file summary at line 4135) | low |
| consult roster per class | unclassified | absent in top 50 | 14 (`consult/runtime.rs`, `normalized_panel_class`) | low |
| `ask.persist_answer handler` | op_string_dispatch_missed | absent in top 50 | 7 (`main.rs`, `already_open_ask_reply`) | low |
| spec aggregator repo tools | unclassified | 13 (`config.ts` `repoTools`); `tools.ts` `repoTool` at 15 | 13 | **high** |

### 1. "board staleness nudge wake text: … and lane cap 8"
The specimen's failure reproduces exactly. The top 4 are `assets/alfonso.schema.json`, `packages/core/src/features/alfonso-core/module-op-kinds.generated.json`, `docs/campaign/census/S16-derived-scheduler-and-residual-completion.json`, and `docs/specs/board.md`. Ranks 5-32 are docs, plans, reports, another `*.generated.json` (`contracts.generated.json`, rank 16), census scripts and unrelated source. All 50 are lexical file-summary rows. The plan ran only the `exact` and `lexical` lanes, with shape `code_literal` and no embedding call, so the semantic lane never saw the query. `run_board_staleness_pass` and `board_staleness_nudge_text` (manager_runtime.rs) are absent from the top 50. The cap constant is `BOARD_LANE_CAP` at rooms.rs 71-74; it is 20 at the pin, and the comment above it still says "8-lane cap".

### 2. "athena roster per class lookup design_review key panel roster config"
Above the answer file: `campaign/scaffolding.rs` `validate_athena_request_params` (1), a `gather_evidence/runtime.rs` summary (2), `athena-v2.ts` `ATHENA_MEMBER_CLASS_KEYS` (3), `alfonso.schema.json` (4), a store method (5), then consult, gather, scaffolding and spec **tests**, two docs, and consult matrix/types summaries (6-13). This is close to the specimen's list. The answer file first appears at 14, but through the neighbouring helper `normalized_panel_class`, followed by `validate_roster_by_class` in spec/runtime.rs at 15. `consult_roster_for_class` itself is absent. Observed pattern: semantic hits on neighbouring validators, schema key lists and tests rank above the function that indexes the roster. The specimen file names no class for this row.

### 3. "ask.persist_answer handler"
Above the answer: a push-reply test (1), `consumer.ts` `persistAnswer` (2), a browser-relay test (3), `script/probe-clarify-answer.ts` (4), the store's `persist_answer` method (5), and a doc summary (6). That is the specimen's flat head. `main.rs` enters at 7 and `dispatch.rs` at 11, both on the wrong function (`already_open_ask_reply`, and the test `answer_ask`). None of the known arms reaches the top 50: the `"persist_answer" =>` arm (dispatch.rs 12510-12556), the `envelope.method == "ask.persist_answer"` arms (main.rs 2117-2145, 2943-2977), and the op registration (dispatch.rs 11463). `scripts/harness/fire-and-forget-census.json` is at 12.

### 4. "spec aggregator merge repository read tools search_repo read_file"
The specimen called this a no-answer query. At the pinned commit that is wrong: the merge-tools replay experiment (committed 2026-09-24) implements these tools. The row is scored against `tools.ts` `repoTool` (129-171, relevance 2), `config.ts` 73-95 and the replay report (relevance 1). Above the answer are twelve `[exact]` rows on the bare `read_file` token. These are `config_overrides.rs` (1), `scripts/harness/extraction-inventory.json` (2, on `readFileSync`), `artifacts_v2.rs` (3), eight TS/script file summaries (4-11; `merge-tools-replay/summarize.ts` at 8) and `scaffolding.rs` (12). The semantic hits `repoTools` (13) and `repoTool` (15) follow. The report under `.cortexkit/alfonso/reports/` is absent. That path matches prefrontal's `.gitignore` (`.cortexkit/alfonso/*`), so it is probably not indexed. The plan reports confidence **high** for this ranking.

### Not scored
- Specimen 5 (`aft_zoom attachments_runtime_session_display_read`, a fn inside `macro_rules!`) is a zoom lookup, not a search query. No runner in this directory measures zoom or outline, so it is listed under `unsupported` in the fixtures and has no row.
- **No-answer class: no row.** None of the four search specimens lacks an answer at the pinned commit. No runner here supports a no-answer row either: `run_external.py` rejects an empty ground truth, and a real-query row scores exactly one opened file.

## Does the same failure exist in the AFT gate?
I checked `real-query-baseline.json` against `real-query-manifest.json` (43 included rows) for each class.

- **Generated or census JSON outranking source: yes, in 9 rows.** Clearest is `followup-census:7617` (`"was not found on PATH"`): `benchmarks/aft-search/engine-fixtures/exact/census_episodes.json` is at rank 1, and the opened `crates/aft/src/commands/configure.rs` is at 4. Also:
  - `followup-census:7956`: codegraph results JSON at 2; opened `cli/profile.rs` at 6.
  - `followup-census:7744`: `docs/v0.49-agent-prefix-capture.json` and `docs/v0.49-unified-tool-surface-inventory.json` at 2-3; opened file absent.
  - `followup-census:14369`: `benchmarks/swe-bench-multilingual.json`, `docs/v0.49-agent-prefix-capture.json` and `benchmarks/swe-bench-django-10.json` at 2-4; absent.
  - `followup-census:18413`: `assets/aft.schema.json` at 1; absent. The query names that file, so this row is weaker evidence.
  - Weaker still, JSON at rank 6-10 above an absent answer: `followup-census:8676`, `9173`, `14337`, `18338`.

  A fix for this class can show its movement in the gate: 7617 and 7956 have the source file ranked below the JSON.
- **Op-string dispatch missed: no gate row.** No included row asks for the handler of an operation string whose opened file is the dispatch arm. The nearest, `followup-census:14369` (`"command": "configure" …`) and `8676` (`pub fn handle_configure`), opened a test and `subc_format.rs`, not a dispatch arm. A fix for this class has no gate row that measures it.
- **No answer but confident hits: no gate row.** Every gate row has an opened file, and no prefrontal row has this class either.
- **Unclassified rows 2 and 4:** I did not look for gate matches for these, since they have no named class. Row 4's pattern (single-token `[exact]` summaries above semantic function hits, reported with high confidence) is worth naming as its own class.

## Gate unchanged
The only gate input this change touches is the answer-key ignore list: `benchmarks/aft-search/.aftignore` gains `prefrontal-search-fixtures.json`. The gate's evidence tree (`30d4a64f`) has no `benchmarks/aft-search/` directory and no file by that name, so the entry matches nothing there. I checked this by replaying the gate on this branch: provision → exact recall → concept recall → `run_real_query.py --profile paged`, with the release build above. All **43/43 rows are byte-equal** (canonical JSON) to `real-query-baseline.json`. The families are equal (real_query MRR@10 0.188760, hit@1 0.093023, hit@5 0.325581), and exact recall is 1.000/1.000 against its 1.000 baseline. The manifest, reference, sidecar and vector pack were not touched and nothing was re-recorded. No vectors were authored: the prefrontal runner uses the live local model, not a pack.
