# Why a natural-language aft_search query was routed `code_literal` (2026-09-26)

Diagnosis only. No routing or ranking code was changed.

Specimen: row 1 of `prefrontal-search-misses-2026-09-25-baseline.md`, query

    board staleness nudge wake text: what the board nudge shows the agent (stale lanes, terminal waits) and lane cap 8

The recorded plan (`benchmarks/aft-search/results/prefrontal-search-baseline.json`, `tasks[0].response_fields.structuredContent.plan`) has `shape: code_literal`, `lanes_run: [exact, lexical]`, `embedding_calls: 0`, `exact_input` = the whole query, `exact_tier: none`, `confidence: low`. The top-level `query_kind` is `NaturalLanguage`.

## 1. The rule that fires

The live router is `crates/aft/src/search_b2/router.rs::classify`. It is installed by `search_b2::install_defaults()` (`crates/aft/src/search_b2/mod.rs:25-49`) and called at `crates/aft/src/commands/semantic_search/mod.rs:723-724`. (The older `extensions.rs::classify_raw_query` is the unused default and does not route this query to `code_literal`.)

`router.rs::classify_analysis`, lines 103-114:

```rust
let code_syntax_outside_span = has_code_syntax_outside_spans(analysis);
if whole_quoted
    || (code_syntax_outside_span && (analysis.spans.is_empty() || analysis.tokens.len() <= 3))
    || (!analysis.spans.is_empty() && analysis.tokens.len() <= 3)
{
    return SearchShape::CodeLiteral;
}
```

`has_code_syntax_outside_spans` (`router.rs:326-350`) returns true on the **first** character outside a quoted span that is in `( ) { } [ ] < > = ! ; , | &`, or on any backslash escape. It has no token-count limit and no check for how much of the query is prose. Because the query has no quoted span, `spans.is_empty()` holds, so a single such character anywhere in a query of any length makes the query `code_literal`.

For the specimen, the first trigger is the `(` before `stale lanes`. The `,` and `)` would each trigger the rule on their own. I checked this against the real router code (see Method):

| Variant of the specimen | Router shape |
|---|---|
| as recorded | `code_literal` |
| parentheses removed, comma kept | `code_literal` |
| comma removed, parentheses kept | `code_literal` |
| both removed | `natural_language` |

The colon, `cap 8`, and the query length play no part. The colon is not in the trigger set. Neither the colon nor `8` matches the timestamp or pid patterns, so the log-excerpt rule does not fire either.

`code_literal` then maps to `[Exact, Lexical]` only (`crates/aft/src/search_b2/lane_plan.rs:72`). The B2 spec pins this row as never running semantic search (R33; `.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b2-shape-router-lane-plan-counter-variants-readiness-refire-7.md:183-189`). The exact lane receives the whole 19-token sentence (`router.rs:352-366`), which no file contains verbatim, so exact finds nothing and only lexical file summaries are left. That is why confidence is low.

The rule follows the spec as written. Precedence row 3 of the spec (`…refire-7.md:138`) says: "code punctuation `(){}[]<>=!;,|&` or a backslash escape occurs outside spans". The spec never considered ordinary prose punctuation such as commas or parenthetical asides. The implementation adds one condition the spec table does not have: `spans.is_empty() || tokens <= 3`. This produces an asymmetry:

- `how are "exact tier" results ranked, and why` → `natural_language`, because a quoted span exists and there are more than 3 tokens.
- `how are exact tier results ranked, and why` → `code_literal`.

So adding quotes to a prose question with a comma takes it off the code path.

### Why `query_kind` says NaturalLanguage

Two independent classifiers run on each request:

- **Router shape** (`search_b2::router::classify`, `SearchShape`): chooses the lane plan. It is reported as `plan.shape`.
- **Ranking kind** (`query_shape::classify`, `crates/aft/src/query_shape.rs:74-125`, `QueryKind`): runs again at `semantic_search/mod.rs:1021` on the request text, after quote stripping for `code_literal`. It chooses ranking weights and mode and is reported as `query_kind` through `query_kind_label`. It has no punctuation rule. It sees more than 2 words and no identifier, error code, path, or regex, so it answers `NaturalLanguage`.

The two classifiers never consult each other, so a response can report `query_kind: NaturalLanguage` together with `plan.shape: code_literal`.

## 2. How often the rule fires on real queries

Sources in the repo:

- `real-query-manifest.json`: 300 sampled real episodes, 60 for each of 5 census strata, drawn from 6,469 census episodes. The census itself (`.alfonso/data/aft-search-followup-census/`) is not in the repo.
- `prefrontal-search-fixtures.json`: 4 real specimens.
- `engine-fixtures/exact/census_episodes.json`: 3 episodes.
- Synthetic fixtures, for contrast: `exact-recall-fixtures.json`, `fixtures.json`, `identifier-fusion-fixtures.json`, `external-fixtures.json`.

I found no other telemetry dump in the repo that contains query text.

Router shape today, 300 manifest rows: NL 92, short 84, identifier 54, **code_literal 43**, log_excerpt 23, path 3, regex 1.

For each of the 43 `code_literal` rows, I judged by hand whether that shape is right:

**Right or defensible (36).**
- Whole-query or multi-span quoted strings, for example `"was not found on PATH"`, `"EXPLAIN QUERY PLAN"`, `"work item" "was not found"`, `"Sticky-balanced routing is waiting for current OAuth quota"`.
- Call-site fragments: `claim_or_open_read_only(`, `Database.layer(`, `pushNotification("action"`, `stamp_project_id(`.
- Code snippets: `name = "libc"`, `"disabled": ["rust"]`, `case "edit":`, `SelectionContext {`, `ModuleManifest serde(default)`, `<MODULE>_STORAGE_DIR`, `persona.reference_get"]`, `git diff --unified=3 HEAD`.
- Short quoted mixes: `"outside <touser>" reminder text`, `"events" "subscribe" op`, `PlanOutcome::ReleaseIncomplete rendering "the module's owner has not published this platform"`.

**Same failure as the specimen: prose punctuation on a natural-language question (2).**
- `followup-census:7183`: `rearm_slice store implementation: which work_dispatch_intent rows get superseded_at stamped on rearm (failed only, or all)`. Triggered by `(`, `,`, `)`.
- `followup-census:11375`: `Four entry points: interactive TUI, omp -p print, --mode rpc NDJSON, acp JSON-RPC`. Triggered by `,`.
- The specimen itself (`prefrontal-board-nudge-lane-cap`) is a third real example, from a different source.

**Mixed: a code fragment plus natural-language keywords, so semantic is lost (2).**
- `followup-census:11468`: `cfg(debug_assertions) AFT_TEST delay environment spawned binary integration`.
- `followup-census:7363`: `CodexAuthPlugin({ codexApiEndpoint experimentalWebSockets issuer OpenAI plugin install built-in`.

**Borderline (3).**
- `3480`: `RECOMMENDED HEAD (highest IQ`. A pasted prose fragment with an unmatched `(`. Either shape is harmless.
- `10215`: `resolveCortexKitStorageRoot,`. A trailing comma turns an identifier into `code_literal`, so the Symbol lane is lost. This is a different failure; see §3.
- `667`: `"ExecutableDescriptorObservationV2" docs/…/schema-definitions.v1.json`. Quoted identifier plus a path. The PathLookup lane is still added by the path fact.

**Estimated prevalence.** All three clear misroutes, and both mixed rows, sit in the census `code_literal` stratum (1,718 of 6,469 episodes). The census `nl` stratum has **0 of 60** `code_literal` today. That is a selection artifact: the census appears to have labelled these rows with the same kind of rule, so misrouted prose was counted as `code_literal`, not as `nl`. Weighting by stratum:

| Group | Sample | Point estimate of census episodes | Wilson 95% | Share of 6,469 |
|---|---|---|---|---|
| Clear NL misroute | 2/60 | ~57 | 16–195 | ~0.9% |
| Clear + mixed | 4/60 | ~115 | 45–274 | ~1.8% |
| `code_literal` overall today | — | ~671 | — | ~10.4% |

**The synthetic sentence fixtures show the mechanism more directly.** In `exact-recall-fixtures.json`, 4 of the 8 docstring "sentence" queries route `code_literal`, only because of a comma or parentheses:

- `All of the file type definitions, sorted lexicographically by name.`
- `Each include selector contributes a set of tasks (unioned together).`
- `In watch mode, we can have a changed package that we want to serve as an entrypoint.`
- `Since we are mutating/assigning only top level props, it is fine to …`

Any prose sentence with a comma or a parenthetical is `code_literal` unless it also contains a quoted span. The tool description tells agents to write full natural-language sentences, which makes this likely to grow.

`fixtures.json`, `identifier-fusion-fixtures.json`, `external-fixtures.json`, and `census_episodes.json` contain no `code_literal` rows.

## 3. Gate rows (43 non-excluded rows in `real-query-manifest.json`)

Router shape today: NL 20, short 14, **code_literal 6**, identifier 2, log_excerpt 1.

Baseline metrics are from `real-query-baseline.json`. That baseline's binary may predate the current router.

| Episode | Query | Judgement | Baseline hit@5 |
|---|---|---|---|
| 7617 | `"was not found on PATH"` | right (whole-quoted) | 1 |
| 8637 | `claim_or_open_read_only(` | right (call fragment) | 0 (`not_a_search_failure`) |
| 10215 | `resolveCortexKitStorageRoot,` | wrong kind: trailing comma removes identifier routing | 1 |
| 10672 | `pushNotification("action"` | right | 1 |
| **11468** | `cfg(debug_assertions) AFT_TEST delay environment spawned binary integration` | **resembles the specimen**: code fragment plus 6 keywords, so semantic never runs | **0** (`index_stale_or_missing`) |
| 18338 | `"disabled": ["rust"]` | right | 0 |

No gate row is a pure prose question with a comma or parentheses like the specimen. **11468** is the nearest: the `(` of `cfg(` alone sends a mostly keyword query down the exact-plus-lexical path. The gate therefore gives almost no signal on this failure. A fix should add a prose-with-punctuation row, and the prefrontal fixture already provides one.

## 4. Options (not implemented)

Each option was replayed with a modified copy of `router.rs` over every query above, plus the router goldens (`crates/aft/tests/fixtures/search_b2/router/cases.json`, 23 cases) and the lane-plan matrix queries (`crates/aft/tests/fixtures/search_b2/lane_plan/matrix.json`).

For all options, exact recall is protected the same way. The natural-language plan still runs the exact lane with the same whole-query `exact_input` (spec R2a; `lane_plan.rs:48-56,73-77`), and an exact verbatim hit is admission-exempt and leads. So moving a query from `code_literal` to NL adds the semantic lane (and the Symbol lane if an identifier token is present) plus one embedding call, and removes nothing. The residual risk is fusion ordering when exact finds nothing and semantic outranks lexical. It must be measured with `run_exact_recall.py` (sentence family) and the real-query gate. It was not run here.

**A. Apply the punctuation rule only to short queries.**
Change `router.rs:110` to `code_syntax_outside_span && tokens.len() <= 3`, which matches the existing quoted-span limit.
- Fixes: the specimen, 7183, 11375, all 4 exact-recall sentences, 3480, and the mixed rows 11468 (gate) and 7363. It also moves `git diff --unified=3 HEAD` to NL.
- Breaks one pinned golden: `why does it print \"cap\" here` → `code_literal` (spec line 148, backslash escape).
- Variant **A′**: keep the backslash rule unconditional and limit only the punctuation. A′ passes all goldens and lane-matrix rows.
- Risk: pasted code lines of 4 or more tokens (for example `if (x) { return y; }`) become NL and pay one embedding call. Exact still runs on the whole line.
- Changes one gate row (11468).

**B. Stop treating prose punctuation as code (smallest correct change; recommended).**
In `has_code_syntax_outside_spans`, drop `,` from the trigger set. Count `(` only when it directly follows an identifier character (call or attribute syntax such as `foo(` or `cfg(`), and ignore `)`.
- Fixes: the specimen, 7183, 11375, all 4 exact-recall sentences, and 3480.
- Keeps as `code_literal`: every call fragment, `cfg(debug_assertions) …`, `CodexAuthPlugin({ …`, `git diff --unified=3 HEAD`, `name = "libc"`, and every router golden and lane-matrix row. Zero golden changes.
- Side effect: `resolveCortexKitStorageRoot,` (gate 10215) becomes `short`, which regains the Symbol and Semantic lanes. That arguably fixes the trailing-comma identifier problem too.
- Risk: code that uses only commas or space-separated parentheses, such as a tuple `(a, b)` or `foo (x)`, stops being `code_literal`. Generic types keep their `<`/`>` trigger.
- Does not help the mixed rows (11468, 7363).
- Needs a spec ruling, because it narrows the literal punctuation list in precedence row 3.

**C. Word-count override: run semantic when there are enough natural-language words.**
Either route to NL when there are at least N (about 6) tokens that are mostly lowercase dictionary words, or keep the shape and add Semantic to the `code_literal` plan above N tokens.
- Fixes the specimen, 7183, 11375, the sentences, and probably 11468 and 7363.
- The plan-side version directly contradicts R33 ("code_literal never semantic"), and the B2 tests are written to go red on it.
- Long whole-quoted error strings (for example the 8-word `"Sticky-balanced routing …"`) would start paying embedding calls unless whole-quoted spans are excluded.
- Needs a tuned threshold and a word-likeness test, which is a new heuristic surface.
- Exact recall is protected as above.

**D. Split mixed queries into exact fragments and a natural-language remainder.**
Route queries with at least 4 tokens and code fragments to NL. Send the code-shaped fragments (`cfg(debug_assertions)`, `foo(`, `{ … }`) to the exact lane, the way `embedded_span` already hands a quoted span to exact inside NL (`router.rs:52-55,64-75`), and embed the whole query for semantic.
- Most correct for 11468 and 7363, and a superset of B's fixes.
- Largest change: fragment extraction, handling of multiple fragments (only one `embedded_span` exists today), a spec change to precedence rows 3 and 5, and new goldens.
- Exact-recall risk: exact searches for the fragment and no longer for the whole line. That helps when the fragment is verbatim and hurts when the user pasted a whole verbatim line that includes prose. Keeping whole-query exact as a second exact input would avoid this.

**Secondary (separate issue).** The two classifiers disagree (`query_kind` from `query_shape` versus `plan.shape` from the router). Once the router changes, the disagreement stays harmless but confusing. Either derive `query_kind` from the router shape or document both fields.

**Recommendation.** Do B, or A′ if the maintainers prefer a size rule to a character rule, together with a gate or prefrontal row for prose with a comma. Treat D as a follow-up for mixed code-plus-keyword queries such as gate row 11468. Before merging either, verify with the router goldens, the lane-plan matrix, `run_exact_recall.py` (sentence family), `run_prefrontal_search.py`, and the real-query gate.

## Method

I built a throwaway binary outside the repo (`/tmp/rtr`, not committed). It `include!`s the unmodified `crates/aft/src/query_shape.rs`, `pattern_compile.rs`, and `search_b2/router.rs`, with a small stand-in module for `RawQuery`, `QueryFacts`, `Span`, and `SearchShape`. For each JSON-string query it prints the router shape and the `query_shape` kind.

Fidelity check: it reproduces all 4 recorded plan shapes and `query_kind`s in `prefrontal-search-baseline.json`, and all 23 expected shapes in the router golden `cases.json`. Options A and B were two text-substituted copies of `router.rs` in the same binary.

The right/wrong judgements in §2 and §3 are manual.
