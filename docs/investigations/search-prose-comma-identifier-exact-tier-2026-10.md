# Why prose with commas loses an identifier-heavy answer under the natural-language plan (2026-10)

## Context

aft_search routes `query` by shape. Until this branch, a sentence containing
an unquoted `,` or `;` counted as code syntax, so it routed as `code_literal`.
A magic-context search lost its answer that way: the query was 'Pi
auto-search hint selects latest user message to search on; skips synthetic
or custom messages' and the pattern was 'autoSearch|auto_search|runAutoSearch'.
Two commits on this branch address part of it:

- every non-regex shape now keeps the semantic lane (`search_b2/lane_plan.rs`
  `legal_lanes`);
- split-query definitions count declarations whose name extends an
  alternative, and the "defined in" line prefers them.

A third change was measured and left out. It reads `, ` and `; ` (a comma or
semicolon followed by whitespace) as prose punctuation in
`search_b2/router.rs` `has_code_syntax_outside_spans`, so such a sentence
routes as `natural_language`. It is kept on branch
`alfonso/task/bg_7c75791d75df3cb1-prose-punctuation-wip`. On top of the
semantic-lane change it improves three real-query rows and makes one worse:

| Row | Mechanism | Rank before | Rank after |
|---|---|---|---|
| followup-census:900035 | wrong_lane_nl | miss | 1 |
| followup-census:910013 | split_query_pattern_fusion | miss | 3 |
| followup-census:910014 | split_query_pattern_fusion | miss | 1 |
| followup-census:7972 | wrong_lane_nl | 1 | miss |

`wrong_lane_nl` MRR@10 is unchanged (0.416667: one row up, one down), and
`split_query_pattern_fusion` goes 0.269048 -> 0.364286 (measured without the
split-definition change).

## The row that gets worse

followup-census:7972, `includeTests: true`, answer
`crates/aft/tests/integration/subc_detach_test.rs`:

> test harness spawns a fake daemon TcpListener, module attaches via
> run_subc_mode_for_test on a thread, then the daemon drops the socket and the
> test asserts the module thread's result

## Per-lane ranks

Measured on the pinned evidence tree with the live MiniLM model and release
builds, `topK` 50. The gate's fixture vectors give the same outcome: rank 1
with the comma read as code, a miss with it read as prose.

| | code_literal (comma is code) | natural_language (comma is prose, or no commas) |
|---|---|---|
| Lanes run | exact, lexical, semantic | symbol, exact, lexical, semantic |
| Exact tier | none | definition (13+ files) |
| Answer's lexical score | 7.52, highest of all files (next 7.26) | same |
| Answer's semantic cosine | 0.60, highest (next 0.53) | 0.62, highest (next 0.53) |
| Answer's final rank | 1 | 13 |
| Ranks 1-3 | subc_detach_test.rs, subc/mod.rs, subc_bridge_test.rs | subc/mod.rs, subc_bridge_test.rs, commands/configure.rs |

The answer leads both scored lanes under both plans. Neither the lexical nor
the semantic signal is lost.

## Cause

The natural-language plan runs an exact pass for each identifier-shaped
token in the query (`run_engine_ranking` in
`commands/semantic_search/mod.rs`, the
`plan.shape == NaturalLanguage && plan.query_facts.has_identifier_token`
branch). Here the tokens are `run_subc_mode_for_test` and `TcpListener`.
Every file that contains one of those names verbatim becomes exact-tier
evidence: E1 phrase hits, plus the declaration in `subc/mod.rs`. More than a
dozen files qualify. The exact tier precedes every scored result and is
ordered score-free (`comparator.rs` `score_free_r3_cmp`: definition first,
then E1 occurrence count, then source before other files, then path). Lane
relevance cannot reorder it, so the answer, which mentions the names fewer
times, lands 13th.

Under `code_literal`, the exact lane looks for the whole sentence, finds no
verbatim match, and runs no per-identifier pass. Fusion of the lexical and
semantic lanes then puts the answer first. The comma did not cause the
miss. It hid a weakness of the natural-language plan: the same sentence
without commas already routes as `natural_language` and misses the same way.

## Candidate changes

1. Smallest: in `has_code_syntax_outside_spans`, read `, ` and `; ` as prose
   punctuation only when the query has no identifier-shaped token
   (`QueryFacts::has_identifier_token`).
   - Expected effect: 7972 keeps `code_literal` and rank 1. The magic-context
     query, 900035, 910013 and 910014 have no identifier-shaped token
     (`auto-search` and `re-exported` are below the 12-character kebab-case
     threshold), so they route as `natural_language` and move as measured
     above.
   - Cost: it is a punctuation exception. An identifier-bearing sentence
     with commas keeps the weaker `code_literal` plan (semantic weight 0.1),
     and the comma-free form of 7972 still misses.
2. Root cause: in the natural-language plan, do not put phrase hits from the
   identifier-fact passes into the score-free exact tier. Fuse them with lane
   scores as evidence, or order that part of the tier by fused score, so lane
   relevance decides among many files that merely mention a named
   identifier.
   - Expected effect: 7972 ranks by its lexical and semantic lead, with or
     without commas. The prose-punctuation change can then land without an
     exception.
   - Cost: it touches every `natural_language` query with an identifier
     token (the `mixed` pinned shape in the real-query manifest), so it needs
     its own measured change and reference comparison.
