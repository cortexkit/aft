---
title: "aft_search quality B2: shape router, lane-plan table, embedding-call counter, token variants, and readiness honesty"
date: 2026-09-09
status: draft
rigor_proposed: r3
---

## intent

B2 is the router-and-lane-plan part of the search-quality split. B1 owns the real-query manifest, gate, baseline, and re-measure; campaign A owns ranking, lane implementations, paging, confidence, and the executable search seam. B2 owns only a seven-shape product vocabulary, the table that selects already-owned lanes, deterministic token variants, readiness-aware degradation, and embedding-call observations. It does not own B1's population or campaign A's result ordering.

The measured population is the follow-up census's 6,469 discriminating episodes and its hand-labelled sample, not a new sample inferred from B2's router (`.alfonso/data/aft-search-followup-census/mechanism-report.md:5-9,13-21`). The five census strata and their weights are `identifier` 31.15%, `code_literal` 26.56%, `short` 12.35%, `nl` 28.13%, and `log_excerpt` 1.81% (`.alfonso/data/aft-search-followup-census/mechanism-report.md:23-31`). B1 freezes those census labels and the row population before B2 lands. B2 may report a product-shape drift, but it may not re-stratify, replace, or silently drop a B1 row.

The landed campaign-A registry is only an ordering registry at the evidence snapshot; it cannot execute a B2 callback. Therefore B2 consumes, rather than precedes, the A-owned seam extension. The authoritative order is **B1-baseline -> the stamped verify leaf `wi_e9aa228a` for `A-seam-extension-for-B2` -> every B2 slice**. A consumes nothing from B2; B2 consumes A's verified seam. Every B2 slice is `blocked_on` that exact verify-leaf stamp.

## constraints

**Evidence snapshot, immutable imports, and sources.** Every product-code statement is pinned to `origin/main` HEAD **`df52c5ee94c732f09a02d32ca1ead3c096872375`**. The two definition imports are content-addressed so moving sibling branches cannot change B2's meaning:

| Imported definition index | BLAKE3 of the supplied pinned bytes |
|---|---|
| `.cortexkit/alfonso/drafts/2026-09-09-aft-search-ranking-engine-a-comparator-exact-lane-anchored-fusion-paging-confidence-refire-1.md` | `77f87c9ad81c366f9764603a619ab5b36e0135500d613ca30391c1873dbc0c09` |
| `.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b1-real-query-benchmark-manifest-gate-baseline-and-remeasure.md` | `98dae2a85b533a1169ff5847f74cbd4932d5552d6acdc62eec64c329672748f1` |

The definition-import snapshot is the supplied worktree assembly at this draft's base SHA `b98805ad0df877f5bd66045876a0e8fae8b90a8b`; both digests were computed from the exact paths in that assembly. The A path is not in the `origin/main` code tree, so its content address, not a moving branch, is authoritative. The resolver rejects any byte mismatch. B1's ruling namespace is B1-local; only the exact family identities `exact-recall`, `concept-recall`, `real-query`, aggregate `search-quality`, and `imports-resolve` are imported here (`.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b1-real-query-benchmark-manifest-gate-baseline-and-remeasure.md:121-132`).

The read-only census sources are the canonical `.alfonso/data/aft-search-followup-census/summary.md:3-38`, `.alfonso/data/aft-search-followup-census/mechanism-report.md:3-164`, `.alfonso/data/aft-search-followup-census/labels.jsonl:1-300`, and `.alfonso/data/aft-search-followup-census/mechanisms.jsonl:1-6469`; the temporary mirror is only an access path and is never cited or written. Exact product facts at the code snapshot are:

| Fact at `df52c5ee94c732f09a02d32ca1ead3c096872375` | Exact evidence |
|---|---|
| `QueryKind` variants, verbatim and in order: `Identifier`, `Mixed`, `ErrorCode`, `Path`, `Regex`, `NaturalLanguage`. | `crates/aft/src/query_shape.rs:51-59` |
| `SearchShape` variants, verbatim and in order: `Identifier`, `Mixed`, `ErrorCode`, `Path`, `Regex`, `NaturalLanguage`. | `crates/aft/src/commands/semantic_search/plan_table.rs:9-19` |
| `SearchLaneKind` variants, verbatim and in order: `Exact`, `Lexical`, `Semantic`. | `crates/aft/src/commands/semantic_search/plan_table.rs:72-79` |
| The A s1 seam's `SearchLane` trait exposes `kind` and `plan_order_index`; `LaneRegistry` stores `Arc<dyn SearchLane>` by `SearchLaneKind`, and registration only stores and orders lanes. It has no execution callback. | `crates/aft/src/commands/semantic_search/mod.rs:17-55` |
| `handle_semantic_search` calls `strip_surrounding_quotes` at line 342 and only then calls `query_shape::classify` at line 349. | `crates/aft/src/commands/semantic_search/mod.rs:316-350` |
| Query-vector cache hits return before `embed_texts`; misses cross the `embed_texts` embedding-client boundary that the counter observes. | `crates/aft/src/semantic_index.rs:1189-1238` |
| Runtime semantic readiness is `SemanticIndexStatus::{Disabled, Building { stage, files, entries_done, entries_total }, Ready { refreshing, accounting }, Failed(String)}`. The lifecycle enum is in `context.rs`; `semantic_index.rs` defines the `SemanticIndex` data object rather than a second readiness enum. | `crates/aft/src/context.rs:928-949`; `crates/aft/src/semantic_index.rs:2101-2121` |
| The product's filename exemption is named `FILENAME_EXEMPTION_RE`; `check_path_exemption` consults it after the absolute/relative path forms. B2 cites the constant instead of copying an extension table. | `crates/aft/src/query_shape.rs:33-34,269-289` |
| The trigram/search-index enum is `IndexStatus::{Ready, Building, Fallback, Disabled}`. | `crates/aft/src/search_index.rs:945-951` |

The mechanism ranking remains the census ranking below; ownership follows the campaign split rather than moving the measured rows into B2 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:73-85`).

| Census rank | Mechanism | Projected episodes | Projected share | Owning response |
|---:|---|---:|---:|---|
| 1 | `not_a_search_failure` | 2,473 | 38.23% | no product change |
| 2 | `wrong_lane_nl` | 789 | 12.19% | B2 NL plan |
| 3 | `index_stale_or_missing` | 719 | 11.11% | B2 readiness |
| 4 | `topk_cut` | 552 | 8.54% | campaign A paging |
| 5 | `phrase_present_not_surfaced` | 545 | 8.43% | campaign A exact lane; B2 selects it |
| 6 | `renamed_or_variant_token` | 532 | 8.22% | B2 variants |
| 7 | `scope_mismatch` | 475 | 7.34% | caller correction, no ranking change |
| 8 | `other` | 284 | 4.38% | no single change |
| 9 | `identifier_not_definition_first` | 101 | 1.56% | A comparator plus B2 identifier plan |

The pinned six-variant enums are evidence, not B2's target vocabulary. Through the A-owned seam prerequisite, B2's deliverable replaces that pinned six-variant `SearchShape` design with the seven variants `Identifier`, `CodeLiteral`, `Short`, `NaturalLanguage`, `LogExcerpt`, `Path`, and `Regex`, whose serialized product labels are respectively `identifier`, `code_literal`, `short`, `nl`, `log_excerpt`, `path`, and `regex`. This is the seven-shape B2 design required by R31; B2 itself never edits the A plan table.

**Reference namespace and resolver closure (R20, R29, R45, R50).** This document's closed namespace is **R1-R51**. `R15b` is a subordinate anchor of imported A ruling R15: the parser accepts the spelling, normalizes it to R15, and never indexes a separate R15b record. B1's identically spelled rulings are B1-local and cannot satisfy B2 citations. Each B2 citation resolves to exactly one class: an A-defined import from the content-addressed A index above, a B2-local ruling, or a `consumer_requirement` naming an imported A or B1 gate. No imported symbol is also B2-local.

| Symbols | Resolver class and meaning in B2 |
|---|---|
| R1-R2 | B2-local: seven-shape routing; embedding counters and zero-call rows. |
| R3, R7-R9, R11-R18 | A-defined imports: comparator, paging, confidence, anchored grammar, depth/reference retrieval, per-run metadata, provenance, exact tier, bounded walk, and stability; `R15b` resolves to R15. |
| R4 | Consumer requirement on imported A exact ready/fallback gates. |
| R5-R6, R10, R13, R22-R27 | B2-local read-only consumer contracts for the pinned B1 families, manifest, shared floor, retrieval profile, quotas, modes, and outputs. |
| R19-R21, R28-R38 | B2-local: regex compatibility, resolver, plan totality, ownership/fence closure, A-seam dependency, variants, quote handling, readiness, no-match, reachability, and schedule. |
| R39-R41 | B2-local folded ownership, `SearchExtensions`, and one-call-site counter contracts. |
| R42-R44 | B2-local folded closed variant grammar, readiness retention/precedence, and per-lane seam suite. |
| R45-R46 | B2-local folded namespace/import identity and router lexical grammar. |
| R47 | B2-local ruling-round closure under R39-R46. |
| R48-R50 | B2-local folded `QueryFacts`, selected-lane disclosure, and concrete citation contracts. |
| R51 | B2-local rewrite-and-fire closure directive. |

`imports-resolve`, the B1-owned read-only gate imported above, scans the complete draft, inline test names, generated B2 ledger, and projected chair ledger before scoring. It verifies both sibling BLAKE3 pins and treats any reference outside R1-R51 as unresolved. It skips only its fixture directory and regions delimited by these markers:

<!-- imports-resolve:ignore -->
The deliberately unresolved negative symbol is `R99`.
<!-- imports-resolve:end -->

The projection renderer preserves those markers around copied negative examples. The B2 ledger has exactly one record for each R1-R51 identity and no record for R15b. Resolver goldens delete one local entry, double-classify one A import, alter either sibling byte digest, and expose the fenced negative symbol in a real test name; each fails before scoring and names the identity. The green baseline is: **`imports-resolve` exits 0 on this unmodified document with the code SHA and two BLAKE3 pins above**.

**Ownership, fences, and schedule (R24, R28, R30, R31, R34, R38-R41).** B2's root is literally outside A's `crates/aft/src/commands/semantic_search/**` glob. B2's exhaustive writable union is `crates/aft/src/search_b2/**`, `crates/aft/tests/search_b2_*.rs`, and `crates/aft/tests/fixtures/search_b2/**`; nothing else. Each concrete path belongs to exactly one slice:

| B2 slice | Sole writable paths | `blocked_on` |
|---|---|---|
| router-and-plan | `crates/aft/src/search_b2/mod.rs`; `crates/aft/src/search_b2/router.rs`; `crates/aft/src/search_b2/lane_plan.rs`; `crates/aft/tests/search_b2_router.rs`; `crates/aft/tests/search_b2_lane_plan.rs`; `crates/aft/tests/search_b2_seam.rs`; `crates/aft/tests/fixtures/search_b2/router/**`; `crates/aft/tests/fixtures/search_b2/lane_plan/**`; `crates/aft/tests/fixtures/search_b2/seam/**` | stamped verify leaf `wi_e9aa228a` |
| variants | `crates/aft/src/search_b2/variants.rs`; `crates/aft/tests/search_b2_variants.rs`; `crates/aft/tests/fixtures/search_b2/variants/**` | stamped verify leaf `wi_e9aa228a` |
| readiness | `crates/aft/src/search_b2/readiness.rs`; `crates/aft/tests/search_b2_readiness.rs`; `crates/aft/tests/fixtures/search_b2/readiness/**` | stamped verify leaf `wi_e9aa228a` |
| embedding-counter | `crates/aft/src/search_b2/embed_counter.rs`; `crates/aft/tests/search_b2_embed_counter.rs`; `crates/aft/tests/fixtures/search_b2/embed_counter/**` | stamped verify leaf `wi_e9aa228a` |

No module, public test, or fixture directory appears in another row. The audit asserts pairwise B2 disjointness and empty intersections with the imported A and B1 fences. It accepts only the assigned paths and rejects every B2 write under `crates/aft/src/commands/semantic_search/**`, every B2 write to `crates/aft/src/semantic_index.rs`, and every B2 write to `crates/aft/src/lib.rs`. It also keeps `query_shape.rs`, `context.rs`, `search_index.rs`, `embed/**`, `benchmarks/aft-search/**`, `scripts/telemetry/**`, and `.github/workflows/**` read-only. Read-only mutation controls run only in an ephemeral scratch worktree.

**Mandatory A-owned seam prerequisite (R31, R39-R41, R48).** The stamped `wi_e9aa228a` verify artifact must prove all of the following before any B2 slice starts; these are prerequisites, not claims about the code snapshot:

- In A-owned files, `SearchExtensions` has defaults reproducing current behavior and carries R31's lane callback `fn(&LaneInput) -> LaneOutput` plus `classify(&RawQuery) -> (Shape, QueryFacts)`, `plan(&Shape, &QueryFacts, &Readiness) -> LanePlan`, `sample_readiness(&Root) -> Readiness`, and `variants(&Token) -> Vec<Variant>`. `QueryFacts`, owned by A and filled once by B2 classification, contains exactly `embedded_span: Option<Span>` (the first matched quoted or backticked span inside NL, as byte offsets in the raw query), `exact_input_tokens: usize`, `has_path_token: bool`, and `has_timestamp_or_pid: bool`. No hidden request state substitutes for these values.
- A dispatch uses one `&dyn SearchExtensions` selected at configure time. Defaults are installed before B2 exists; B2 later supplies its implementation through A's generic registration hook. No A semantic-search module imports the B2 implementation. The only required top-level declaration is the A-owned line `pub mod search_b2;` in `crates/aft/src/lib.rs`.
- A extends the pinned `SearchShape` to B2's seven variants and `SearchLaneKind` to `Symbol`, `Exact`, `Anchored`, `Lexical`, `Variants`, `Semantic`, `PathLookup`, `FallbackWalk`, and `ReadinessDisclosure`; the callback executes the selected lane. Original pre-strip query text reaches `classify`; quote removal occurs only inside code-literal execution.
- The counter interface is `search_b2::embed_counter::{begin(request_id), record(request_id, calls), read(request_id)}`. Its one and only write call site is the single `record` line that `wi_e9aa228a` adds at the embedding request boundary in `semantic_index.rs`; the stamped commit records that line. B2 never edits `semantic_index.rs`, and adding a second B2-owned write there fails the fence audit on path.
- The A-alone gate installs defaults and keeps all 69 parity fixtures byte-identical; the hook-probe gate installs a probe and observes `classify`, `plan`, `sample_readiness`, `variants`, and the selected lane callback exactly once per request (`.cortexkit/alfonso/rulings/search-b2-r4.md:12-28`). With identical readiness, the actual extension interface must distinguish `nl/plain`, `nl/quoted-long`, and `nl/quoted-tiny` through `QueryFacts`; omitting any fact or bypassing the trait fails the probe (`.cortexkit/alfonso/rulings/search-b2-r5.md:3-9`).

**Read-only B1 benchmark interface (R5, R6, R10, R13, R22, R23, R25-R27).** B1 owns every benchmark and gate artifact. B2 only supplies a built binary to the B1 gate and reads its result.

- B1 stratifies on the five census labels and weights in `.alfonso/data/aft-search-followup-census/mechanism-report.md:23-31`. Each row also records the product classifier's authoring-time result as `pinned_shape`. At evaluation, B1 records `runtime_shape` and `shape_drift: true|false`; a drift is printed and scored on the existing row, never used to move the row, recompute quotas, or change the population. Thus B2 landing cannot change B1's denominator.
- The hand-labelled source is the fixed 300-row sample described at `.alfonso/data/aft-search-followup-census/mechanism-report.md:5-9`; `.alfonso/data/aft-search-followup-census/labels.jsonl:1-300` retains the labels and `.alfonso/data/aft-search-followup-census/mechanisms.jsonl:1-6469` retains the eligible episodes. These remain offline authoring inputs.
- File-level rank collapses repeated symbol results to each path's first position. `MRR@10` is `1/rank` when the opened file is among the first ten distinct paths and `0` otherwise; `hit@1` and `hit@5` are the corresponding binary indicators. Family, `pinned_shape`, and mechanism aggregates are unweighted row means; a census-weighted figure may be reported but is non-gating.
- The B1 shared floor remains unchanged: exact- and concept-recall families are flat-or-up on every metric at aggregate and fixture-group level; real-query aggregate `MRR@10` is flat-or-up; each `pinned_shape` present in the baseline may fall by no more than 0.01 absolute on `MRR@10`; each such shape's `hit@5` is flat-or-up. A ranking slice must additionally raise its targeted mechanism's `MRR@10` strictly without lowering its `hit@5`. A non-ranking slice must pass every declared binary fixture and the same shared floor. These are the sibling B1 contract at `.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b1-real-query-benchmark-manifest-gate-baseline-and-remeasure.md:121-132`; B2 neither renames nor redefines them.
- The baseline identity, public retrieval profile, quota arithmetic, fixture embedding packs, resolver implementation, gate modes, rubric, and post-release outputs remain B1-owned and read-only. B2 never creates an alternative baseline or gate.

**Seven-shape router and regex boundary (R1, R19, R31, R33, R46, R48, R50).** Empty trimmed input is `invalid_request`. Before shape precedence, the tokenizer applies this closed grammar in order:

1. An ISO-8601 date/time or `HH:MM:SS(.fff)?` is a `timestamp` token: variable in a log excerpt and literal elsewhere.
2. A bare integer of 2-7 digits adjacent to `pid`, `[`, `]`, or `#` is a variable `pid` token.
3. A token whose suffix matches `\w+\.[a-z0-9]{1,5}$` and is accepted by the product filename authority is a `path` token. The authority is cited, not copied: `FILENAME_EXEMPTION_RE` at `crates/aft/src/query_shape.rs:33-34,269-289`.
4. `\"` and `\'` are literal character pairs, never quote delimiters.
5. An apostrophe inside a word, as in `don't` or `user's`, belongs to the word.
6. A single or double quote opens a span only at a token boundary and only when a matching close exists. An unmatched quote is literal and cannot by itself make the query `code_literal`.

These rules are the closed R46 grammar (`.cortexkit/alfonso/rulings/search-b2-r4.md:60-68`); the extension authority is fixed by R50 (`.cortexkit/alfonso/rulings/search-b2-r5.md:20-25`). The tokenizer emits `QueryFacts` once; the first matching shape rule then wins:

| Precedence | Product shape | Predicate |
|---:|---|---|
| 0 | `regex` | The pinned whole-branch oracle says the existing regex route owns the query. Classification stops; B2 does not reinterpret it. |
| 1 | `log_excerpt` | At least three quote-aware whitespace tokens and a timestamp, a whole-token level `INFO\|WARN\|ERROR\|DEBUG\|TRACE\|panicked`, or a pid token. |
| 2 | `path` | Exactly one token and `has_path_token`. |
| 3 | `code_literal` | The whole query is one matched single- or double-quoted span; code punctuation `(){}[]<>=!;,\|&` or a backslash escape occurs outside spans; or at most three tokens contain a matched quoted span. |
| 4 | `identifier` | Exactly one token matching `[A-Za-z0-9_:.$#-]+`. |
| 5 | `nl` | At least four tokens. Embedded spans do not change ordinary surrounding prose to `code_literal`. |
| 6 | `short` | Every remaining non-empty query. |

Public-backend overlap goldens retain `foo::bar -> identifier`, `2026-09-08 ERROR open /tmp/a.rs failed -> log_excerpt`, `"exceeds the cap" -> code_literal`, `"ab" -> code_literal`, `'ab' -> code_literal`, `RunStarted { -> code_literal`, `if (x) return; -> code_literal`, `log "cap" here -> code_literal`, `why does it print "exceeds the cap" here -> nl`, `where do we cap the room name -> nl`, `character cap -> short`, `a b -> short`, and `src/main.rs -> path`. Four additional public overlap goldens close the tokenizer:

| Input | Literal expected shape | Property pinned |
|---|---|---|
| `why don't user's cache entries refresh` | `nl` | both apostrophes remain intra-word |
| `why does it print \"cap\" here` | `code_literal` | escaped quotes are not spans; the outside backslash escape selects code literal |
| `why does it print "cap here` | `nl` | unmatched quote is literal and does not select code literal |
| `2026-09-08T12:00:01 ERROR worker pid [4821] failed` | `log_excerpt` | timestamp and adjacent bracketed pid are variable log tokens |

The branch oracle is captured at the pinned code SHA and preserves `pre_tier_exempt` before `looks_like_regex`; compatibility covers every truth-value quadrant plus all matrix literals. Regex replies preserve result content, order, trailer, and existing keys; B2 adds only `plan.shape: "regex"`, the existing route's single `lanes_run` entry, and the three counters at zero. `^export` remains regex anchoring and `foo.*` remains regex text.

`SearchExtensions::classify` returns `(Shape, QueryFacts)` and `plan` receives `(&Shape, &QueryFacts, &Readiness)`. The NL matrix rows key on `QueryFacts.embedded_span.is_some()` and exact-mode rows key on `exact_input_tokens`; `has_path_token` supports the path rule and `has_timestamp_or_pid` supports log planning. The facts are not encoded as extra shape variants or recovered from hidden state.

**Exact input and mode (R4, R21, R33).** `exact_input(q)` is total and independent of the surrounding prose:

1. If the whole trimmed query is one matched quoted span, select it. Otherwise, when an `nl` plan runs exact because it contains a quote, select the first matched quoted span. Otherwise select no span and use the whole normalized query.
2. For a selected span, remove exactly one matching outer quote pair. Preserve every interior quote, apostrophe, hyphen, and whitespace character except the exact lane's later case-folding and whitespace-run collapse.
3. Tokenize the resulting phrase into maximal `[A-Za-z0-9_]` runs. `QueryFacts.exact_input_tokens` is the number of those runs whose length is at least three. Exact is in `ready` mode iff `trigram_ready` and `exact_input_tokens > 0`; otherwise it is in `fallback` mode. A shape whose plan omits exact reports `n/a`.

The literal derivation goldens are:

| Public/classifier input | Selected span | `exact_input` | Tokens | all-ready mode |
|---|---|---|---|---|
| `"ab"` | whole query | `ab` | `[ab]` | `fallback` |
| `'ab'` | whole query | `ab` | `[ab]` | `fallback` |
| `"a b c"` | whole query | `a b c` | `[a,b,c]` | `fallback` |
| `"exceeds the cap"` | whole query | `exceeds the cap` | `[exceeds,the,cap]` | `ready` |
| `why does it print "ab" here` | first embedded span | `ab` | `[ab]` | `fallback` |
| `character cap` | none | `character cap` | `[character,cap]` | `ready` |
| `parse_header` | none | `parse_header` | `[parse_header]` | `n/a` |
| `"doesn't cap the room's name"` | whole query | `doesn't cap the room's name` | `[doesn,t,cap,the,room,s,name]` | `ready` |

The first three are public-backend requests through `handle_semantic_search`, not classifier-only tests. They assert `shape == code_literal`, the literal `exact_input`, mode, and all embedding counters zero. Restoring quote stripping before classification makes the `"ab"` request reach the bare-token identifier/tiny path and reds the shape assertion before any matrix assertion.

**Plan table, counters, and readiness (R2, R21, R33, R35).** The retrieval-lane labels in `plan.lanes_run` are `symbol`, `exact`, `anchored`, `lexical`, `variants`, `semantic`, `path_lookup`, and `fallback_walk`. `readiness_disclosure` is an executable plan step but not a retrieval lane and therefore is recorded in `plan.executed_callbacks`, not `lanes_run`. Ready-state plans are:

| Shape | all-ready retrieval lanes | Semantic policy | Cold `embedding_calls` |
|---|---|---|---:|
| `identifier` | `symbol` (definition/prefix/reference), `lexical`, `variants` | never | 0 |
| `short` | `symbol` (prefix only), `exact`, `lexical`, `variants`, `semantic` | enabled | 1 |
| `code_literal` | `exact`, `lexical` | **never** | 0 |
| `nl` | `lexical`, `semantic`; add `exact` only for an embedded quoted span | enabled | 1 |
| `log_excerpt` | `anchored`, `lexical` | never | 0 |
| `path` | `path_lookup`, `lexical` | never | 0 |
| `regex` | existing regex route only | never | 0 |

The code-literal row is deliberately lexical/exact-only: whole-query `"ab"`, `'ab'`, and `"a b c"` make no cache lookup and no embedding-client call in cold or warm state. This is the R33 policy, not an exception patched into tests.

`plan.embedding_calls` counts calls crossing `SemanticEmbeddingModel::embed_texts` during the request; `plan.embedding_cache_hits` counts query-vector lookups served before that boundary; `plan.live_embed_calls` counts boundary calls served by a network- or model-backed provider. The table contains cold-cache expectations. On a repeated semantic-enabled query, the semantic lane still runs and the second reply reports `(embedding_calls, embedding_cache_hits) == (0,1)`. For `identifier`, `code_literal`, `log_excerpt`, `path`, `regex`, and any degraded row without semantic, both values stay `(0,0)` in cold and warm state. B1's fixture provider can make `embedding_calls` non-zero while `live_embed_calls` remains zero; B2 does not own that provider.

Readiness is a frozen resource-bearing observation, never a caller-supplied triple (R35, R43; `.cortexkit/alfonso/rulings/search-b2-r4.md:40-46`). Each false source chooses one reason by this precedence: `disabled` > `failed:<code>` > `evicted` > `lock_contention` > `building:<stage>`. The chosen reason appears in `plan.readiness`; lower-priority simultaneous conditions are not rendered.

| Bit | True predicate and retained resource | False-state inputs before precedence |
|---|---|---|
| `semantic_ready` | `SemanticIndexStatus::Ready`, including non-empty `refreshing`; retain the query path's cloned semantic snapshot `Arc`. | `Disabled`; `Failed(code)`; no loaded index after eviction; lock contention; `Building { stage }`. |
| `trigram_ready` | `IndexStatus::Ready`, including a live snapshot during refresh; retain its snapshot handle. | `Disabled`; `Fallback` as `failed:fallback`; no loaded snapshot after eviction; lock contention; `Building` as `building:trigram_index`. |
| `symbol_ready` | symbol cache loaded for the selected root; retain its cache snapshot handle. | configured off; loader failure code; discarded root entry; lock contention; load in progress as `building:symbol_cache`. |

All three are sampled once at request admission. Only when that sample is all-down does the existing bounded first-search wait run; at its end all three are sampled once more and only the second sample selects the row. The selected sample owns every observed ready snapshot handle until reply assembly completes, then releases it. There is no per-lane re-sampling. Therefore eviction after a ready sample cannot remove a selected lane: execution serves from the retained handle and emits no not-ready disclosure. An overlapping disabled/evicted fixture reports `disabled`. Public tests drive the real state enums and eviction/lock seams; they do not inject readiness booleans.

Given the retained observation, degradation is deterministic:

- `symbol`, `lexical`, `variants`, and `semantic` run only when both the legal shape/facts row selects them and their source is ready. A skipped semantic lane performs no cache lookup.
- `exact` remains selected when the shape/facts row wants it. It uses the ready predicate above or delegates to A's deterministic bounded walk and its exact `exact pass: bounded (N files, <reason>)` contract; imported reasons are `file limit`, `result limit`, `index not ready`, and watchdog `time limit`. The watchdog suffix and stability behavior remain A-owned.
- `anchored` remains selected for its row and uses A's bounded walk when trigram is unavailable. `path_lookup` is index-free and always runs for `path`.
- If degradation leaves no retrieval lane, `fallback_walk` runs. No observed row has an empty `lanes_run`; that is not a promise of a match.

**Selected-lane disclosure (R49; `.cortexkit/alfonso/rulings/search-b2-r5.md:11-18`).** A not-ready source produces a footer only when the shape/facts legal plan would have used its lane. An unavailable source irrelevant to the request is silent. After any results, present applicable lines once each in this fixed order and exact text:

1. `trigram index <reason>; lexical lane skipped`
2. `symbol cache <reason>; definition-first skipped`
3. `semantic index <reason>; semantic lane skipped`
4. `variants applied: <list>`
5. `fallback walk: <n> files scanned`

The variants line appears only for variants that contributed admitted results, in generation order; the fallback line appears only when that callback ran. A shared unavailable trigram source produces the single trigram line, not a second variants-unavailable message. Public goldens are literal: path query plus semantic down has no readiness footer; identifier plus only symbol cache down has only `symbol cache disabled; definition-first skipped`; NL plus only trigram down has only `trigram index disabled; lexical lane skipped`. They also assert exact callback sets.

The matrix is the full product of the eight readiness triples and these ten literal query classes: `identifier/long` = `parse_header`; `short/long` = `character cap`; `short/tiny` = `a b`; `code_literal/long` = `"exceeds the cap"`; `code_literal/tiny` = `"ab"`; `nl/plain` = `where do we cap the room name`; `nl/quoted-long` = `why does it print "exceeds the cap" here`; `nl/quoted-tiny` = `why does it print "ab" here`; `log_excerpt/std` = `2026-09-08 ERROR open /tmp/a.rs failed`; `path/std` = `src/main.rs`. The independently authored 80-row expectation first proves `(snapshot branch, shape, QueryFacts, exact presence, exact_input, token list, all-ready mode)` through the public backend, then asserts exact `lanes_run`, callbacks, counters, mode, retained-readiness reason, and footer lines.

**Ordered token variants (R32, R42; `.cortexkit/alfonso/rulings/search-b2-r4.md:30-38`).** Variants run only for `identifier` and `short`, whether or not a definition was found, and only while trigram is ready. They never invoke the embedding cache or client. The closed pipeline is:

1. For each whitespace token in token order, parse words only at `_`, `-`, `.`, `/`, lower-to-upper boundaries, and upper-to-upper-lower boundaries. Thus `HTTPServer` is `[HTTP,Server]`, never `[H,T,T,P,Server]`. Case conversion preserves a leading `#`.
2. Emit `snake_case`, `kebab-case`, `camelCase`, `PascalCase`, then `SCREAMING_SNAKE`. Exclude the original token and prior duplicates.
3. Toggle number on the last parsed word only, preserving the rest of the token. Apply these rules in order: (a) `status`, `bus`, `class`, `process`, `address`, `analysis`, `basis`, `axis`, `alias`, `canvas`, `focus`, and `lens` are singular exceptions; pluralize by `es` (or `es`→`ses` for `-sis`); (b) otherwise treat a word ending in `s` as plural and singularize by `ies`→`y`, `es`→`` when the stem ends in `s`, `x`, `z`, `ch`, or `sh`, else `s`→``; (c) otherwise pluralize by `y`→`ies` after a consonant, `es` after `s`, `x`, `z`, `ch`, or `sh`, else `s`. Emit the changed token once unless duplicate.
4. If the token matches `^#?[0-9a-f]{6,}[_-]`, emit the unmatched remainder unless duplicate.
5. Concatenate each token's emissions in token order, de-duplicate by first emission, and retain the first six outputs across the whole query, not six per token.

Literal ordered goldens are:

| Input | Full relevant derivation | Final ordered output |
|---|---|---|
| `HTTPServer` | five case forms, then pluralize the last word | `[http_server, http-server, httpServer, HttpServer, HTTP_SERVER, HTTPServers]` |
| `items` | unchanged lower forms are excluded; Pascal and screaming precede singularization | `[Items, ITEMS, item]` |
| `#abc123_foo` | snake equals input; hash remainder follows number toggle | `[#abc123-foo, #abc123Foo, #Abc123Foo, #ABC123_FOO, #abc123_foos, foo]` |
| `#abc123_HTTPServer` | five case forms and number toggle fill the cap before the seventh hash remainder | `[#abc123_http_server, #abc123-http-server, #abc123HttpServer, #Abc123HttpServer, #ABC123_HTTP_SERVER, #abc123_HTTPServers]` |
| `status` | singular exception, then `es` pluralization | `[Status, STATUS, statuses]` |
| `character cap` | process `character` before `cap`; the query-wide cap keeps both complete three-item groups | `[Character, CHARACTER, characters, Cap, CAP, caps]` |

A direct rule golden additionally asserts `key -> keys`, preventing unconditional `y -> ies`; the `character cap` golden proves query-wide concatenation and truncation.

The reply prints only variants that contributed an admitted result, in generation order. The honest no-match condition is that the original token and **every generated variant** are absent; it never assumes every input produces six variants. Within candidates equal on imported comparator fields R3(1)-(4), an original-form hit precedes a variant hit by R3(5). Across evidence classes, A's comparator remains authoritative; B2 changes no comparator field.

**No-match and readiness-relative reachability (R36, R37).** Reachability is an independent fixture oracle, never inferred from `lanes_run` or from the observed result. For a specific fixture and observed `(semantic_ready, trigram_ready, symbol_ready)` triple, the only oracle values are `reachable_via:<lane>`, `unreachable`, and `fallback_walk`. The last-resort promise is exactly: **if the oracle says `reachable_via:<lane>` for the observed triple, the result is non-empty and the admitted result records that lane**. `fallback_walk` means the bounded walk is the expected mechanism; its fixture separately says whether the match is within or beyond the bound. There is no universal reachability claim.

The table below is exhaustive for every B2 fixture in this draft that makes a match-reachability claim. Structural classifier, counter-only, resolver, and fence goldens make no match claim and therefore do not have an oracle row. Grouped episode ids share one declared corpus mechanism and one row.

| Fixture(s) | 111 | 110 | 101 | 100 | 011 | 010 | 001 | 000 |
|---|---|---|---|---|---|---|---|---|
| definition identifier and episodes 14708/17879/20043 | `reachable_via:symbol` | `reachable_via:lexical` | `reachable_via:symbol` | `fallback_walk` | `reachable_via:symbol` | `reachable_via:lexical` | `reachable_via:symbol` | `fallback_walk` |
| variant-only identifier and episodes 5985/13820/20065 | `reachable_via:variants` | `reachable_via:variants` | `unreachable` | `fallback_walk` | `reachable_via:variants` | `reachable_via:variants` | `unreachable` | `fallback_walk` |
| lexical-only NL episodes 4173/2299/1776 | `reachable_via:lexical` | `reachable_via:lexical` | `unreachable` | `unreachable` | `reachable_via:lexical` | `reachable_via:lexical` | `fallback_walk` | `fallback_walk` |
| semantic-only NL seam fixture | `reachable_via:semantic` | `reachable_via:semantic` | `reachable_via:semantic` | `reachable_via:semantic` | `unreachable` | `unreachable` | `fallback_walk` | `fallback_walk` |
| long exact fixtures (`character cap`, phrase episodes 667/7617/15364, quoted-long classes, beyond-bound control) | `reachable_via:exact` | `reachable_via:exact` | `fallback_walk` | `fallback_walk` | `reachable_via:exact` | `reachable_via:exact` | `fallback_walk` | `fallback_walk` |
| tiny exact public fixtures (`"ab"`, `'ab'`, `"a b c"`, `a b`, quoted-tiny NL) | `fallback_walk` | `fallback_walk` | `fallback_walk` | `fallback_walk` | `fallback_walk` | `fallback_walk` | `fallback_walk` | `fallback_walk` |
| anchored log fixture | `reachable_via:anchored` | `reachable_via:anchored` | `fallback_walk` | `fallback_walk` | `reachable_via:anchored` | `reachable_via:anchored` | `fallback_walk` | `fallback_walk` |
| path-lookup fixture `src/main.rs` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` | `reachable_via:path_lookup` |
| original-token fallback identifier | `reachable_via:lexical` | `reachable_via:lexical` | `unreachable` | `fallback_walk` | `reachable_via:lexical` | `reachable_via:lexical` | `unreachable` | `fallback_walk` |
| no-match identifier | `unreachable` | `unreachable` | `unreachable` | `fallback_walk` | `unreachable` | `unreachable` | `unreachable` | `fallback_walk` |
| empty selected-root path fixture | `unreachable` | `unreachable` | `unreachable` | `unreachable` | `unreachable` | `unreachable` | `unreachable` | `unreachable` |

Column labels are the observed bits in `(semantic, trigram, symbol)` order. For the beyond-bound control, the `fallback_walk` cells assert an honest empty result and the bound disclosure; for tiny public fixtures, the corpus places the match within the bound and the result is non-empty. The variant-only identifier with trigram down and symbols up is `unreachable`: its non-matching symbol lane prevents last resort, so it returns the no-match line plus the trigram readiness disclosure.

When all planned retrieval lanes ran and found nothing in an all-ready state, the reply carries exactly one line, `no match for <shape>: <n> lanes searched (<lane list>)`, and none of the readiness, contributed-variants, or bounded-walk disclosures. The all-ready no-match identifier therefore says `no match for identifier: 3 lanes searched (symbol, lexical, variants)` and nothing else. In a degraded `unreachable` row, R37 narrows that rule: the same single no-match line may be accompanied by the required readiness reason, but never by a claim that an unavailable lane ran. Empty selected roots additionally carry their existing empty-scope statement. No fixture fabricates a nearest neighbour.

## acceptance sketch

- **Draft and source integrity.** Front matter is delimited by `---`; exactly the required five `##` sections are present; the file is below 80 KB. Code citations resolve at `df52c5ee94c732f09a02d32ca1ead3c096872375`; both sibling BLAKE3 pins reproduce; census weights reproduce `.alfonso/data/aft-search-followup-census/mechanism-report.md:23-31` and its complete five-stratum population.
- **Resolver closure.** The complete unmodified draft, inline tests, generated B2 ledger, and projected chair ledger resolve uniquely through R1-R51 before scoring; R15b normalizes to R15 and has no record. Removing a local entry, double-classifying an A import, changing either sibling digest, exposing the fenced negative symbol, or stripping ignore markers fails and names the identity.
- **Fence and dependency audit.** Every implementing diff belongs to exactly one concrete B2 module/test/fixture assignment above; slice intersections and A/B1 intersections are empty. Every descriptor names `blocked_on: wi_e9aa228a` and its stamped verify artifact. Any B2 write under `commands/semantic_search/**`, to `semantic_index.rs`, `lib.rs`, or another read-only A/B1 path fails; a synthetic second counter write in `semantic_index.rs` is rejected on path.
- **Executable seam request suite (R44; `.cortexkit/alfonso/rulings/search-b2-r4.md:48-52`).** First, the prerequisite's A-alone default run keeps its 69 parity fixtures byte-identical and its probe observes every extension hook exactly once. Then separate requests through `handle_semantic_search` focus each retrieval callback: `symbol` via a definition identifier; `exact` via `character cap`; `anchored` via a log excerpt; `lexical` via plain NL; `variants` via a variant-only identifier; `semantic` via a semantic-only NL fixture; `path_lookup` via `src/main.rs`; and `fallback_walk` via an all-down identifier. A trigram-down/symbol-up variant fixture focuses `readiness_disclosure`. Each request asserts its full legal matrix row and exact callback set, and that its focal callback ran once. A lane with no legal selecting request fails the suite. Suppressing any registered callback fails its corresponding public test; B2 tests never patch A files.
- **Seven-shape and regex goldens.** Every precedence overlap and the four literal apostrophe/escaped/unmatched/timestamp-plus-pid cases assert their expected seven-shape result through the public backend and the `FILENAME_EXEMPTION_RE` citation stays resolvable. The branch oracle remains pointwise identical, including pre-tier-exempt/regex overlaps. `^export` and `foo.*` remain on the existing regex route with preserved results/keys and zero counters. Reversing precedence, treating unmatched quotes as spans, or routing regex through B2 fails.
- **Whole-query quote control.** Public requests `"ab"`, `'ab'`, and `"a b c"` assert `code_literal`, the literal inner `exact_input`, fallback mode, `lanes_run == {exact, lexical}` when trigram is ready, and `(embedding_calls, embedding_cache_hits, live_embed_calls) == (0,0,0)` cold and warm. Restoring strip-before-classify reds `"ab"` on shape. Enabling semantic for any code-literal row reds all affected counter and lane assertions.
- **Matrix totality and facts seam.** The independent 80-row golden covers every declared class and readiness triple. It compares the runtime row, not the derivation function, and checks `QueryFacts`, lanes, callbacks, counters, exact mode, precedence-selected reason, and exact footer lines. With identical readiness, public dispatch distinguishes NL/plain, NL/quoted-long, and NL/quoted-tiny through the actual `plan(&Shape, &QueryFacts, &Readiness)` hook. Enabling semantic for identifier/code-literal, retaining variants while trigram is down, removing last resort, deriving exact mode from surrounding prose, or dropping a `QueryFacts` field fails affected rows.
- **Readiness retention, precedence, and disclosure.** Public fixtures exercise semantic `Disabled`, `Building { stage: loading_artifacts }`, `Ready` with empty/non-empty `refreshing`, and `Failed`; trigram `Ready`, `Building`, `Fallback`, and `Disabled`; symbol loaded/unloaded; eviction and lock contention. Overlapping disabled/evicted reports `disabled`. A transition during all-down wait proves the second sample selects the plan. Eviction after a ready sample still serves through the retained snapshot until reply, keeps the row/counters fixed, and emits no not-ready line. Literal selected-lane goldens prove path+semantic-down is silent, identifier+symbol-down prints only the symbol line, and NL+trigram-down prints only the trigram line in footer order.
- **Embedding boundary.** Definition-present/absent identifiers, every code literal, log excerpt, path, regex, and semantic-disabled row assert no cache lookup and no boundary call cold and warm. Semantic-enabled short/NL controls assert one cold boundary call and a warm cache hit without dropping `semantic`. The request-id counter is read from `search_b2::embed_counter`; only the A-owned `semantic_index.rs` boundary records it. Inferring calls from lane labels or adding a second record site fails the cache/fence controls.
- **Variant pipeline.** All six ordered arrays compare literally. `status` defends the exception list; `character cap` defends token-order concatenation and the shared six-output cap; `key -> keys` defends consonant-`y`; the original four retain acronym, singularization, hash, and truncation coverage. Definition-present/absent identifiers both execute variants when trigram is ready. The footer lists only contributing variants in generation order. Reordering, splitting an acronym by letter, toggling a non-final word, omitting SCREAMING_SNAKE, stripping every hash, or capping per token fails a literal golden.
- **Reachability and honest zero.** Every match-bearing B2 fixture uses the oracle row above before checking results. Every `reachable_via` cell produces a non-empty result with that lane recorded. `unreachable` and beyond-bound cells may be empty and must not fabricate a row. The all-ready no-match identifier carries only the exact R36 line; the trigram-down/symbol-up variant fixture carries that line plus the trigram reason; the empty-root fixture carries its empty-scope statement. A mutation treating non-empty `lanes_run` as proof of reachability or returning a nearest neighbour reds these assertions.
- **Measured mechanism checks through B1.** The NL lexical change targets episodes 4173/2299/1776 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:95-99`); definition-first checks episodes 14708/17879/20043 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:101-105`); variants check episodes 5985/13820/20065 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:119-123`); readiness checks the index-stale fixtures (`.alfonso/data/aft-search-followup-census/mechanism-report.md:125-129`). B1 scores the unchanged rows under `pinned_shape`; B2 must meet B1's existing gate and may not rewrite expected metrics or population after seeing its result.
- **B1 population immutability.** A golden runs the same B1 manifest before and after a deliberately changed B2 classification: row ids, census strata, quotas, denominator, and `pinned_shape` stay byte-identical; only `runtime_shape` and the drift report change. Mutation control re-stratifies on `runtime_shape`; the population digest reds.
- **Imported A behavior remains read-only.** Exact and anchored retrieval, comparator ordering, fusion, paging, confidence, bounded-walk limits, and provenance remain in the content-addressed A definition index. B2 public tests may assert outputs as consumer evidence but no B2 slice writes their implementation. Import closure requires an A gate identity for every such assertion and the pinned A digest.

## non-goals

- B1's manifest construction, corpus and embedding packs, quality-gate implementation, baseline recording, quota arithmetic, confidence calibration, or post-release re-measure.
- Campaign A's comparator, exact/anchored/lexical/semantic implementations, fusion, paging, confidence, provenance, dispatch, plan table, or public agent surfaces.
- A new BM25/body index, a new symbol index, a new embedding provider/model/dimension, or any network-backed CI dependency.
- Any agent-facing lane or hint parameter; any change to `grep`, regex semantics, `topK`, project-root selection, or `includeTests`.
- A guarantee that every on-disk match is reachable in every readiness state; reachability is fixture-and-state specific under R37.

## open_questions

- none: ownership, namespace closure, B1 population stability, seam dependency, quote handling, variant order, readiness observation, no-match text, and reachability are fixed above.
