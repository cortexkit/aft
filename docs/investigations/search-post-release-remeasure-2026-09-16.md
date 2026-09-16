# aft_search post-release re-measure (2026-09-16)

Live telemetry re-extract of the Sep 8 follow-up census, on the same episode
definition, after search A (engine) and B2 (shape router) were placed on this
fleet's daemon. The extract directory is gitignored at
`.alfonso/data/aft-search-followup-census-2026-09-16/`.

## Method

- **Extract:** unchanged `extract.py` from the Sep 8 census, `--since 2026-08-26`,
  reading `~/.local/share/opencode/opencode.db` as `file:?mode=ro` (streamed;
  snapshot max part rowid 8,603,973). Pi sessions under `~/.pi/agent/sessions`
  were included with the same collector. An episode is still an `aft_search`
  followed within three tools by a grep-family call (`grep` tool, bash grep, or
  `ast_search`). Discriminating means `derived.any_new_file_then_acted`.
- **Release cut for this fleet:** 2026-09-12 (daemon cards from main on
  2026-09-10..12), not the v0.56.0 tag date 2026-09-15.
- **Estimator:** `scripts/telemetry/postA-remeasure.sh` →
  `benchmarks/aft-search/post_release_remeasure.py --release-date 2026-09-12`
  with `--discriminating 326 --all-episodes 1168` taken from this extract's
  post-cut window, rubric `benchmarks/aft-search/failure-rubric-v1.md`.
  `--discriminating` is D (discriminating episodes in the measured window);
  `--all-episodes` is N (qualifying `aft_search`→grep episodes in that window).
  R34 compares a conditional rate against 3,996/6,469 and a projected
  all-episode rate against 3,996/20,183; the conditional interval is never
  compared with 0.198.
- **Labels:** the census's 300-row `labels.jsonl` joined onto the new extract by
  `(session_id, call_id)`. 89 matched, all in Aug 26–Sep 8. Zero matched on or
  after 2026-09-12. 42 additional in-window labelled sessions from the Sep 8
  census are gone from `opencode.db` (retention). True-failure is
  `label != not_a_search_failure`, matching the 3,996/6,469 numerator
  (`scope_mismatch` and `other` stay in).
- **Latency / not-ready / `[exact]` on all searches:** second read-only pass
  over OpenCode `part.state.time` (`end - start`) plus a Pi session walk.
  OpenCode search counts in that pass differ from `extract.py` by 6 calls
  (34,138 vs 34,132).

## Coverage (2026-08-26 .. 2026-09-16)

| Harness | aft_search calls | Qualifying episodes | Discriminating | Refinement loops |
|---|---:|---:|---:|---:|
| OpenCode | 34,132 | 7,639 | — | 21,123 |
| Pi | 403 | 149 | — | 188 |
| **Total** | **34,535** | **7,788** | **2,531 (32.5%)** | **21,311 (61.7% of searches)** |

Sep 8 census (≈6 months): 90,363 searches, 20,183 episodes, 6,469 discriminating
(32.05%), projected true-failure 3,996/20,183 (19.80%).

## True-failure rates (R34)

`post_release_remeasure.py --release-date 2026-09-12` over the new post-cut
discriminating episodes returned **no labelled episodes; no interval computed**.
The 300 census labels do not fall in 2026-09-12..now, and this box was not
re-labelled. The numbers below therefore hold the original five-stratum
conditional rate (300 labels, original census weights) and only update D/N
from this extract. A mix-updated sensitivity reweights the same within-shape
label shares onto this window's shape counts. Intervals are the estimator's
approximate containment summaries, not significance tests.

| Window | D | N | D/N | Conditional (within D) vs 3,996/6,469 | Projected (all episodes) vs 3,996/20,183 |
|---|---:|---:|---:|---|---|
| Census (through 2026-09-08) | 6,469 | 20,183 | 32.05% | 61.77% | 19.80% |
| Pre (Aug 26–Sep 8) | 1,837 | 5,637 | 32.59% | 61.77% transported; 66.26% on 89 matched labels (95% 54.58–76.23) | 20.13% transported (95% 17.94–22.32); 21.59% matched (95% 17.89–25.29) |
| Bridge (Sep 9–11) | 368 | 983 | 37.44% | 61.77% transported | 23.12% transported (95% 20.12–26.13) |
| **Post (Sep 12–now)** | **326** | **1,168** | **27.91%** | **61.77% transported (not re-labelled)** | **17.24% transported (95% 14.87–19.61)** |
| Post, mix-updated | 326 | 1,168 | 27.91% | 56.75% (identifier-heavy mix) | 15.84% |

The matched-89 pre interval is under-populated (17/27/15/16/14 labels per
stratum against a 60 target) and is a retention-biased leftover of the original
sample, not a new draw.

## Mechanism projection

Projected with the census within-shape sample shares × this extract's
discriminating shape counts (independent rounding, R37). Not a new labelling.

| Mechanism | Census (D=6,469) | Pre (D=1,837) | Post (D=326) |
|---|---:|---:|---:|
| `not_a_search_failure` | 2,473 (38.23%) | 719 (39.1%) | 141 (43.3%) |
| `wrong_lane_nl` | 789 (12.19%) | 219 (11.9%) | 24 (7.4%) |
| `index_stale_or_missing` | 719 (11.11%) | 197 (10.7%) | 31 (9.5%) |
| `topk_cut` | 552 (8.54%) | 154 (8.4%) | 25 (7.7%) |
| `phrase_present_not_surfaced` | 545 (8.43%) | 150 (8.2%) | 27 (8.3%) |
| `renamed_or_variant_token` | 532 (8.22%) | 155 (8.4%) | 32 (9.8%) |
| `scope_mismatch` | 475 (7.34%) | 134 (7.3%) | 24 (7.4%) |
| `other` | 284 (4.38%) | 77 (4.2%) | 15 (4.6%) |
| `identifier_not_definition_first` | 101 (1.56%) | 31 (1.7%) | 7 (2.1%) |

Post `wrong_lane_nl` shrinks because NL's share of discriminating episodes fell,
not because NL queries were re-labelled.

## Shape strata (discriminating episodes)

| Shape | Census share | Pre Aug 26–Sep 8 | Bridge Sep 9–11 | Post Sep 12–now |
|---|---:|---:|---:|---:|
| `identifier` | 31.15% (2,015) | 34.1% (627) | 32.3% (119) | **44.2% (144)** |
| `code_literal` | 26.56% (1,718) | 25.1% (461) | 30.2% (111) | 20.6% (67) |
| `short` | 12.35% (799) | 11.8% (216) | 9.2% (34) | 16.0% (52) |
| `nl` | 28.13% (1,820) | 27.5% (505) | 26.4% (97) | **16.9% (55)** |
| `log_excerpt` | 1.81% (117) | 1.5% (28) | 1.9% (7) | 2.5% (8) |
| **D** | **6,469** | **1,837** | **368** | **326** |

The official estimator still weights strata by the census shares above, not by
the measured-window mix.

## Per-era operational table

Searches, `[exact]`, and not-ready are OpenCode+Pi. Latency is OpenCode
`part.state.time` only (Pi has no `state.time`; 1 OpenCode search in the pre
era lacked timestamps). Refinements are `refinements_within_window` on
qualifying episodes from the unchanged extract (query-changing `aft_search`
inside the three-tool window). All-search refinement-loop starts remain
21,311/34,535 (61.7%) pooled; the extract does not emit that series by day.

| Era | Searches | aft_search→grep episodes | Discriminating | aft_search→aft_search refinements | `[exact]` share (searches) | p50 ms | p90 ms | not-ready (`loading_artifacts` / `reloading` / still building) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Aug 26–Sep 8 | 26,143 | 5,637 | 1,837 | 2,573 | 2,056/26,143 (7.9%) | 1,261 | 10,587 | 5,219/26,143 (20.0%) |
| Sep 9–11 | 4,430 | 983 | 368 | 373 | 1,365/4,430 (30.8%) | 1,373 | 8,983 | 210/4,430 (4.7%) |
| Sep 12–now | 3,968 | 1,168 | 326 | 406 | 2,440/3,968 (61.5%) | 1,032 | 4,749 | 69/3,968 (1.7%) |

Episode-level `[exact]` (census's original denominator): 678/5,637 (12.0%) →
270/983 (27.5%) → 730/1,168 (62.5%). Census through Sep 8 was 847/20,183 (4.2%).

## Confounds

1. **No clean release boundary.** This box ran main cards throughout. Engine and
   shape-router cards landed on the daemon across 2026-09-10..12; v0.56.0 was
   tagged 2026-09-15. Sep 9–11 is a dirty bridge, not a holdout.
2. **Grep on logs / CI output is not a search failure.** The census rubric
   already drops those as `not_a_search_failure` when labelled. In the post era,
   16/1,696 qualifying grep follow-ups (1/164 bash grep) had found/acted-on
   absolute paths outside every known project root (OpenCode `project.worktree`,
   `project_directory`, and session directories). Examples: `/tmp` and
   `/private/tmp` views-drill / OpenCode2 unpack logs, `~/.local/share/cortexkit/staging`
   rollback images, `~/.local/share/cortexkit/aft/cache-keys.json`. That is 0.9%
   of post grep follow-ups and cannot move D/N.
3. **Train/CI-heavy week.** Post-era qualifying episodes are 85% Alfonso
   worktrees (vs 72% pre). Identifier share of discriminating episodes jumped
   to 44%; NL fell to 17%. Search volume is ~794/day post vs ~1,867/day in
   Aug 26–Sep 8. Fewer masons doing exploratory code search.
4. **Label retention.** 42 labelled Aug 26–Sep 8 sessions from the Sep 8 census
   are no longer in `opencode.db`, so even the pre matched sample is incomplete.
5. **Transported conditional rate.** Holding 61.77% true-failure-within-D after
   the engine shipped assumes the within-shape failure process did not change.
   `[exact]` and not-ready moved enough that this assumption is the claim to
   distrust, not a result.

## Verdict

The official post-cut estimator has nothing to label: this fleet's 2026-09-12
window contains 326 discriminating `aft_search`→grep episodes and none of the
census's 300 labels, and the box never had a clean release cut. What the same
extract *did* re-measure is the discriminating share (32.6% → 27.9%), an
identifier-heavier mix, `[exact]` tags on 62% of post searches (from 8% pre and
4.2% in the six-month census), not-ready replies down to 1.7% (from 20%), and
p90 latency halved (10.6 s → 4.7 s). Transporting the original 61.77%
conditional rate across the new D/N yields a 17.2% all-episode projection
against 19.80% (R34), or 15.8% if the new shape mix is allowed to speak — both
are mix and volume shifts, not a newly labelled failure-rate drop. Treat this
as evidence the engine is answering and tagging more on a quieter, more
identifier-shaped week, not as a claim that true search failures fell.
