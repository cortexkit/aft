# Per-result row-facts spike — 2026-09

## Summary

**Decision: do not add a default risk suffix to every search/navigation row.** In the three largest repository strata, only 3,875 of 181,346 classified calls in search follow-up windows (2.14%) were even in the zoom/callgraph/root-inspect ceiling. The 200-event audit found only eight fully answerable follow-ups, all zero-caller queries. A four-fact suffix added **200.37 bytes/search** at `topK=10` (about 50.1 tokens using the disclosed bytes/4 estimate), or **169.55 bytes/42.4 estimated tokens** after removing the caller count that AFT already renders.

| Candidate fact | Yes / no / partial (n=200) | Misleading when available | Added bytes/search, 500-query replay | Recommendation | Deciding number |
|---|---:|---:|---:|---|---|
| Direct caller count | 8 / 173 / 19 | 1/93 (1.08%) | 30.82 gross; **0 incremental** | **Ship**: retain the existing `↩N`; do not duplicate it | All 8 yes cases were zero callers, while 4,125/4,766 historical result rows already carried `↩N` |
| Names of reaching test files | 0 / 196 / 4 | 1/58 (1.72%) | **132.81** | **Do not ship** | 0 yes, largest per-fact cost, and one 63-test-file transitive reach was mostly module-touch evidence |
| Fresh cyclomatic complexity | 0 / 200 / 0 | 0/83 (0%) | **9.27** | **Ship as opt-in** | Cheapest fact, but it replaced 0 calls and was unavailable for every MTPLX replay row |
| Incoming edge provenance | 0 / 199 / 1 | 0/83 (0%) | **27.47** | **Ship as opt-in** | Only 1 partial; expose it where a caller/impact intent already exists, not on every search row |

“Ship” for caller count means **no product change**: keep the current compact annotation. This spike changes no agent-facing tool. The opt-in recommendations are evidence for a future explicit risk/navigation view, not a feature delivered here.

## Corpus and cutoff

The read-only OpenCode snapshot is the rolling 30-day interval **2026-08-08 08:03:43.889Z through 2026-09-07 08:03:43.889Z** (`end_ms=1788768223889`). The extraction saw 783,491 terminal tool calls. Repository attribution uses `session.project_id -> project.worktree`, so primary checkouts and Alfonso worktrees are grouped together.

The three largest completed-search strata at that cutoff were:

| Repository | Completed searches | Sessions |
|---|---:|---:|
| prefrontal | 14,234 | 944 |
| magic-context | 13,521 | 682 |
| AFT | 8,955 | 596 |

Failed search anchors were excluded because they have no ranked rows. Completed and failed calls both remain eligible as subsequent calls. Calls in the anchor's own `message_id` are excluded: parallel tool calls in one assistant message cannot have been caused by seeing one another's output.

## 1. Follow-up census

For each completed `aft_search`, and separately each completed `aft_callgraph` `callers`/`impact`, the script takes the next five terminal tool calls in the same session and assigns each call exactly one class. A zoom/callgraph match requires the same returned `(file, symbol)`; read/edit requires a returned file; root inspect means absent/`.`/repository scope. The denominator below is follow-up calls, not anchors. “Anchor incidence” answers the complementary question: how often did an anchor's five-call window contain the class at least once?

### Search anchors, all three repositories

| Class | Calls | Share of 181,346 | Anchors with class | Share of 36,710 anchors |
|---|---:|---:|---:|---:|
| (a) zoom returned symbol | 3,816 | 2.104% | 2,966 | 8.080% |
| (b) callers/impact returned symbol | 30 | 0.017% | 29 | 0.079% |
| (c) inspect root | 29 | 0.016% | 24 | 0.065% |
| (d) read returned file | 15,789 | 8.707% | 8,277 | 22.547% |
| (e) edit returned file | 1,462 | 0.806% | 904 | 2.463% |
| (f) another search | 42,319 | 23.336% | 17,692 | 48.194% |
| (g) unrelated | 117,901 | 65.014% | 33,287 | 90.676% |

Classes overlap at the anchor-window level, so the incidence percentages must not be summed. At the call level, the raw removable ceiling is `(a)+(b)+(c) = 3,875 / 181,346 = 2.14%`. Most of that ceiling is zoom, which normally requests source rather than risk metadata.

### Search anchors by repository

| Repository | Anchors | (a) zoom | (b) graph | (c) inspect | (d) read | (e) edit | (f) search | (g) unrelated |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| AFT | 8,955 | 907 (2.06%) | 8 (0.02%) | 5 (0.01%) | 4,352 (9.87%) | 546 (1.24%) | 9,626 (21.84%) | 28,634 (64.96%) |
| magic-context | 13,521 | 1,599 (2.39%) | 1 (<0.01%) | 7 (0.01%) | 5,500 (8.21%) | 401 (0.60%) | 17,089 (25.52%) | 42,365 (63.27%) |
| prefrontal | 14,234 | 1,310 (1.86%) | 21 (0.03%) | 17 (0.02%) | 5,937 (8.44%) | 515 (0.73%) | 15,604 (22.19%) | 46,902 (66.71%) |

The ceiling is stable across codebases: 2.09% of classified AFT follow-up calls, 2.40% for magic-context, and 1.92% for prefrontal.

### Callgraph anchors

There were only 89 completed `callers` anchors and one completed `impact` anchor in these repositories.

| Caller-anchor class | Calls | Share of 445 |
|---|---:|---:|
| (a) zoom returned symbol | 4 | 0.899% |
| (b) callers/impact returned symbol | 5 | 1.124% |
| (c) inspect root | 1 | 0.225% |
| (d) read returned file | 21 | 4.719% |
| (e) edit returned file | 9 | 2.022% |
| (f) another search | 27 | 6.067% |
| (g) unrelated | 378 | 84.944% |

All five calls after the sole `impact` anchor were unrelated. The callgraph-anchor `(a)-(c)` ceiling was 10/445 (2.25%), also too small to justify taxing every navigation row.

## 2. Would a row fact have answered the follow-up?

The audit is a deterministic stratified probability sample (`seed=20260907`) from 3,856 class-(a)/(b)/(c) events with rendered follow-up output. Because graph/root-inspect events are rare, it includes all 33 graph and all 30 root-inspect events, then randomly selects 137 zoom events. Raw counts are reported rather than pretending this is a proportional sample. I reviewed every rare-stratum call and output and the flagged misleading cases; the script records the executable adversarial rubric.

- **Yes** means the fact supplies everything material in the actual response.
- **Partial** means it supplies one datum but omits identities, locations, source, traversal depth, or another requested inspect category.
- **No** is not promoted to partial merely because the fact is generally related.

| Fact | Yes | No | Partial | What the yes/partial cases actually were |
|---|---:|---:|---:|---|
| Direct caller count | 8 | 173 | 19 | Yes: eight `0 callers` responses. Partial: caller counts where the response also supplied caller identity/site. Transitive `impact` is not direct fan-in. |
| Reaching test-file names | 0 | 196 | 4 | Four caller investigations had test origins, but file names omitted caller symbol, line, and assertion semantics. |
| Fresh complexity | 0 | 200 | 0 | Root inspect calls asked for TODOs, dead code, duplicates, cycles, diagnostics, or all health; no call asked only for a returned row's complexity. |
| Edge provenance | 0 | 199 | 1 | One callers result contained approximate `~` edges; aggregate provenance counts helped assess trust but did not identify the callers/sites. |

The eight yes cases would have removed **1,131 bytes total** of follow-up input plus rendered output (141.4 bytes/call). No other fact had a yes case. Even before token cost, the observed fully replaceable set is therefore caller-count-only.

AFT already presented caller annotations in 439/500 historical top-10 searches and on 4,125/4,766 parsed symbol rows. An agent still issuing `callers` after seeing `↩N basename,…` is usually asking who/where/depth, not asking for the same count again.

## 3. Could the fact mislead?

The denominator here is sample rows for which the current read-only store could render that fact, not all 200 rows. Targets absent from the current generation are “unavailable,” never silently zero.

| Fact | Available | Wrong or misleading | Rate |
|---|---:|---:|---:|
| Direct caller count | 93 | 1 | 1.08% |
| Reaching test-file names | 58 | 1 | 1.72% |
| Fresh complexity | 83 | 0 | 0% |
| Edge provenance | 83 | 0 | 0% |

Two examples explain why facts need provenance and absence semantics:

1. `record_campaign_delivery_rejection` in prefrontal resolved to `callers=4`, but all four incoming call sites were `name_match`; exact/resolver and `type_match` contributed zero. A naked fan-in of four overstates known direct use. The historical follow-up itself found the callgraph building, so the count would have looked more definitive than the evidence available to that agent.
2. `ModuleMemoryAuthorityError` had one direct caller (`assertTsMemoryWriteAllowed`), while transitive reverse traversal reached **63 test files**. **55/63** entered through `<top-level>`/setup/smoke-like origins. A suffix such as `tests=pi-compaction-off.test.ts,auto-search-pi.test.ts,+61` is factually a reachability list, but it is not evidence that any of those tests assert this error's behavior.

The renderer used for measurement refuses ambiguous symbol/overload matches, omits stale complexity, and labels resolver/type/name edge counts separately. Those safeguards explain the zero observed complexity/provenance mislead counts; dropping them would change the result.

## 4. Token/byte cost

### Candidate format

The throwaway renderer appends only available facts:

```text
 callers=4 tests=crates/prefrontal-core-store/tests/spec_campaign_s2_store.rs,crates/prefrontal-core-store/tests/work_graph_store.rs cx=2 edges=n4
```

`tests` shows at most two repository-relative paths plus `,+N`; `r/t/n` mean `treesitter+resolver`, `type_match`, and `name_match`. An absent fact means unavailable, not zero; an omitted `r`, `t`, or `n` component has count zero. Complexity is emitted only when the inspect contribution's size and nanosecond mtime match the live file and the symbol/line resolves unambiguously.

### 500 real-query replay requested by the spike

The replay executes 500 query strings taken from real `aft_search` calls against current repositories with `top_k=10`: AFT (167 queries), OSS opencode (167, TypeScript), and OSS MTPLX (166, Python-majority). AFT and opencode use their own session queries. The database had no MTPLX sessions, so MTPLX uses a seeded sample from the full real-query pool; this tests row-shape cost, not MTPLX task relevance. Search artifacts are APFS clone-copied to temporary storage, while callgraph/inspect generations are opened `mode=ro`.

| Repository | Searches / symbol rows | Caller | Tests | Complexity | Provenance | Full bundle | Bundle minus existing caller count |
|---|---:|---:|---:|---:|---:|---:|---:|
| AFT | 167 / 1,340 | 32.66 | 175.16 | 16.36 | 29.46 | 253.64 | 220.98 |
| opencode (TS) | 167 / 883 | 30.90 | 147.11 | 11.40 | 26.14 | 215.56 | 184.66 |
| MTPLX (Python) | 166 / 1,242 | 28.88 | 75.80 | 0.00 | 26.81 | 131.49 | 102.61 |
| **Combined** | **500 / 3,465** | **30.82** | **132.81** | **9.27** | **27.47** | **200.37** | **169.55** |

Cells are mean added UTF-8 bytes per search, including zero when a search returned no symbol rows. MTPLX had no metadata-fresh inspect complexity contribution, correctly producing no `cx` bytes rather than `cx=0`.

No model tokenizer is assumed. The disclosed bytes/4 estimate makes the combined full bundle **50.1 tokens/search** and the actually incremental bundle **42.4 tokens/search**. For scale, one 200.37-byte full-bundle search costs more than the 141.4-byte average fully replaced follow-up—and only eight audited events were replaceable, while the suffix would be paid on every search.

### Historical-output sensitivity check

A separate 500-search sample from the three session repositories (167 AFT, 167 magic-context, 166 prefrontal; 4,766 rows) joined current stores onto the recorded rows. It measured 313.38 bytes/search for the full bundle and 277.52 after deduplicating caller count. This is higher than the current-repository replay because the historical rows had denser transitive test reach. Current stores cannot resolve every historical row after code movement, so this is a cost sensitivity check rather than the primary replay estimate.

## Recommendations by fact

1. **Direct callers — ship (retain existing).** `↩N` has zero incremental cost and answered the only eight fully replaceable cases. Preserve caller basenames and provenance-aware follow-up navigation; do not add `callers=N` beside it.
2. **Reaching test files — do not ship.** It costs 132.81 bytes/search, answered zero calls, and its one misleading case demonstrates the exact `tested=1` failure mode. If exposed elsewhere, call it `test_reach`, list files, and never label it tested/covered/asserted.
3. **Complexity — ship as opt-in.** At 9.27 bytes/search it is cheap and had no observed mislead, but it removed zero calls and freshness made it absent throughout one codebase. An explicit risk view can preserve “absent means unavailable” without taxing ordinary search.
4. **Edge provenance — ship as opt-in.** It was partial once, yes zero times, and costs 27.47 bytes/search. It belongs on caller/impact output or an explicit facts request where the agent is already evaluating graph evidence.

## Reproduction

Committed scripts:

- [`scripts/row-facts-census.sql`](scripts/row-facts-census.sql): exact terminal-call extraction and repository-volume SQL; next-five matching is implemented in Python.
- [`scripts/row-facts-spike.py`](scripts/row-facts-spike.py): row parser, next-five classifier, sample/rubric, read-only store joins, historical cost, and cross-language store-shape check.
- [`scripts/row-facts-replay.py`](scripts/row-facts-replay.py): isolated 500-query replay and suffix byte measurement.

Commands used for this report:

```bash
python3 docs/investigations/scripts/row-facts-spike.py \
  --end-ms 1788768223889 \
  --output /tmp/row-facts.json \
  --sample-csv /tmp/row-facts-sample.csv

cargo build -p agent-file-tools --bin aft --profile stage
python3 docs/investigations/scripts/row-facts-replay.py \
  --binary target/stage/aft \
  --end-ms 1788768223889 \
  --output /tmp/row-facts-replay.json
```

Every SQLite open in the Python scripts uses `file:...?mode=ro` plus `PRAGMA query_only=ON`. The SQL can be run directly with a bound `:end_ms`, but never invoke `sqlite3` with a bare database path: a misspelled path would create a decoy database. The replay's temporary search artifacts are removed after each repository; no live OpenCode database, live AFT artifact database, GitHub issue, or product-code branch is written.

## Limitations

- The yes/no/partial rules in `row-facts-spike.py` (`rate_event`) only ever assign `yes`/`partial` to class-(b) graph and
  class-(c) inspect follow-ups; every class-(a) zoom follow-up in the 200-sample (137 of them) is `no` by
  construction, not by judgment. That is defensible - a zoom fetches a body, which no row fact replaces - but it means
  the table measures "could a fact replace a graph/inspect call", and the zoom majority was never adjudicated.

- Tool traces show the action selected, not the agent's unspoken intent. The audit therefore grades conservatively and never upgrades a related fact to “yes.”
- The current callgraph/inspect generations are not historical snapshots. Facts unavailable after code movement are omitted; follow-up input/output remains the source of truth for what the agent actually requested and saw.
- Static test reach misses dynamic dispatch, subprocesses, and runtime registration, while also including module-touch paths. It measures graph reachability only.
- Byte counts are exact for the stated suffix; token counts are explicitly bytes/4 estimates and will vary by model tokenizer.
- The MTPLX replay uses real queries from other repositories because no MTPLX session existed in the database. It is a language/code-shape cost check, not a behavioral follow-up census.
