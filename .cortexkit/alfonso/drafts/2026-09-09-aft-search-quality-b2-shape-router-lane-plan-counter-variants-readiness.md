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

**Evidence snapshot and sources.** All code statements in this draft are pinned to `origin/main` HEAD **`df52c5ee94c732f09a02d32ca1ead3c096872375`**. The read-only census sources are the canonical `.alfonso/data/aft-search-followup-census/summary.md:3-38`, `.alfonso/data/aft-search-followup-census/mechanism-report.md:3-164`, `.alfonso/data/aft-search-followup-census/labels.jsonl:1-300`, and `.alfonso/data/aft-search-followup-census/mechanisms.jsonl:1-6469`; the temporary read-only mirror is only an access path and is never cited or written. The source facts that constrain implementation are:

| Fact at the pinned SHA | Exact evidence |
|---|---|
| `QueryKind` has the variants `Identifier`, `Mixed`, `ErrorCode`, `Path`, `Regex`, `NaturalLanguage`, in that order. | `crates/aft/src/query_shape.rs:51-59` |
| `SearchShape` has the variants `Identifier`, `Mixed`, `ErrorCode`, `Path`, `Regex`, `NaturalLanguage`, in that order. | `crates/aft/src/commands/semantic_search/plan_table.rs:9-19` |
| `SearchLaneKind` has the variants `Exact`, `Lexical`, `Semantic`, in that order. | `crates/aft/src/commands/semantic_search/plan_table.rs:72-79` |
| `SearchLane` is a trait exposing `kind` and `plan_order_index`; `LaneRegistry` stores `Arc<dyn SearchLane>` by kind and registration only stores and orders lanes. There is no execution callback. | `crates/aft/src/commands/semantic_search/mod.rs:17-55` |
| `handle_semantic_search` strips one surrounding quote pair before calling `query_shape::classify`. | `crates/aft/src/commands/semantic_search/mod.rs:316-350` |
| Query-vector cache hits return before `embed_texts`; misses call `embed_texts`, which is the embedding-client boundary B2 counts. | `crates/aft/src/semantic_index.rs:1189-1238` |
| The runtime semantic readiness enum is `SemanticIndexStatus::{Disabled, Building {..}, Ready { refreshing, accounting }, Failed(String)}`. It is declared in `context.rs`; `semantic_index.rs` contains the index and embedding implementation, not a second readiness enum. | `crates/aft/src/context.rs:928-949` |
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

The pinned six-variant enums are evidence, not B2's target vocabulary. Under the A-owned seam prerequisite, the A plan table is extended to the B2-required seven `SearchShape` variants `Identifier`, `CodeLiteral`, `Short`, `NaturalLanguage`, `LogExcerpt`, `Path`, and `Regex`, whose serialized product labels are respectively `identifier`, `code_literal`, `short`, `nl`, `log_excerpt`, `path`, and `regex`. This is the seven-shape B2 design required by R31; B2 itself never edits the A plan table.

**Reference namespace and resolver closure (R20, R29).** This document's closed namespace is **R1-R38**. B1's identically spelled references are B1-local and never satisfy a B2 citation. Each B2 citation resolves to exactly one of: an A-defined import in the pinned A definition index, a B2-local ruling in the B2 ledger, or a `consumer_requirement` that names its imported A gate. No A-defined symbol is also entered as B2-local.

| Symbols | Resolver class and meaning in this document |
|---|---|
| R1 | B2-local: router precedence and seven-shape labels. |
| R2 | B2-local: the three embedding counters and zero-call rows. |
| R3 | A-defined import: total comparator and field order. |
| R4 | `consumer_requirement`: exact-lane ready/fallback modes, depending on A's R16/R17 gates. |
| R5-R6 | B2-local consumer contract for invoking B1's gate and interpreting its decision predicate; B2 does not implement it. |
| R7, R8, R9 | A-defined imports: paging, confidence, and anchored grammar. |
| R10 | B2-local read-only contract for B1's pinned manifest inputs. |
| R11-R12 | A-defined imports: depth tiers and reference retrieval. |
| R13 | B2-local read-only contract for B1's shared quality floor. |
| R14-R18, including R15b | A-defined imports: per-run stop metadata, provenance, score-free exact results, deterministic bounded walk, and stability units. |
| R19 | B2-local: snapshot regex-branch compatibility and reply preservation. |
| R20 | B2-local: this resolver and three-class namespace. |
| R21 | B2-local: plan totality over query classes and readiness states. |
| R22-R23 | B2-local read-only requirements on B1's public retrieval profile and quota totality. |
| R24 | B2-local: one B2 owner per writable path and no shared B2 path. |
| R25, R26, R27 | B2-local read-only requirements on B1's gate modes and re-measure destinations. |
| R28 | B2-local: A/B2, B1/B2, and B2-internal fence disjointness. |
| R29 | B2-local: closure through R38, fenced negative payloads, and the green baseline. |
| R30 | B2-local: only new `b2_*.rs` modules plus the counter region may be written. |
| R31 | B2-local consumer requirement on the A-owned executable seam extension. |
| R32 | B2-local: ordered token-variant pipeline and literal goldens. |
| R33 | B2-local: whole-query quotes reach classification and code literals embed nothing. |
| R34 | B2-local: every B2 slice blocks on the seam verify-leaf stamp. |
| R35 | B2-local: readiness mapping, reasons, sampling, and re-sampling. |
| R36 | B2-local: exact no-match statement. |
| R37 | B2-local: fixture-by-state reachability oracle. |
| R38 | B2-local: B1-baseline then A seam verification then B2. |

`imports-resolve`, owned by B1 and consumed read-only here, scans this complete draft, B2's inline test names, the generated B2 ruling ledger, and the projected chair ledger before any score runs. It skips its fixture directory and regions delimited by the following markers. Every deliberately unresolved payload, including a copy rendered into the projected chair ledger, must be inside such a region:

<!-- imports-resolve:ignore -->
The deliberately unresolved negative symbol is `R99`.
<!-- imports-resolve:end -->

The projection renderer must preserve those markers around copied negative examples; a projection containing an unfenced unresolved symbol is a resolver failure. The B2 resolver ledger contains exactly one index record for every symbol R1-R38. An A-defined record is a reference-only pointer into the pinned A index, not a second B2-local definition; local and consumer-requirement records name the defining bullet above. Resolver goldens delete one local entry, duplicate one A import into the B2-local class, and inject the fenced negative symbol into a real test name outside an ignore region; each fails before scoring and names the symbol. The single green-baseline statement is: **`imports-resolve` exits 0 on this unmodified document at the pinned sha**.

**Ownership, fences, and schedule (R24, R28, R30, R31, R34, R38).** B2 has no shared-serialized path. Inline tests live with their owning B2 module. The complete writable union and dependency are:

| B2 slice | Sole writable fence | `blocked_on` |
|---|---|---|
| router-and-plan | new `crates/aft/src/commands/semantic_search/b2_router.rs` and `b2_lane_plan.rs` | stamped verify leaf `wi_e9aa228a` |
| variants | new `crates/aft/src/commands/semantic_search/b2_variants.rs` | stamped verify leaf `wi_e9aa228a` |
| readiness | new `crates/aft/src/commands/semantic_search/b2_readiness.rs` | stamped verify leaf `wi_e9aa228a` |
| embedding-counter | only the embedding-counter region of `crates/aft/src/semantic_index.rs` | stamped verify leaf `wi_e9aa228a` |

No path appears in two rows. The union-of-fences audit asserts `A-fence ∩ B2-fence = ∅`, `B1-fence ∩ B2-fence = ∅`, and pairwise-disjoint B2 slice fences. B2 treats all of these as **read-only**: `crates/aft/src/query_shape.rs`; `crates/aft/src/commands/semantic_search/mod.rs`; `plan_table.rs`; `lexical_lane.rs`; `exact_lane.rs`; `anchored_lane.rs`; comparator modules; `crates/aft/src/embed/**`; `crates/aft/src/lib.rs`; all `benchmarks/aft-search/**`; `scripts/telemetry/**`; and `.github/workflows/**`. B2 also has no write authorization for a pre-split search monolith. Mutations of read-only A code run only in an ephemeral scratch worktree and never enter a B2 diff.

The A-owned `A-seam-extension-for-B2` prerequisite changes A files before B2 starts. It extends `SearchLaneKind` from the pinned `Exact | Lexical | Semantic` set to cover `Symbol`, `Exact` (the phrase pass), `Anchored`, `Lexical`, `Variants`, `Semantic`, `PathLookup`, `FallbackWalk`, and `ReadinessDisclosure`; adds a registration entry carrying `fn(&LaneInput) -> LaneOutput`; makes `mod.rs`'s single dispatch execute that callback; extends `SearchShape` to the seven variants above; and passes original, pre-quote-strip query text into classification, moving quote removal inside the `code_literal` execution branch. Its stamp is a precondition, not a claim that these changes exist at the pinned snapshot.

**Read-only B1 benchmark interface (R5, R6, R10, R13, R22, R23, R25-R27).** B1 owns every benchmark and gate artifact. B2 only supplies a built binary to the B1 gate and reads its result.

- B1 stratifies on the five census labels and weights in `.alfonso/data/aft-search-followup-census/mechanism-report.md:23-31`. Each row also records the product classifier's authoring-time result as `pinned_shape`. At evaluation, B1 records `runtime_shape` and `shape_drift: true|false`; a drift is printed and scored on the existing row, never used to move the row, recompute quotas, or change the population. Thus B2 landing cannot change B1's denominator.
- The hand-labelled source is the fixed 300-row sample described at `.alfonso/data/aft-search-followup-census/mechanism-report.md:5-9`; `.alfonso/data/aft-search-followup-census/labels.jsonl:1-300` retains the labels and `.alfonso/data/aft-search-followup-census/mechanisms.jsonl:1-6469` retains the eligible episodes. These remain offline authoring inputs.
- File-level rank collapses repeated symbol results to each path's first position. `MRR@10` is `1/rank` when the opened file is among the first ten distinct paths and `0` otherwise; `hit@1` and `hit@5` are the corresponding binary indicators. Family, `pinned_shape`, and mechanism aggregates are unweighted row means; a census-weighted figure may be reported but is non-gating.
- The B1 shared floor remains unchanged: exact- and concept-recall families are flat-or-up on every metric at aggregate and fixture-group level; real-query aggregate `MRR@10` is flat-or-up; each `pinned_shape` present in the baseline may fall by no more than 0.01 absolute on `MRR@10`; each such shape's `hit@5` is flat-or-up. A ranking slice must additionally raise its targeted mechanism's `MRR@10` strictly without lowering its `hit@5`. A non-ranking slice must pass every declared binary fixture and the same shared floor. These are the sibling B1 contract at `.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b1-real-query-benchmark-manifest-gate-baseline-and-remeasure.md:29-44,47-57`; B2 neither renames nor redefines them.
- The baseline identity, public retrieval profile, quota arithmetic, fixture embedding packs, resolver implementation, gate modes, rubric, and post-release outputs remain B1-owned and read-only. B2 never creates an alternative baseline or gate.

**Seven-shape router and regex boundary (R1, R19, R31, R33).** Empty trimmed input is `invalid_request`. Otherwise the first matching rule wins:

| Precedence | Product shape | Predicate |
|---:|---|---|
| 0 | `regex` | The pinned whole-branch oracle says the existing regex route owns the query. Classification stops; B2 does not reinterpret it. |
| 1 | `log_excerpt` | At least three quote-aware whitespace tokens and either a date/time token, a whole-token level `INFO\|WARN\|ERROR\|DEBUG\|TRACE\|panicked`, or a pid-shaped bracketed integer. |
| 2 | `path` | Exactly one token containing `/` and a dotted extension. |
| 3 | `code_literal` | The whole query is one matched single- or double-quoted span; or code punctuation `(){}[]<>=!;,\|&` or a backslash escape occurs outside quoted spans; or at most three tokens contain a quoted span. |
| 4 | `identifier` | Exactly one token matching `[A-Za-z0-9_:.$#-]+`. |
| 5 | `nl` | At least four tokens. Embedded quoted spans do not change this to `code_literal` when ordinary prose outside the span is the only other content. |
| 6 | `short` | Every remaining non-empty query. |

Quote-aware tokenization keeps a matched quoted span together. Public-backend overlap goldens pin `foo::bar -> identifier`, `2026-09-08 ERROR open /tmp/a.rs failed -> log_excerpt`, `"exceeds the cap" -> code_literal`, `"ab" -> code_literal`, `'ab' -> code_literal`, `RunStarted { -> code_literal`, `if (x) return; -> code_literal`, `log "cap" here -> code_literal`, `why does it print "exceeds the cap" here -> nl`, `where do we cap the room name -> nl`, `character cap -> short`, `a b -> short`, and `src/main.rs -> path`. The branch oracle is captured at the pinned SHA and preserves the snapshot's `pre_tier_exempt`-before-`looks_like_regex` decision. The compatibility golden covers every truth-value quadrant of those predicates plus all matrix literals. Regex replies preserve result content, order, trailer, and existing keys; B2 adds only `plan.shape: "regex"`, the existing route's single `lanes_run` entry, and the three counters at zero. `^export` still exercises regex anchoring, and `foo.*` still matches regex text rather than a literal identifier.

**Exact input and mode (R4, R21, R33).** `exact_input(q)` is total and independent of the surrounding prose:

1. If the whole trimmed query is one matched quoted span, select it. Otherwise, when an `nl` plan runs exact because it contains a quote, select the first matched quoted span. Otherwise select no span and use the whole normalized query.
2. For a selected span, remove exactly one matching outer quote pair. Preserve every interior quote, apostrophe, hyphen, and whitespace character except the exact lane's later case-folding and whitespace-run collapse.
3. Tokenize the resulting phrase into maximal `[A-Za-z0-9_]` runs. Exact is in `ready` mode iff `trigram_ready` and at least one such token has length at least three; otherwise it is in `fallback` mode. A shape whose plan omits exact reports `n/a`.

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

Readiness is a frozen observation, not a caller-supplied triple. Each false source carries exactly one of `disabled`, `failed:<code>`, `building:<stage>`, `evicted`, or `lock_contention` into the corresponding disclosure and structured `plan.readiness` member.

| Bit | True predicate | False mapping |
|---|---|---|
| `semantic_ready` | `SemanticIndexStatus::Ready`, including a non-empty `refreshing` set because queries serve the live snapshot. | `Disabled -> disabled`; `Building { stage } -> building:<stage>`; `Failed(code) -> failed:<code>`; no loaded index after eviction -> `evicted`; lock contention -> `lock_contention`. |
| `trigram_ready` | `IndexStatus::Ready`, including a live Ready snapshot while files refresh. | `Disabled -> disabled`; `Building -> building:trigram_index`; `Fallback -> failed:fallback`; no loaded snapshot after eviction -> `evicted`; lock contention -> `lock_contention`. |
| `symbol_ready` | the symbol cache is loaded for the selected root. | configured off -> `disabled`; loader failure code -> `failed:<code>`; load in progress -> `building:symbol_cache`; discarded root entry -> `evicted`; lock contention -> `lock_contention`. |

All three are sampled once at request admission. If and only if that sample is all-down, the existing bounded first-search wait runs; when it ends, all three are sampled exactly once more, and only the second sample selects the matrix row, counters, and disclosure. There is no later per-lane re-sampling. Public-backend readiness tests drive the real state enums through a test seam; no test injects booleans directly.

Given the observed triple, plan degradation is deterministic:

- `symbol` runs only when its shape row wants it and `symbol_ready`.
- `lexical` runs only when its shape row wants it and `trigram_ready`.
- `variants` runs only when its shape row wants it and `trigram_ready`; otherwise the readiness callback reports `variants: unavailable - lexical index rebuilding (<reason>)`.
- `semantic` runs only when its shape row wants it and `semantic_ready`; otherwise it performs no cache lookup and reports `semantic: rebuilding - lexical results only (<reason>)`.
- `exact` remains in the plan when its shape/query row wants it. It uses ready mode only under the exact-mode predicate above; fallback mode delegates to A's deterministic bounded walk and prints `exact pass: bounded (N files, <reason>)`, where the imported R17 reason is exactly `file limit`, `result limit`, `index not ready`, or watchdog `time limit`. The watchdog form appends `- page stability void`, sets `plan.stability_void: true`, and makes no stability claim for that reply.
- `anchored` remains in its row. It discovers through trigram when ready and otherwise uses the A-owned bounded walk with its disclosure.
- `path_lookup` is index-free and always runs for `path`.
- If those rules leave no retrieval lane, `fallback_walk` runs. Therefore no observed matrix row has an empty `lanes_run`; this does not promise a non-empty result.

The matrix has the full product of these eight observed readiness triples and these ten literal query classes: `identifier/long` = `parse_header`; `short/long` = `character cap`; `short/tiny` = `a b`; `code_literal/long` = `"exceeds the cap"`; `code_literal/tiny` = `"ab"`; `nl/plain` = `where do we cap the room name`; `nl/quoted-long` = `why does it print "exceeds the cap" here`; `nl/quoted-tiny` = `why does it print "ab" here`; `log_excerpt/std` = `2026-09-08 ERROR open /tmp/a.rs failed`; `path/std` = `src/main.rs`. The checked-in expectation is an independently authored 80-row table. Every class first proves `(snapshot branch, B2 shape, exact presence, exact_input, token list, all-ready mode)` through the public backend. The matrix then asserts exact `lanes_run`, counters, mode, and disclosure presence for every row.

**Ordered token variants (R32).** Variants run only for `identifier` and `short`, whether or not a definition was found, and only while trigram is ready. They never invoke the embedding cache or client. For input token `t`, generation is exactly:

1. Parse words at separators, lower-to-upper boundaries, and upper-to-upper-lower boundaries, so `HTTPServer` is `[HTTP,Server]`, never `[H,T,T,P,Server]`. A leading `#` is preserved by case-form conversion.
2. Emit case forms in this order: `snake_case`, `kebab-case`, `camelCase`, `PascalCase`, `SCREAMING_SNAKE`. Exclude `t` itself and any duplicate of an earlier emission.
3. Toggle number on the last parsed word of `t` only, preserving the rest of `t`: `y` and `ies` are inverse forms; sibilant endings use `es`; ordinary forms use `s`; an already-ambiguous terminal word is unchanged. Emit the result once if it differs from `t` and earlier outputs.
4. If `t` matches `^#?[0-9a-f]{6,}[_-]`, emit the unmatched remainder.
5. De-duplicate by first emission, then retain only the first six outputs.

Literal ordered goldens, including the exact truncation result, are:

| `t` | Full relevant derivation | Final ordered output |
|---|---|---|
| `HTTPServer` | five case forms, then pluralize the last word | `[http_server, http-server, httpServer, HttpServer, HTTP_SERVER, HTTPServers]` |
| `items` | unchanged lower-case forms are excluded; Pascal and screaming forms precede singularization | `[Items, ITEMS, item]` |
| `#abc123_foo` | snake equals `t`; the hash remainder is reached after number toggle | `[#abc123-foo, #abc123Foo, #Abc123Foo, #ABC123_FOO, #abc123_foos, foo]` |
| `#abc123_HTTPServer` | full pipeline yields the five case forms, number toggle, then hash remainder `HTTPServer`; truncation removes that seventh emission | `[#abc123_http_server, #abc123-http-server, #abc123HttpServer, #Abc123HttpServer, #ABC123_HTTP_SERVER, #abc123_HTTPServers]` |

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

- **Draft and source integrity.** Front matter is delimited by `---`; exactly the required five `##` sections are present; the file is below 80 KB; the pinned SHA and every code enum/trait/call-site citation above are checked against `origin/main`; census weights are checked against `.alfonso/data/aft-search-followup-census/mechanism-report.md:23-31` and sum to the source's complete five-stratum population.
- **Resolver closure.** The complete unmodified draft, inline tests, generated B2 ledger, and projected chair ledger resolve through R1-R38 before scoring. Removing a local entry, double-classifying an A import, exposing the fenced negative symbol, or stripping the projection's ignore markers fails and names the symbol.
- **Fence and dependency audit.** Every implementing diff is contained by exactly one row of the B2 fence table; A/B2, B1/B2, and B2/B2 intersections are empty. Every B2 slice descriptor names `blocked_on: wi_e9aa228a` and the stamped verify artifact. Any B2 write to a read-only A or B1 path fails the audit.
- **Executable seam.** One end-to-end request through `handle_semantic_search` proves each retrieval callback (`symbol`, `exact`, `anchored`, `lexical`, `variants`, `semantic`, `path_lookup`, `fallback_walk`) executed by observing its fixture-specific result and `plan.executed_callbacks`. A degraded request proves `readiness_disclosure` executed by observing the exact mapped reason. Mutation control: register each callback but suppress dispatch; the corresponding public-backend test reds. These tests consume the A seam and never patch A files in the B2 checkout.
- **Seven-shape and regex goldens.** Every router precedence overlap asserts the seven-shape result through the public backend. The branch oracle remains pointwise identical, including pre-tier-exempt/regex overlaps. `^export` and `foo.*` remain on the existing regex route with result and key-set preservation and zero counters. Mutation controls reverse regex precedence or route regex through B2; branch and execution goldens red.
- **Whole-query quote control.** Public requests `"ab"`, `'ab'`, and `"a b c"` assert `code_literal`, the literal inner `exact_input`, fallback mode, `lanes_run == {exact, lexical}` when trigram is ready, and `(embedding_calls, embedding_cache_hits, live_embed_calls) == (0,0,0)` cold and warm. Restoring strip-before-classify reds `"ab"` on shape. Enabling semantic for any code-literal row reds all affected counter and lane assertions.
- **Matrix totality.** The independent 80-row golden covers every declared class and readiness triple. It compares the runtime row, not the derivation function, and checks lanes, all counters, exact mode, mapped reasons, and disclosure presence. Named mutations enable semantic for identifier/code-literal, retain variants while trigram is down, remove last resort, compute exact mode from surrounding prose, count outer quotes as token characters, or collapse class-dependent rows to shape-only rows; each reds its affected rows.
- **Readiness state machine.** Public-backend fixtures exercise semantic `Disabled`, `Building { stage: loading_artifacts }`, `Ready` with empty and non-empty `refreshing`, and `Failed`, including the literal `building:loading_artifacts` disclosure; trigram `Ready`, `Building`, `Fallback`, and `Disabled`; symbol loaded/unloaded; and eviction and lock contention for each applicable index. They assert the observed row and exact reason string. A transition during the all-down wait proves the second sample alone selects the plan; a transition after that sample does not change the request's row or counters.
- **Embedding boundary.** Definition-present and definition-absent identifiers, every code literal, log excerpt, path, regex, and semantic-disabled row assert no cache lookup and no boundary call in cold and warm state. Semantic-enabled short and NL controls assert one cold boundary call and a warm cache hit without dropping `semantic` from `lanes_run`. A mutation that infers calls from lane labels instead of instrumenting `embed_texts` reds the cache controls.
- **Variant pipeline.** The four ordered arrays above compare literally. The over-six case proves first-emission truncation; `items` proves plural-to-singular; the hash cases prove prefix stripping; `HTTPServer` proves acronym-run boundaries. Definition-present and absent identifier paths both execute variants when trigram is ready. The footer lists only contributing variants in generation order. Mutations reorder camel/kebab, split an acronym by letter, toggle a non-final word, omit SCREAMING_SNAKE, strip every hash, or truncate before de-duplication; a literal golden reds.
- **Reachability and honest zero.** Every match-bearing B2 fixture uses the oracle row above before checking results. Every `reachable_via` cell produces a non-empty result with that lane recorded. `unreachable` and beyond-bound cells may be empty and must not fabricate a row. The all-ready no-match identifier carries only the exact R36 line; the trigram-down/symbol-up variant fixture carries that line plus the trigram reason; the empty-root fixture carries its empty-scope statement. A mutation treating non-empty `lanes_run` as proof of reachability or returning a nearest neighbour reds these assertions.
- **Measured mechanism checks through B1.** The NL lexical change targets episodes 4173/2299/1776 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:95-99`); definition-first checks episodes 14708/17879/20043 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:101-105`); variants check episodes 5985/13820/20065 (`.alfonso/data/aft-search-followup-census/mechanism-report.md:119-123`); readiness checks the index-stale fixtures (`.alfonso/data/aft-search-followup-census/mechanism-report.md:125-129`). B1 scores the unchanged rows under `pinned_shape`; B2 must meet B1's existing gate and may not rewrite expected metrics or population after seeing its result.
- **B1 population immutability.** A golden runs the same B1 manifest before and after a deliberately changed B2 classification: row ids, census strata, quotas, denominator, and `pinned_shape` stay byte-identical; only `runtime_shape` and the drift report change. Mutation control re-stratifies on `runtime_shape`; the population digest reds.
- **Imported A behavior remains read-only.** Exact and anchored retrieval, comparator ordering, fusion, paging, confidence, bounded-walk limits, and provenance are tested by their pinned A gates. B2's public tests may assert their outputs as consumer evidence but no B2 slice writes their implementation. The import-closure audit requires a pinned A gate for every such assertion.

## non-goals

- B1's manifest construction, corpus and embedding packs, quality-gate implementation, baseline recording, quota arithmetic, confidence calibration, or post-release re-measure.
- Campaign A's comparator, exact/anchored/lexical/semantic implementations, fusion, paging, confidence, provenance, dispatch, plan table, or public agent surfaces.
- A new BM25/body index, a new symbol index, a new embedding provider/model/dimension, or any network-backed CI dependency.
- Any agent-facing lane or hint parameter; any change to `grep`, regex semantics, `topK`, project-root selection, or `includeTests`.
- A guarantee that every on-disk match is reachable in every readiness state; reachability is fixture-and-state specific under R37.

## open_questions

- none: ownership, namespace closure, B1 population stability, seam dependency, quote handling, variant order, readiness observation, no-match text, and reachability are fixed above.
