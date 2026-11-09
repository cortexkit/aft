# Prose search with checkout views

## Reproduction and limits

The investigation used an isolated scratch Git repository, private storage and
a loopback embedding backend, through the real configure and search entry points.
No live SQLite file was opened, and no daemon was stopped or reconfigured.

`per_checkout_semantic_runtime::prose_watchdog_query_reports_views_coverage_and_keeps_symbol_previews`
uses the query:

> how does the bash watchdog kill a background task that exceeded its timeout

The filled checkout serves **11 vectors**. The semantic lane returns **11 raw
hits**, with **0 pending files**, **0 failed files** and complete coverage. Its
public answer has five distinct files. Before the fix, the first result was:

```text
src/watchdog.rs [lexical match]
```

After the fix, the same first result is:

```text
src/watchdog.rs [lexical match]
  kill_expired_task [function] lines 3-5
      pub fn kill_expired_task() {
          terminate_child();
      }
```

This proves that missing semantic-looking rows do **not** establish an empty
view or a failed query embedding. A semantic contribution can supply the symbol
while lexical wins the file's fused provenance. It does not establish the live
daemon's vector count: that count remains unmeasured. The provided `view loaded`
log alone cannot establish that every file has vectors. This investigation chose
the scratch-reproduction option rather than copying the live store.

## Why the symbol disappeared

`commands/semantic_search/mod.rs:4377-4410` retains the semantic lane's best
symbol metadata per file. At `:4870-4924`, the fused file can keep that metadata
but get `source = "lexical"` when lexical is its winning non-exact lane.

Previously, `snippet_set_by_engine` (`:3804`) treated every lexical result as
an already-selected matching line. That is true for lexical-only file summaries,
but false for semantic symbols with lexical-winning provenance. Source preview
enrichment therefore skipped them. The rendering loop (`:7780`) also treated
every lexical result as a file-only match and skipped its symbol header/body.
Together these branches reduced a populated semantic symbol to a bare file path.

The fix restricts both shortcuts to `FileSummary`. Real symbols keep fresh,
rank-budgeted source previews and their line ranges. The file still says
`[lexical match]`: its winning lane is not relabelled as semantic.

Git history locates these broad shortcuts in `fbd264f22` (identifier answer
rendering), before both reported cards, `8905b87d7` and `6aa715d2f`. The changes
between those two cards do not introduce either shortcut. The different display
can depend on whether the file has semantic symbol metadata at query time:
lexical-only file summaries use the matching-line route and already had snippets.
No code-only bisect between the two cards can isolate that state-dependent bug.

## The legacy import conflict

Semantic family keys address **inputs**: file bytes, relative path, chunker,
template/caps and model fingerprint (`views/semantic.rs:106-122`). They do not
hash the output vectors. `migration/per_checkout.rs:1316-1317` places legacy
vector bytes under that input key. Separate backend invocations can yield
different float bytes for the same inputs.

The family store correctly refuses a different payload under the same key
(`blob_store/v2.rs:459-511`). Previously `store_bundle` propagated that conflict
with `?`, aborting the entire import, including unrelated semantic files and
subsequent artifacts. The failure can leave a resumable ledger at `Validated`;
it is not evidence that the subsequent semantic view loaded no vectors. The
view worker loads independently and fills missing content.

The import rule now keeps and touches the first stored payload **only if it
decodes for the expected semantic producer** (`migration/per_checkout.rs:1754`).
The store's immutable equal-key/equal-payload rule is unchanged. Invalid or
quarantined semantic keys are skipped individually, logged by key, excluded
from the successful-file count and published as pending rather than ready
(`:1878`, `:1954`). Other files still import. I/O/database errors and trigram
conflicts are not suppressed. Durable staged bundles and legacy caches are not
rewritten; a resumed import applies the same rule again.

Two regressions cover these choices:

- `semantic_import_keeps_the_first_vector_payload_and_imports_other_keys`
  supplies different vector bytes under the same input key, verifies the
  existing payload is immutable and that every other key imports.
- `semantic_import_rejects_only_the_unusable_key` checks both an invalid
  existing payload and a quarantined key. Only that file remains pending, and
  the ledger completes.

## Honest coverage labels

A second scratch case loads a view but holds its fill. It serves **0 vectors**
and **0 semantic hits**, with `lib.rs` named as pending. The pre-fix reply
correctly included that gap but contradicted it with a final `semantic: ready`.

`SearchLaneStatus::attach_labels` (`commands/semantic_search/mod.rs:1786`)
appended runtime usability labels after the query had already disclosed its
coverage. Runtime readiness is not coverage completeness. It now honours the
reply's `semantic_gap` in the footer, rendering `semantic: partial (checkout
coverage gaps)` or the named unavailable reason. Query-only and split requests
also label the semantic response status from their actual checkout gaps, rather
than calling it ready. The regression
`unfilled_checkout_semantic_view_names_the_gap_without_a_ready_footer` checks
both request forms without timing or a running fill worker.

## The walk trailer: proposal only

`commands/semantic_search/trailer.rs:100-110` maps an exhausted candidate list
(`S2Exhausted`) to `Reason::Walk` whenever the requested page does not contain
the entire list. Thus `shown 8 of 76 results (walk)` can mean “all candidate
lanes exhausted, but only the requested page is shown”, not a filesystem walk.

An honest future wording would be **`shown 8 of 76 results (page)`**, with the
existing narrowing knobs. Exhaustion establishes the exact total; pagination
explains why only eight were printed. A distinct page reason would preserve
`walk` for actual bounded walks. No trailer, shared reason vocabulary or
trailer-contract fixture is changed here.

## Verification and ranking boundary

The only ranking-fenced production file touched is
`commands/semantic_search/mod.rs`. Enumeration, lane selection, scores, fusion,
candidate metadata, ordering and page cuts are unchanged. There is an
`engine_unwired` descriptor for the task branch.

The pinned replay completed **16 exact-recall fixtures**, **26 concept fixtures**
and **93 real-query rows** using `aft 0.58.2`. Exact recall is 1.000. All 93 rows
are byte-identical to the reference after removing only `summary_text` from each
row; the complete families, fixture groups, shapes and mechanisms are also
byte-identical. The score SHA-256 is
`35d02c69960cfeef0afcbb55a4c026534c55c40643b23869bf96a93822cc0613`.

The unmodified full `engine_unwired` predicate nevertheless returns
`engine_unwired_mismatch:row=real_query.followup-census:910001`: ten split rows
capture the first eight display lines in `summary_text`, and those lines encoded
the bare-path bug. Their previews intentionally change. The affected row suffixes
are `910001`, `910003`-`910009`, `910011`, and `910012`. No reference, manifest,
answer key or gate predicate was changed to conceal this presentation conflict.

The debug-binary full replay timed out at the shell's 30-minute cap after exact
and concept completed. The failed real-query phase was rerun successfully with
the optimized release binary; the already-completed phases were not rerun.
The fixture server logged broken pipes while shutting down its held-building
backend, but the 93-row replay exited 0 with its complete score artifact.

Affected gates pass: 101 search unit tests, 12 migration unit tests, six views
runtime integration tests, 12 public tool-call parity tests, 17 migration
integration tests (one child-only fixture ignored), 204 engine tests and 172
list-envelope tests. The Windows `--tests` compile with warnings denied and
`cargo fmt --all -- --check` pass. Scoped Rust diagnostics report zero errors
and warnings. Rust/cargo are 1.99.0, rustfmt 1.10.0, Python 3.9.6.

Every fix had a running pre-fix failing regression. Four staged-state mutation
proofs reverted its essential control in turn, marked `NON-VACUITY BREAK`,
captured a non-empty diff, ran the affected target, restored from the staged
implementation and captured an empty diff:

| Disabled control | Only test that failed | Other tests still passing |
| --- | --- | ---: |
| Preview enrichment for lexical-winning symbols | `lexical_winner_with_semantic_symbol_keeps_its_location_and_preview` | 100 search tests, including the coverage-gap regression |
| Coverage-aware footer | `unfilled_checkout_semantic_view_names_the_gap_without_a_ready_footer` | 100 search tests, including the symbol-preview regression |
| First-valid-payload reuse | `semantic_import_keeps_the_first_vector_payload_and_imports_other_keys` | 11 migration tests, including unusable-key isolation |
| Per-key rejection rather than whole-import abort | `semantic_import_rejects_only_the_unusable_key` | 11 migration tests, including first-payload reuse |

The restored targets are green. Mutation markers are not part of the delivered
code. Full mutation logs and benchmark artifacts remain under the ignored
`benchmarks/aft-search/.bench/search-prose/` in the task worktree.

## Completing the presentation-only quality gate

The continued delivery starts at main `2772a315f` and cherry-picks the preserved
`d347aa4c3` fix. It leaves the reference, its sidecar, manifest, vector packs and
answer keys unchanged. The ranking-fenced production change is still only
`crates/aft/src/commands/semantic_search/mod.rs`, and it changes presentation,
not candidate ranking or routing.

The harness now accepts `presentation_rows` **only on `engine_unwired`**. Each
entry pins the exact reference and replay `summary_text` arrays. Every listed
row must actually change; missing rows, mismatched before/after lines and any
unlisted difference fail with the row and field named. All other row fields,
including ranked paths and pattern summaries, and the existing family/metric
comparisons remain strict. Latency-only ranking descriptors get no allowance.
Existing cross-Python aggregate-rounding normalization is unchanged.

The new real replay used a fresh release build (`aft 0.58.2`, binary SHA-256
`535aa74067aa2357ef56a087382f85070560d8d1e0ec00231a721122894093fa`), the
provisioned four-repository exact corpus, and the pinned evidence tree at
`30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17` (tree SHA-256
`310722813f165b4e1b5d6f2427428c5ab5dd82a05f510ee8361d7920aece59d5`).
`AFT_SEARCH_BENCH_SETTLE_SECONDS=30` avoids the documented macOS watcher burst.
It replayed **16 exact fixtures, 26 concept fixtures and 93 real-query rows**.
The strict descriptor failed on `real_query.followup-census:910001`, before the
new descriptor copied the measured lines. The replay score SHA-256 is
`90ffe51fba4c3477f615339b4cf26eea10346bb1cae5ec134b0aa07793457c14`; the
reference SHA-256 is
`396d491df35d5d203d159b28815bb164ed8e41e051dff4a227649ebd59a0b60b`.

Exactly these ten rows differ, **only in `summary_text`**:

- `followup-census:910001`
- `followup-census:910003`
- `followup-census:910004`
- `followup-census:910005`
- `followup-census:910006`
- `followup-census:910007`
- `followup-census:910008`
- `followup-census:910009`
- `followup-census:910011`
- `followup-census:910012`

All 93 rows' remaining fields are byte-equal, as are the complete families,
fixture groups, shapes and mechanisms. For example, `910008`'s eight lines were:

```text
crates/aft/src/search_index.rs [lexical match]

crates/aft/src/main.rs [lexical match]

crates/aft/src/runtime_drain.rs [lexical match]

crates/aft/src/callgraph_store/mod.rs [lexical match]

```

They are now:

```text
crates/aft/src/search_index.rs [lexical match]
  ingest_paths_parallel [method] lines 1241-1288
          fn ingest_paths_parallel(&mut self, paths: &[PathBuf]) -> usize {
              let max_file_size = self.max_file_size;
              let pool_size = search_index_build_pool_size();
              let chunk_size = pool_size.saturating_mul(4).clamp(1, 32);
              let pool = match rayon::ThreadPoolBuilder::new()
                  .num_threads(pool_size)
```

The task descriptor is renamed to the continuation branch's label so automatic
descriptor discovery works. It contains all ten exact measured pairs; the gate
passes on that score without re-recording anything. **The next train must
re-record the reference** after this presentation fix lands, rather than carry
these temporary reference transitions indefinitely.

The continuation's required gates pass on Rust/cargo 1.99.0 and Python 3.9.6:

- `scripts/telemetry/cost-gate.sh --search-quality --self-test`: goldens plus
  116 tests (one Windows-only case skipped).
- `cargo test -p agent-file-tools --lib -- semantic_search migration views`:
  503 passed, 12 ignored; child fixtures also passed.
- `cargo test --features test-hooks -p agent-file-tools --test integration per_checkout_semantic_runtime`:
  six passed.
- `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu`:
  finished successfully (compile-only, not Windows runtime execution).
- `cargo fmt --all -- --check`: exit 0 with rustfmt 1.10.0.

Ten additional staged-state mutations disabled the presentation pass path,
before-line check, after-line check, unchanged-row refusal, unlisted-row fence,
other-row-field fence, family/aggregate comparisons, descriptor-class check,
descriptor-shape validation and missing-row refusal separately. Each reddened
only its corresponding named `EngineUnwiredPresentationTests` test; the 13
existing `EngineUnwiredGateTests` remained green. The pass-path mutation ran its
one targeted test plus those 13 controls; the other nine ran all 23 tests. The
other tests in those runs remained green, and every mutation was restored from
the staged implementation with an empty `git diff --stat` afterward.

Scoped Python diagnostics show only the pre-existing `None`-to-`str` argument
error in `UnreachableSplitRowTests.test_a_waiver_is_refused_when_the_answer_file_cannot_be_read`;
no new Python diagnostic was introduced. The JSON schema has no LSP producer.
The first release build timed out while queued for shared compile slots; the
retry finished with the queue preserved. The fixture server printed broken
pipes when the held-building replay shut down, but every replay phase exited 0
and produced its complete score. Only the expected strict-descriptor predicate
failed before the allowance was populated.
