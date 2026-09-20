# aft_search → grep sweep, 2026-09-15 through 2026-09-17

Fixed snapshot: OpenCode `part.rowid <= 8,646,101` (cutoff 2026-09-17T11:00:24.375000Z); Pi records through the same cutoff. Raw evidence is gitignored under `.alfonso/data/search-sweep-2d/`.

## Mechanism ranking (first)

Every discriminating episode was hand-labelled. Shares use the 56-episode discriminating denominator; `stale_index` is separated from `index_building`, and the two new exclusions are shown rather than folded into `not_a_failure`.

| Rank | Mechanism | Episodes | Share of discriminating | True-failure numerator? |
|---:|---|---:|---:|---|
| 1 | `not_a_failure` | 25 | 44.64% | no |
| 2 | `topk_cut` | 10 | 17.86% | yes |
| 3 | `wrong_lane` | 6 | 10.71% | yes |
| 4 | `stale_index` | 3 | 5.36% | yes |
| 5 | `identifier_not_definition_first` | 2 | 3.57% | yes |
| 6 | `index_building` | 2 | 3.57% | yes |
| 7 | `nonproject_root` | 2 | 3.57% | no |
| 8 | `path_filter_missing` | 2 | 3.57% | yes |
| 9 | `token_variant` | 2 | 3.57% | yes |
| 10 | `phrase_present_not_surfaced` | 1 | 1.79% | yes |
| 11 | `scope_mismatch` | 1 | 1.79% | yes |

The actionable ranking is therefore `topk_cut` (10), `wrong_lane` (6), `stale_index` (3), then four tied two-episode mechanisms (`identifier_not_definition_first`, `index_building`, `path_filter_missing`, `token_variant`).

## Rates and coverage

- **All aft_search calls:** 29/1407 = **2.06%** true failures.
- **Discriminating episodes:** 29/56 = **51.79%** true failures.
- **Qualifying aft_search→grep episodes:** 29/447 = **6.49%** true failures; this is the denominator comparable to the old 19.8% projection.
- Qualifying episodes/searches: 447/1407 = 31.77%; discriminating/qualifying: 56/447 = **12.53%**.
- Search refinements inside the next-three-call window: 743/1407 = 52.81%.

A true failure is any discriminating label except `not_a_failure` and `nonproject_root`. As in pass 2, `scope_mismatch` remains in the numerator; `path_filter_missing` also remains because the query expressed a project-relative directory restriction that search did not apply.

## Counts by harness

| Harness | Searches | Qualifying | Discriminating | True failures | Failures/search |
|---|---:|---:|---:|---:|---:|
| `OpenCode` | 1343 | 403 | 51 | 25 | 1.86% |
| `Pi` | 64 | 44 | 5 | 4 | 6.25% |

## Counts by model

| Model | Searches | Qualifying | Discriminating | True failures | Failures/search |
|---|---:|---:|---:|---:|---:|
| `openai/gpt-5.6-sol` | 728 | 118 | 11 | 5 | 0.69% |
| `openai/gpt-6-astra` | 172 | 88 | 13 | 3 | 1.74% |
| `google/antigravity-gemini-3.8-flash` | 150 | 75 | 6 | 4 | 2.67% |
| `xai/grok-4.6` | 97 | 30 | 1 | 0 | 0.00% |
| `anthropic/claude-opus-5` | 88 | 57 | 12 | 8 | 9.09% |
| `openai/gpt-5.6-luna` | 62 | 2 | 0 | 0 | 0.00% |
| `google-antigravity/antigravity-gemini-3.8-flash` | 41 | 25 | 3 | 2 | 4.88% |
| `anthropic/claude-fable-5-1` | 24 | 19 | 4 | 3 | 12.50% |
| `openai-codex/gpt-5.6-sol` | 23 | 19 | 2 | 2 | 8.70% |
| `ollama-cloud/deepseek-v4-flash:0731` | 15 | 13 | 4 | 2 | 13.33% |
| `ollama-cloud/glm-5.2` | 7 | 1 | 0 | 0 | 0.00% |

## Counts by project

| Project | Searches | Qualifying | Discriminating | True failures | Failures/search |
|---|---:|---:|---:|---:|---:|
| `prefrontal` | 365 | 156 | 18 | 9 | 2.47% |
| `aft` | 305 | 67 | 12 | 5 | 1.64% |
| `magic-context` | 269 | 44 | 7 | 5 | 1.86% |
| `synapse` | 160 | 50 | 2 | 1 | 0.62% |
| `thalamus` | 100 | 34 | 6 | 3 | 3.00% |
| `plexus` | 66 | 18 | 1 | 0 | 0.00% |
| `anthropic-auth` | 64 | 44 | 5 | 4 | 6.25% |
| `broca` | 37 | 20 | 4 | 2 | 5.41% |
| `cerebellum` | 22 | 5 | 0 | 0 | 0.00% |
| `openai-auth` | 14 | 4 | 0 | 0 | 0.00% |
| `fusiform` | 3 | 3 | 0 | 0 | 0.00% |
| `alfonso-ios` | 1 | 1 | 1 | 0 | 0.00% |
| `engram` | 1 | 1 | 0 | 0 | 0.00% |

## Same-query current-engine rerun

Reruns used the exact query text and historical `includeTests` value on the exact historical root. The live ready AFT actor opened each target root's shared artifact through the borrowed read-only path; it did not build target indexes. Eleven short-lived worktree roots no longer existed and were not replaced with canonical checkouts. One initial smoke call against the canonical broca root mistakenly entered a building fallback before this borrowed path was established; no reported rank comes from that call.

Of 29 true failures, 18 roots were probed and 11 were unavailable. The grep-found file is now in the top 10 for **6/18** probed episodes and in the top 50 for **8/18**; it is still absent from the top 50 for **10/18**. Per-file ranks and complete raw responses are recorded in `episodes.jsonl` and `current-probes/`.

| Mechanism | Failed episodes | Probed | Root unavailable | Now top 10 | Now top 50 |
|---|---:|---:|---:|---:|---:|
| `topk_cut` | 10 | 7 | 3 | 3 | 5 |
| `wrong_lane` | 6 | 3 | 3 | 0 | 0 |
| `stale_index` | 3 | 3 | 0 | 3 | 3 |
| `identifier_not_definition_first` | 2 | 1 | 1 | 0 | 0 |
| `index_building` | 2 | 1 | 1 | 0 | 0 |
| `path_filter_missing` | 2 | 1 | 1 | 0 | 0 |
| `token_variant` | 2 | 0 | 2 | 0 | 0 |
| `phrase_present_not_surfaced` | 1 | 1 | 0 | 0 | 0 |
| `scope_mismatch` | 1 | 1 | 0 | 0 | 0 |

## Comparison with the pre-engine census

The current qualifying-episode failure rate is **6.49%** versus **19.8%** pre-engine. The discriminating share is **12.53%** versus **32.6%** in the recent pre-engine window. On the newly requested all-search denominator, this sweep is **2.06%**; the closest reconstructable old value is 3,996/90,363 = **4.42%**, not 19.8%.

These are descriptive deltas, not a clean release experiment. This remained a CI-heavy week: many Alfonso worktrees, many exact identifiers, fewer exploratory NL searches, and 11 historical roots gone before rerun. The 56-row hand-labelled denominator is also much smaller than the old 6,469 discriminating episodes.

| Mechanism | Current D=56 | Pre-engine D=6,469 |
|---|---:|---:|
| `not_a_failure` | 25 (44.64%) | 2,473 (38.23%) |
| `wrong_lane` | 6 (10.71%) | 789 (12.19%) |
| stale/building combined | 5 (8.93%) | 719 (11.11%) |
| `topk_cut` | 10 (17.86%) | 552 (8.54%) |
| `phrase_present_not_surfaced` | 1 (1.79%) | 545 (8.43%) |
| `token_variant` | 2 (3.57%) | 532 (8.22%) |
| `scope_mismatch` | 1 (1.79%) | 475 (7.34%) |
| `other` | 0 (0.00%) | 284 (4.38%) |
| `identifier_not_definition_first` | 2 (3.57%) | 101 (1.56%) |
| `path_filter_missing` (new) | 2 (3.57%) | — |
| `nonproject_root` (new) | 2 (3.57%) | — |

The clearest post-engine shift in this small sample is a larger `topk_cut` share and smaller phrase/token shares; current reruns recover eight files by rank 50, including three episodes labelled `stale_index` because the file moved inside its historical allowance.

## Verbatim fixtures by mechanism

Up to three episodes are shown per mechanism. When a mechanism has fewer than three episodes, every available fixture is shown rather than fabricating examples. Queries and paths below are copied verbatim from the retained episode records.

### `not_a_failure` (25 episode(s))
1. Query: `is_subc_native_plumbing_tool`
   - Search returned: `crates/aft/src/subc/manifest.rs`; `crates/aft/src/subc/mod.rs`; `benchmarks/aft-search/bundles/aft-evidence-30d4a64f.zip`
   - Grep found: `packages/opencode-plugin/src/index.ts`
   - Opened/edited: `packages/opencode-plugin/src/index.ts`
2. Query: `subc_management_surface_test`
   - Search returned: `crates/aft/tests/integration/profile_memory_test.rs`; `crates/aft/src/fleet_status.rs`; `crates/aft/tests/integration/subc_plumbing_drift_test.rs`; …
   - Grep found: `crates/aft/tests/integration/subc_bridge_test.rs`
   - Opened/edited: `crates/aft/tests/integration/subc_bridge_test.rs`
3. Query: `subc_management_surface`
   - Search returned: `crates/aft/tests/integration/profile_memory_test.rs`; `docs/health-digest.md`; `crates/aft/tests/integration/subc_plumbing_drift_test.rs`; …
   - Grep found: `crates/aft/src/subc/health.rs`
   - Opened/edited: `crates/aft/src/subc/health.rs`

### `topk_cut` (10 episode(s))
1. Query: `VaultHandles load vault-handles.json`
   - Search returned: `docs/loop/ledger.md`; `crates/broca-module-serve/src/config_home.rs`; `crates/broca-module-serve/src/vault_config.rs`; …
   - Grep found: `crates/broca-module-serve/src/main.rs`
   - Opened/edited: `crates/broca-module-serve/src/main.rs`
2. Query: `fn write_gather_log`
   - Search returned: `crates/prefrontal-core-module/src/gather_evidence/runtime.rs`
   - Grep found: `crates/prefrontal-core/src/evidence/cache.rs`
   - Opened/edited: `crates/prefrontal-core/src/evidence/cache.rs`
3. Query: `"log retention sweep: removed_files="`
   - Search returned: `benchmarks/aft-search/bundles/aft-evidence-30d4a64f.zip`
   - Grep found: `crates/aft/src/logging.rs`
   - Opened/edited: `crates/aft/src/logging.rs`

### `wrong_lane` (6 episode(s))
1. Query: `cortexkit-cow isolation backend diff usage`
   - Search returned: `crates/cortexkit-cow/src/lib.rs`; `crates/cortexkit-cow/src/zfs.rs`; `crates/prefrontal-core-module/Cargo.toml`; …
   - Grep found: `crates/prefrontal-core-module/src/worktree.rs`
   - Opened/edited: `crates/prefrontal-core-module/src/worktree.rs`
2. Query: `manager.ingest_event wire operation handler in core module`
   - Search returned: `packages/core/src/features/background-agent/manager-consumer.ts`; `packages/opencode/src/plugin/event.ts`; `packages/opencode/src/features/host-bridge/canonical-adapter.ts`; …
   - Grep found: `crates/prefrontal-core-module/src/task_lifecycle/runtime.rs`
   - Opened/edited: `crates/prefrontal-core-module/src/task_lifecycle/runtime.rs`
3. Query: `ingest_canonical event into store task lifecycle`
   - Search returned: `crates/prefrontal-core-module/src/manager_runtime.rs`; `crates/prefrontal-core-module/src/subscription_hub/mod.rs`; `packages/opencode/src/features/host-bridge/canonical-adapter.ts`; …
   - Grep found: `crates/prefrontal-core-module/src/task_lifecycle/runtime.rs`
   - Opened/edited: `crates/prefrontal-core-module/src/task_lifecycle/runtime.rs`

### `stale_index` (3 episode(s))
1. Query: `profile --memory`
   - Search returned: `.alfonso/release-notes/v0.56.0.md`; `docs/memory-census.md`; `benchmarks/aft-search/real-query-baseline.json`
   - Grep found: `crates/aft/src/cli/profile.rs`
   - Opened/edited: `crates/aft/src/cli/profile.rs`
2. Query: `struct LineageId`
   - Search returned: `docs/contracts/d5-preservation/22a42ba3/fold-sections.json`
   - Grep found: `crates/thalamus-core/src/d5_protocol.rs`
   - Opened/edited: `crates/thalamus-core/src/d5_protocol.rs`
3. Query: `derived_path`
   - Search returned: `crates/aft/tests/integration/view_assembly_wiring_test.rs`; `crates/aft/src/views/assembly.rs`; `crates/aft/src/executor/view_publication_tests.rs`; …
   - Grep found: `crates/aft/src/callgraph_store/mod.rs`
   - Opened/edited: `crates/aft/src/callgraph_store/mod.rs`

### `nonproject_root` (2 episode(s))
Only 2 fixture episode(s) exist in this window; all are shown.
1. Query: `AskAttachment index decoding attachments digest`
   - Search returned: `Alfonso/Wire/ManagementClient.swift`; `Alfonso/Asks/AskDetailView.swift`; `Alfonso/Asks/AskListViewModel.swift`; …
   - Grep found: `Sources/SubcChatAskSupport/AskModels.swift`
   - Opened/edited: `Sources/SubcChatAskSupport/AskModels.swift`
2. Query: `serde alias`
   - Search returned: `Found 0 match across 0 file [index: fallback]`
   - Grep found: `crates/mc-module/tests/d5_redeem_vectors.rs`
   - Opened/edited: `crates/mc-module/tests/d5_redeem_vectors.rs`

### `identifier_not_definition_first` (2 episode(s))
Only 2 fixture episode(s) exist in this window; all are shown.
1. Query: `spawn_cadence`
   - Search returned: `crates/prefrontal-core-module/src/task_lifecycle/runtime.rs`; `docs/campaign/census/S01-recovery-and-campaign-foundation.json`; `ci/gates/s16_derived_scheduler_and_residual_completion.py`; …
   - Grep found: `crates/prefrontal-core-module/src/manager_runtime.rs`
   - Opened/edited: `crates/prefrontal-core-module/src/manager_runtime.rs`
2. Query: `SessionStatus`
   - Search returned: `/private/tmp/opencode-v1.18.29-issue220/specs/v2/api.html`; `/private/tmp/opencode-v1.18.29-issue220/packages/app/src/context/server-sync.tsx`; `/private/tmp/opencode-v1.18.29-issue220/packages/app/src/context/server-session.ts`; …
   - Grep found: `/private/tmp/opencode-v1.18.29-issue220/packages/opencode/src/session/status.ts`
   - Opened/edited: `/private/tmp/opencode-v1.18.29-issue220/packages/opencode/src/session/status.ts`

### `path_filter_missing` (2 episode(s))
Only 2 fixture episode(s) exist in this window; all are shown.
1. Query: `tokens: {`
   - Search returned: `crates/prefrontal-core-module/src/task_lifecycle/attachment.rs`; `crates/prefrontal-core-module/src/gather_evidence/runtime.rs`; `packages/core/src/config/schema/athena-v2.ts`; …
   - Grep found: `packages/opencode/src/plugin/event.test.ts`
   - Opened/edited: `packages/opencode/src/plugin/event.test.ts`
2. Query: `command-dialogs`
   - Search returned: `STRUCTURE.md`; `ARCHITECTURE.md`; `docs/feature-catalogue.md`; …
   - Grep found: `packages/opencode/src/tests/command-dialogs.test.ts`
   - Opened/edited: `packages/opencode/src/tests/command-dialogs.test.ts`

### `index_building` (2 episode(s))
Only 2 fixture episode(s) exist in this window; all are shown.
1. Query: `\b100\b`
   - Search returned: `crates/aft/tests/semantic_chunk_census.rs`; `crates/aft/tests/list_envelope_outline.rs`; `crates/aft/tests/list_envelope_grep.rs`; …
   - Grep found: `benchmarks/aft-search/search_quality_lib.py`
   - Opened/edited: `benchmarks/aft-search/search_quality_lib.py`
2. Query: `redeem-vectors-v1.json byte size changed`
   - Search returned: `Semantic index is reloading from the shared read-only snapshot; retry shortly.`
   - Grep found: `crates/mc-module/tests/d5_coverage_proof_vectors.rs`
   - Opened/edited: `crates/mc-module/tests/d5_coverage_proof_vectors.rs`

### `token_variant` (2 episode(s))
Only 2 fixture episode(s) exist in this window; all are shown.
1. Query: `83329`
   - Search returned: `Semantic search unavailable: semantic index is read-only but loading stopped at the interactive budget (borrowed_semantic_index_load_budget)`
   - Grep found: `crates/mc-module/tests/d5_coverage_proof_vectors.rs`
   - Opened/edited: `crates/mc-module/tests/d5_coverage_proof_vectors.rs`
2. Query: `leases.acquire`
   - Search returned: `crates/broca-core/src/wal_fold/tests.rs`; `crates/broca-core/src/wal_fold.rs`; `crates/broca-core/src/lease.rs`; …
   - Grep found: `crates/broca-module/src/opener.rs`
   - Opened/edited: `crates/broca-module/src/opener.rs`

### `phrase_present_not_surfaced` (1 episode(s))
Only 1 fixture episode(s) exist in this window; all are shown.
1. Query: `OpenCode 2 parity record imposed vs chosen divergence keymap.layer sidebar`
   - Search returned: `packages/plugin/src/plugin/rpc-handlers.ts`; `packages/plugin/src/features/magic-context/compaction-marker.ts`; `packages/dashboard/src-tauri/src/db.rs`; …
   - Grep found: `PARITY.md`
   - Opened/edited: `PARITY.md`

### `scope_mismatch` (1 episode(s))
Only 1 fixture episode(s) exist in this window; all are shown.
1. Query: `connectionFile`
   - Search returned: `packages/core/src/claustrum.ts`; `packages/opencode/src/index.ts`; `packages/e2e-tests/src/mock-claustrum.ts`; …
   - Grep found: `node_modules/@cortexkit/subc-client/src/client.ts`
   - Opened/edited: `node_modules/@cortexkit/subc-client/src/client.ts`

### `other` (0 episode(s))
No episode received this label.

## Top 10 failed query strings

All 29 failed query strings occurred once, so frequency does not distinguish them; this table uses lexicographic order as the tie-break.

| Query | Failures | What would have fixed it |
|---|---:|---|
| `"log retention sweep: removed_files="` | 1 | Put the exact logging.rs phrase hit inside the first three results; the current rerun places it fourth. |
| `#[ignore]` | 1 | Rank the exact ignored gateway baseline test inside the requested top 30. |
| `83329` | 1 | Match digit-separated numeric literals so `83329` finds `83_329`. |
| `OpenCode 2 parity record imposed vs chosen divergence keymap.layer sidebar` | 1 | Surface the root PARITY.md record for its exact keymap and parity terms instead of only implementation files. |
| `Pi session start notice config warning banner test logPiConfigLoad` | 1 | Place the matching index.test.ts beside the index.ts result when tests are included. |
| `SessionStatus` | 1 | Rank the SessionStatus definition ahead of consumers and generated API references. |
| `VaultHandles load vault-handles.json` | 1 | Return the main.rs load call within topK=4; the current rerun places it seventh. |
| `\b100\b` | 1 | Wait for the lexical index to be ready before answering the regex query. |
| `carry_required` | 1 | Put the exact subc_transform.rs source hit inside topK=15 rather than behind contract documents. |
| `command-dialogs` | 1 | Honor the requested tests-directory scope so command-dialogs.test.ts is returned. |

## Method and retained evidence

- OpenCode was read from `~/.local/share/opencode/opencode.db` with SQLite URI `mode=ro`, `query_only=ON`, a fixed maximum part rowid, and streaming cursors. The 22 GB database was never copied or written.
- Pi JSONL records under `~/.pi/agent/sessions` were included through the same cutoff. Only model turns containing tool calls entered the call stream.
- An episode is one `aft_search` and its next three tool calls. Qualifying follow-ups are AFT `grep`, `ast_grep_search`, or conservatively parsed bash command segments whose executable is `grep`, `rg`, `ag`, or `git grep`; every bash decision carries a reason in `episodes.jsonl`. Search→search calls are refinements, counted separately.
- Discriminating means grep produced a path absent from the full search output and a later call inside the same three-call window opened or edited that path.
- `.alfonso/data/search-sweep-2d/episodes.jsonl` contains all 447 qualifying episodes, labels, reasons, and current rerun ranks. `raw/*.txt` retains full stored search/follow-up inputs and outputs plus intervening agent text. These files are intentionally gitignored and are not part of this commit.

## Reference rebaseline after ranking slice 3 (2026-09-18)

Slice 3 landed on train 107 under a `ranking` descriptor (targeted mechanism
`wrong_lane_nl`): real-query MRR@10 0.154 → 0.188, hit@5 0.233 → 0.302,
census-weighted MRR 0.124 → 0.152. A ranking landing passes the gate but does
not move the committed reference; that is an audited act, and it was not done
in the following train. Trains 108–113 carried no ranking-fence file, so the
byte-equality predicate never ran, and train 114 — the first fenced diff
since (the `semantic.query_timeout_ms` ceiling constant) — refused with
`engine_unwired_mismatch` on row `followup-census:3184`, its uploaded diff
showing exactly slice 3's numbers.

The reference pair was re-recorded with `cost-gate.sh --search-quality
--mode record-reference` on a release build of main at `d2d27fa96` (slice-3
engine plus the ceiling constant, which is byte-neutral where query embeds
finish inside the budget): 43 rows, profile `paged`, MRR@10 0.187984 —
identical to the CI head score that exposed the gap. Rule recorded: the
train after any `ranking` landing carries the rebaseline.

Second re-record, 2026-09-20 (train 133). The gate's capability row hashes
`packages/pi-plugin/src/tools/semantic.ts` whole, so the includeTests coercion
fix in that file (external bug 9) moved `real_query.capability.schema_sha256`
while every ranked row stayed byte-equal, and the train refused with
`engine_unwired_mismatch`. Reference pair re-recorded on a release build of
`8b700f4ac`: 43 rows, `paged`, MRR@10 0.187984, unchanged; the only leaves that
moved are the baseline sha, the binary sha and the capability schema sha. A
follow-up worth taking: hash the `aft_search` schema block rather than the file,
so unrelated edits to the tool file do not read as capability changes.
