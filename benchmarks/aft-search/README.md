# AFT search eval harness

Manual retrieval benchmarks for `aft_search`. The original in-tree fixture suite
still measures AFT on this repository. The external suite adds Vera's published
21-task corpus so we can report Recall@1/5/10, MRR@10, and nDCG@10 against the
same pinned repos Vera uses.

## Setup

Build the release binary first:

```bash
cargo build --release -p agent-file-tools
```

Provision the Vera-compatible corpus and pinned real-query evidence from the
repository root:

```bash
python3 benchmarks/aft-search/provision_corpus.py
python3 benchmarks/aft-search/provision_evidence.py
```

`provision_corpus.py` reads `corpus/corpus.toml`, fetches each immutable commit
with depth one into `.bench/repos/<name>/`, and writes
`.bench/repos/provisioned.json`. `provision_evidence.py` materializes the pinned
AFT commit under `.bench/repos/aft-evidence-<sha>/`, using the local Git object
when available and fetching that SHA from origin otherwise. Its digest is the
SHA-256 of a canonical map from every projected relative path to that file's
SHA-256. `.bench/` is ignored by git and must not be committed. The runners
validate the exact-recall record and the complete evidence-tree digest before
starting AFT.

## In-tree benchmark

The existing AFT fixtures are unchanged in `fixtures.json` and still use
file-path expected results (`expected_top_files`). From `benchmarks/aft-search`:

```bash
uv run python run.py
```

Equivalent explicit invocation:

```bash
uv run python run.py \
  --binary ../../target/release/aft \
  --project-root ../.. \
  --out baseline.json
```

The runner starts `aft`, sends a user-tier `configure` document with search and
semantic search enabled, waits for the semantic index to be ready, runs
`semantic_search(top_k=5)` for every fixture, and writes the baseline-shaped JSON
report. A local embedding model and ONNX Runtime are required.

## Exact-recall gate

`exact-recall-fixtures.json` contains two deterministic sentence samples and two
2-3 content-token co-occurrence samples from comments or strings in each pinned
corpus repository. Sentence fixtures require the containing file at rank 1;
co-occurrence fixtures require it within the top 10. The report records the
result's `[exact]` classification separately; the marker does not affect recall.

The exact-recall runner deliberately disables semantic search, so the nightly
cost gate can exercise lexical retrieval and exact-tier fusion without an
embedding backend. The existing concept benchmark remains separate because the
nightly runner does not currently install its model/runtime prerequisites.

```bash
python3 provision_corpus.py
python3 run_exact_recall.py \
  --binary ../../target/release/aft \
  --out results/exact-recall.json
```

The command compares against `exact-recall-baseline.json` and exits nonzero if
sentence rank-1 or pair recall@10 drops below the checked-in baseline. The
nightly workflow appends the same table to its job summary and uploads the JSON
beside the index-cost artifacts.

`--semantic` replays the same fixtures and invariants with semantic search on,
using the live local model (the managed ONNX Runtime and model cache, as the
prefrontal runner uses). It answers a question the gate cannot: whether
routing a query to a semantic plan displaces its exact answer. It is
report-only and exits 0 unless the run fails, because the baseline was
recorded with semantic search off. Each row records the router `shape` and
`lanes_run`, so a reader can see which rows actually ran the semantic lane.

```bash
python3 run_exact_recall.py --semantic --ready-timeout 5400 \
  --out results/exact-recall-semantic-<date>.json
```

## Real-query quality gate

After the exact-recall corpus has been provisioned once, this command provisions
or repairs the pinned real-query evidence tree, builds `target/release/aft`
unless `AFT_BINARY_PATH` names a binary, runs exact recall and concept recall,
replays every included real-query row, writes an ignored score under `.bench/`,
and passes that score to the predicate:

```bash
scripts/telemetry/cost-gate.sh --search-quality --mode record-reference --dry-run
```

The real-query replay verifies and consumes the provisioned pinned tree, starts
the loopback-only embedding fixture server with empty temporary caches, and calls the
public `search` tool through standalone AFT's `tool_call` NDJSON command. The
`single_page` profile sends one request at the product's maximum `topK` without
an offset. The `paged` profile requires a declared, working offset, sends enough
maximum-size pages to cover the frozen 400-result scoring depth, and separately
executes page-size invariance plans that retrieve the same 100 rows. The harness
sets that maximum once as `PAGE_SIZE`; a test compares it with the checked-in
`aft_search.topK.maximum` schema so a later product-cap change fails by name
instead of turning every replay row into an invalid request. Every request
forwards the row's recorded `includeTests` value.

### Which embeddings the gate uses

The semantic lane is served by `embedding_fixture_server.py` from
`real-query-vectors.bin`: the real all-MiniLM-L6-v2 vectors, the model behind
AFT's default local backend, for every text AFT embeds on the pinned tree
(27,415 chunks, queries and the index probe). They are stored as float16,
indexed by the SHA-256 of the embedded text (`vector_pack.py`), about 22 MB.
A text with no stored vector is refused with `vector_missing`; the server never
makes one up.

The gate serves stored vectors instead of running the model because the live
local backend is not reproducible run to run. Three full paged replays on one
Mac and one binary disagreed on 3 of 49 rows, and each took 8-18 minutes
against about 6 for the pack. Serving the pack gave rows byte-identical to a
live run, and float16 gave the same rows as float32. int8 broke page
invariance, so it is not used. Until 2026-09 the pack held 8-number vectors
hashed from each text, so the semantic lane's effect on every earlier MRR
figure was deterministic noise.

On macOS the replay itself is not yet repeatable. FSEvents reports the
just-copied evidence tree to AFT's watcher about a second after the index is
ready. The watcher invalidates every semantic file (`runtime_drain.rs`, the
`SemanticIndex` apply phase) while the index still reports `ready`: entries
drop from 27,476 to 0 and are re-embedded over about 20 seconds, so early rows
are ranked against a partly empty index. Two Mac replays of the pack disagreed
with a third on 4 of 49 rows. On Linux, where CI runs, there is no such burst,
and three replays gave byte-identical scores. Record the reference on Linux.

When the pinned product's semantic chunk format intentionally changes, recapture
the pack in an authoring environment that has the model cache:

```bash
cd benchmarks/aft-search
uv run --with onnxruntime==1.24.4 --with tokenizers==0.22.2 --with numpy \
  python3 capture_real_query_vectors.py --allow-vector-authoring
```

`minilm_embedder.py` mirrors `crates/aft/src/local_embed.rs` step by step and
refuses a model snapshot whose digests differ from the captured one. The
capture starts from an empty pack and rebinds every manifest row.
`run_real_query.py --live-model` replays the same rows on AFT's own local
backend. That run is report-only, for checking a new pack against the live
model, and its `model_id` carries a `:live` suffix so the gate refuses it.

Run the independently named cases with, for example:

```bash
cd benchmarks/aft-search
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_missing_checkout_is_provisioned_from_local_repository_with_manifest_digest
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_mutated_evidence_tree_is_rejected_with_mismatch_fault
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_runner_is_byte_deterministic_on_the_same_tree
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_single_page_request_grammar_rejects_an_offset
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_paged_profile_covers_frozen_depth_and_runs_invariance_requests
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_stop_token_precedence_is_page_cap_then_exhausted_then_ten_files
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_missing_exact_recall_corpus_names_provision_command
python3 -m unittest -v test_run_real_query.RealQueryRunnerTests.test_recorded_include_tests_changes_ranked_paths
```

### Concept recall

`run_concept_recall.py` replays the 26 cases in `fixtures.json` through the
public `search` tool on the same pinned tree, served by the same fixture server
as the real-query replay. The chunk vectors come from `real-query-vectors.bin`.
The concept queries have their own pack, `concept-query-vectors.bin`, which
uses the same format and model. It records the digest of the chunk pack it was
captured against, so the real-query pack and the manifest rows bound to its
digest never change for a concept edit. Each case sends one `topK=50`,
`includeTests: false` request. It scores the rank of the first of its
`expected_top_files` among the first ten distinct files returned. The router
decides which lanes run, as it does for users; each row records `lanes_run`.
On the pinned tree 14 of the 26 cases run the semantic lane: every mixed,
natural-language and generic-file case, and both error-code cases. The nine
identifier and three path cases do not.

It refuses rather than guesses. A fixture query with no stored vector stops the
run before AFT starts. A text the fixture server refused during the run fails
it (`vector_missing`), because AFT would otherwise have ranked that query
without its semantic lane. A query pack from another model, pin or chunk pack
is rejected. Like the real-query replay, it is not repeatable on macOS. Record
it on Linux.

Until 2026-09 the runner never started AFT. It wrote 1.0 for every metric of
every case that had a hashed stand-in vector in `concept-vectors.json`, so the
gate's concept family could not move.

After changing `fixtures.json`, recapture the query pack in the authoring
environment the real-query capture uses:

```bash
cd benchmarks/aft-search
uv run --with onnxruntime==1.24.4 --with tokenizers==0.22.2 --with numpy \
  python3 run_concept_recall.py --capture
```

The capture only records texts that are fixture queries. A chunk missing from
the real-query pack fails it, because recording that chunk here would hide a
stale pack.

#### Answer keys checked against the pinned tree

Four answer keys were found not to hold at the pinned evidence tree
(`30d4a64f99b3`). Each was re-read there and either corrected or removed. An
answer is chosen because it answers the question, not because it ranks well;
the corrected cases carry their evidence (file and line) in their `notes` in
`fixtures.json`.

- `semantic_search unavailable renderer`, corrected. It listed the two
  plugin `tools/semantic.ts` files, and neither contains "unavailable". The
  text a user sees when the semantic lane is unavailable is rendered in
  `crates/aft/src/commands/semantic_search/mod.rs`
  (`format_lexical_unavailable_text`, line 3792, and
  `format_grep_lexical_unavailable_text`, line 3811). The Pi renderer
  `packages/pi-plugin/src/tools/semantic.ts` stays: it shows a non-ready
  `semantic_status` as a warning and prints "Semantic index is not ready."
  (lines 86-105). The OpenCode `tools/semantic.ts` is dropped; it passes the
  engine's text through and renders nothing itself.
- `process group already terminated`, corrected. The phrase occurs nowhere.
  The code that handles an already-terminated process group is
  `terminate_pgid` in `crates/aft/src/bash_background/process.rs`
  (lines 92-113; lines 100-104 say killpg on an already-empty group is a
  harmless ESRCH). It replaces `registry.rs`, which only calls it
  (line 4145), and `watchdog.rs`, which only asks the registry to kill a
  timed-out task (line 61).
- `subagent_type`, removed. The identifier occurs in no source file. Outside
  the benchmark's own answer keys it appears only as a sample query string in
  `crates/aft/tests/integration/query_shape_test.rs` (line 47), a test file
  the case's `includeTests: false` request excludes, and it is not an answer
  there either. The listed files, `tools/hoisted.ts` and
  `tools/hoisted-internals.ts`, do not mention subagents. The subagent code
  that does exist, `packages/opencode-plugin/src/shared/subagent-detect.ts`,
  identifies a subagent by its session's parent ID and has no notion of a
  subagent type, so someone looking up that field would not find it there.
- `useState hook examples`, removed. The only example of the hook in use is
  the test fixture `crates/aft/tests/fixtures/imports_ts.ts` (lines 5 and 14),
  which `includeTests: false` excludes. The other listed file,
  `commands/add_import.rs`, uses `useState` only as a sample import name in a
  doc comment (line 161) and a unit test (line 865). The remaining mentions
  are the same kind: sample import names in `imports/mod.rs` docs, and unit
  tests in `imports/mod.rs`, `query_shape.rs` and
  `commands/semantic_search/mod.rs`. No file the request can return shows
  the hook being used.

Removing a case also removes its vector from `concept-query-vectors.bin`, so
the pack keeps exactly one vector per case. The remaining vectors are the
captured ones, byte for byte.

The pinned tree also carries `.alfonso/reports/search-fusion-quality.md`,
which quotes these fixture queries. Both replays exclude it from the index;
see the corpus hygiene section below.

### Rows the reference may record as a miss

The paged replay faults on any row whose ranking depends on page size (the
`topK` 10, 25 and 50 invariance plans must agree). One manifest field relaxes
that for the reference only: `reference_not_page_invariant`, a one-line reason
naming the engine defect. A row carrying it that breaks page invariance is
recorded as a miss (every metric 0) with `page_invariance_failed` on the score
row, so a reference can be taken on an engine that still has the defect and
the fix can be measured against it.

The field never excuses a changed engine. Under a `ranking` descriptor,
`total_gate` faults on any evaluated score row that broke page invariance,
flagged or not, so the change that fixes the defect must make every row
invariant. Under a `non_ranking` or `engine_unwired` descriptor the engine is
unchanged, so a flagged row must reproduce the reference's recorded miss
exactly (the same `page_invariance_failed` record, every metric 0), and a row
the reference scored normally must not fail. That lets the train carrying
the rows and their reference land before the train that fixes the engine. A row without the field faults at
replay time exactly as before. Set the field only on rows added to measure a
known paging defect, never on a row that used to pass: there it would turn a
regression into a quiet miss. Remove it in its own change once the defect is
fixed, not in the same train as a reference re-record.

The field is set on six regex identifier rows,
`followup-census:900003`-`900008` (#375): the regex route on the recording
engine kept the first matches its scan met, in parallel scan order, and
sorted only those, so its pages depended on page size. The seventh regex row,
`900009`, has fewer matching lines than the smallest invariance page, so that
engine replays it invariantly and it carries no flag. Rows with between about
ten and forty matching lines were left out: on that engine they pass or fail
page invariance from run to run, so no reference taken on it would repeat.

### Split query/pattern rows

The gate manifest adds 12 hand-split rows, `followup-census:910001`–`910012`,
under the mechanism `split_query_pattern_fusion` and benchmark-only shape `split`.
The pin is AFT commit `30d4a64f99b3`; this shape does not extend that commit's
router enum. The R1–R7 categories describe these fusion scenarios. Counts are
R1=2 (stale mention), R2=2 (broad Error/Result), R3=1 (moderate selectivity),
R4=2 (one dual-answer input, definition and concept), R5=2 (empty/whitespace),
R6=1 (semantic building), R7=2 (mixed-selectivity alternation). Each has
`row_source`, `answer_key_basis`, and `answer_kind: concept|definition`.
`author_split_rows.py` reproduces the hand splits of retained census queries and
labels constructed inputs honestly rather than giving them invented telemetry.

The unchanged binary runs **joined form**: `query + " " + pattern` when the
pattern has non-whitespace content; otherwise it sends exactly `query`, with no
trailing space. A split-capable binary sends `query` and `pattern` separately,
including an empty pattern. Capability is probed on the actual binary with
`pattern: "["`: `success: false, code: invalid_pattern` means supported,
success means the legacy engine ignored the parameter, and any other error is a
hard fault. The score records `pattern_probe`, `pattern_declared`, and each split
row's `input_form`. Joined references are measurement of the old engine, never
split measurements, so the split predicates (the joined-candidate refusal, the
paired rule and the R1-R6 requirements below) do not judge a joined row. When
the probe reports `ignored_pattern`, the engine cannot run split form at all:
its joined split rows are recorded as not applicable and skipped by those
predicates under every descriptor class. The score names them in
`split_rows_not_applicable` (rows and reason), and both the runner and the gate
print a `split_rows_not_applicable:` line. They still count in the real-query
aggregates, which compare the joined replay with the joined reference. Once an
engine honours `pattern`, its candidate must be split form: a joined row then
faults with `split_candidate_required`, and every split predicate applies.

Every split row also runs **prose-only**, at the same scoring depth and page
profile. Paged replays verify invariance for both forms. The score carries the
prose requests, paths, metrics and `paired_mrr_delta`; the runner prints the
per-row deltas. The ranking predicate fails if **any concept-answer row** has
lower MRR@10 with its pattern than without it, regardless of aggregate gains.
Definition answers are not protected by that paired rule. R1 additionally needs
rank 1, R3 hit@5, R4 both answers hit@3, and R6 a hit with `complete: false`.
R5 compares the actual result arrays with prose-only, not just their scores.

A `ranking` descriptor may name split rows that no pattern design can satisfy
in `unreachable_split_rows`: a list of `{episode_id, predicate, reason}`, where
`predicate` is one of the absolute split predicates (`split_rank1_required`,
`split_hit5_required`, `split_hit3_required`, `split_partial_hit_required`) and
must be the one that judges the row's split kind. `split_paired_harm` and every
non-split predicate can never be waived, and a malformed entry faults the gate.
The gate honours an entry only after checking the run's own data: (a) the row's
split rank equals its prose-only rank, so the pattern did not move the answer,
and (b) no line of the gold answer file at the evidence pin matches the row's
pattern, so no pattern design could lift it. A waived row is printed as
`unreachable_split_row_waived:` with both ranks and the reason; when a check
fails the failure stands, next to an `unreachable_split_row_refused:` line
naming the check.
The reference seeds `fixture_groups.real_query.split` and `shapes.split`.

R3's anchored declaration alternation matches exactly 30 files at the pin,
checked by a named harness test. `expected_prose_overlap: 5` is informational:
five selected declarations concern output compression, and the other files
concern unrelated systems. Actual prose-lane membership depends on the semantic
index, so exact overlap is not a gate. Split scores preserve the pattern summary
and the first eight text lines for inspection. R6 uses a separate AFT process
whose corpus embeddings are held by an event while query embeddings and the
model-initialization probe remain available. The runner waits for the trigram
index's `ready` status and the semantic index's `building` status (or its initial
`loading / embedding_symbols` stage);
it does not race a normally progressing semantic index or alter the 56 old rows.

`split-tuning-manifest.json` contains **8 additional tuning-only rows**
(`920001`–`920008`), with disjoint query/pattern pairs. Neither these rows nor
their scores can enter the gate/reference. They are for search-engine developers to
tune weights, damping and RRF constants, without training on the gate answers:

```bash
python3 benchmarks/aft-search/run_real_query.py --tuning-only \
  --manifest benchmarks/aft-search/split-tuning-manifest.json --profile paged \
  --binary target/release/aft \
  --exact-score benchmarks/aft-search/.bench/search-quality/exact.json \
  --concept-score benchmarks/aft-search/.bench/search-quality/concept.json \
  --output benchmarks/aft-search/.bench/search-quality/tuning.json
```

`split-query-vectors.bin` supplements, rather than replaces, the original chunk
and query pack. It holds real MiniLM vectors for both the joined and prose-only
texts of gate and tuning rows. The manifest binds its digest; the loader verifies
model/pin/template/dimension and refuses conflicting overlap with the old pack.
The existing row bindings, original pack, and concept query pack stay untouched.
To reproduce authoring (requires the managed model cache):

```bash
python3 benchmarks/aft-search/author_split_rows.py
cd benchmarks/aft-search
uv run --with onnxruntime==1.24.4 --with tokenizers==0.22.2 --with numpy \
  python3 capture_split_query_vectors.py --allow-vector-authoring
```

Tuning rows added later (920009-920011, concept answers that call the named
symbol) carry their vectors in `split-tuning-vectors.bin`, bound only by the
tuning manifest, so the gate's pack and manifest stay byte-identical:
`capture_split_query_vectors.py --allow-vector-authoring --tuning-only`.
Set `AFT_SEARCH_SPLIT_TRACE=1` in the engine's environment to record each split
row's `split_trace` (how every leading result was placed) in the score.

The gate pack also holds the query text of every gate row that
`real-query-vectors.bin` has no vector for: rows whose shape ran no semantic
lane when the main pack was captured (identifiers, regexes, and prose the
router took for a code literal). An engine that routes such a text to the
semantic lane is then measured instead of faulting with `vector_missing`.
The capture embeds only texts the pack lacks; every vector already in it keeps
its bytes, and each run rebinds only its own manifest.

Rows `910013` and `910014` (kind R7) rebuild a report from the magic-context
repository, which is not a benchmark corpus, on the pin. There, `query: "Pi
auto-search hint selects latest user message to search on; skips synthetic or
custom messages"` with `pattern: "autoSearch|auto_search|runAutoSearch"`
returned release notes and reports first, and the summary line named a local
`const autoSearch` inside a dashboard component as the definition, while the
answer, `auto-search-pi.ts`, declares `runAutoSearchHintForPi`. Two engine
behaviours combine: prose with a comma or semicolon is routed as a code
literal, so the query runs without its semantic lane, and a declaration whose
name only begins with an alternative (`runAutoSearch` in
`runAutoSearchHintForPi`) does not count as that alternative's definition.
Both rows keep that shape: a sentence with a comma, and a camelCase,
snake_case and longer-name alternation whose answer declares a longer name
(`resolveAftConfigPaths` in the Pi plugin's `config.ts`;
`resolve_cross_file_edge` in `callgraph.rs`, whose short name
`resolve_cross_file` appears in 18 Markdown files). Their answer is the
declaring file, so `answer_kind` is `definition`.

Rows `900035`–`900037` (mechanism `wrong_lane_nl`) are query-only sentences
with a comma or semicolon and no identifier, whose answer is found by meaning
rather than shared words: where the search index decides a file is binary,
how adjacent hashline replacements are merged, and how a timed-out background
task's process group is killed. Like the split rows they are constructed from
the pin after that report, not taken from telemetry.

Descriptor suggestion for a change adding benchmark rows and harness support: `slice_class: non_ranking`,
`kind: harness`, `targeted_mechanism: none`, `fixtures: ["harness-goldens"]`.
For the subsequent engine implementation change: `slice_class: ranking`, `kind: ranking`,
`targeted_mechanism: split_query_pattern_fusion`; its MRR@10 must strictly improve,
with exact/concept recall, shape floors, page invariance and paired checks intact.

### Hyphenated-literal rows

Rows `followup-census:900010`–`900017` (mechanism
`phrase_present_not_surfaced`, shape `identifier`) are single kebab-case
literals an agent searches for by name: a hash domain tag
(`aft-ignore-rules-v1`), a log reason code (`drop-no-compatible-handler`), an
LSP binary name (`haskell-language-server-wrapper`), a request id
(`doctor-removal-status`), a digest constant (`aft-escalation-payload-v3`), a
cargo directive (`rerun-if-changed`) and two thread names (`aft-lsp-idle-reap`,
`aft-mem-sampler`). Each occurs in exactly one file at the pin, and that file
is the answer. They measure whether a hyphenated query is searched as one
string: an engine that splits it into its parts lets a field named after one
part, or three lines mentioning all parts, count as exact evidence and
outrank the file that holds the literal. Identifier-shaped queries run no
semantic lane, so the rows need no query vectors.

### Definition, receiver.member and missing-name rows

Rows `followup-census:900018`–`900034` are identifier queries (shape
`identifier`, no semantic lane, so no query vectors) for three engine
questions:

- `900018`–`900020`, mechanism `identifier_not_definition_first`. The answer
  is the file that declares the name. `900018` (`line_col_to_byte`, a real
  agent query from Synapse's rerank-eval corpus, id `aft-030`) has a planning
  note under `.gsd/` that quotes the declaration and sorts before
  `crates/aft/src/edit.rs` by path. `900019` (`ctx.set_harness`) and `900020`
  (`adapter.clearPluginCache`) are census `receiver.member` queries: their
  text occurs only at call sites, and the answer is the declaration on the
  receiver's type (`context.rs`, the `HarnessAdapter` interface in
  `adapters/types.ts`).
- `900021`–`900034`, mechanism `renamed_or_variant_token`. Every census
  episode on this repository whose query is one identifier-shaped token that
  occurs in no file at the pin, and that the agent followed by opening a
  file. The answer is the file the agent opened next. `answer_key_basis`
  names the closest name that file holds at the pin, if any. These measure
  what an agent gets for a name that does not exist.

The score records ranked files only, so no row checks which line a result is
rendered at.

### Re-recording the reference

`real-query-baseline.json` and its `manifest.sha256` sidecar are the byte-equality
reference: the `engine_unwired` class asserts every ranked row is byte-equal to
it. A presentation-only repair may declare `presentation_rows`, a map from
episode id to `{"before": ["exact reference lines"], "after": ["exact replay lines"]}`.
Only `summary_text` on those rows may differ, and both arrays must match exactly.
The gate refuses unlisted changes, mismatched lines and listed rows that did not
change, naming the row and field. Ranked paths, pattern summaries, metrics and
all existing family checks remain strict. This field is refused on other
descriptor classes. Copy its lines from a real release-binary replay, not an
invented expectation; re-record the reference in a subsequent train once the
presentation repair has landed.

Moving that pair is an audited act, never a repair to make a red gate green.
Re-record only after the cause of the difference is established, on one binary,
with `cost-gate.sh --search-quality --mode record-reference`, and say in the
commit message which change moved the rows and why it was the right direction.

Re-records so far:

- 2026-10-10, when rows `900035`-`900037` and `910013`-`910014` were added
  (see Split query/pattern rows above). Recorded on macOS arm64 from a clean
  release build of the unchanged engine at `bad57cf4e` (`aft 0.59.0`, binary
  sha256 `b6ebfae631a7ad4faba05fc2a78828435c3b8ecebd04ed544587cacf2dd2df1a`),
  in two steps on that binary: `--mode verify` with the old 93-row manifest
  (its split pack bound from `bad57cf4e`), then `--mode record-reference
  --manifest-changed --old-score <that replay> --base-ref bad57cf4e`. The 93
  old rows are byte-equal to the previous reference in every replay, and two
  recordings of the 98-row manifest on that binary are byte-equal row for
  row, so the change is the new rows alone: answer ranks miss, 3 and 2 for
  `900035`-`900037`, and miss for both split rows, with and without their
  pattern. `paged` MRR@10 0.394892 -> 0.383248 (98 rows), census-weighted
  MRR 0.382926 -> 0.379181, `wrong_lane_nl` 0.341026 -> 0.329167,
  `split_query_pattern_fusion` 0.313889 -> 0.269048. Exact recall stayed
  1.000 and concept recall 0.639423, re-measured in the recording run.
- 2026-10-02, after train 271's ranking change `894133741` (search a
  hyphenated query as one literal) landed on `main`, together with the 17
  identifier rows `900018`–`900034` from `9aaef09d5` (see Definition,
  receiver.member and missing-name rows above). Recorded on macOS arm64 from a
  clean release build of unchanged `origin/main` `f15190422` (`aft 0.58.2`, no
  rustc wrapper, binary sha256
  `dd4afd242b4947ef77e0d1e3857cefc6637f5d00af192fb5f79e6dc8cc9db8f4`), in two
  steps on that one binary. First `cost-gate.sh --search-quality --mode
  record-reference` with the old 76-row manifest: exactly nine rows moved, all
  hyphenated single-token queries, the eight hyphenated-literal rows
  `900010`–`900017` (each now ranks its answer first) and `19696`
  (`cortexkit-store`, a miss before and after, whose candidates no longer lead
  with files that only mention the parts); the other 67 are byte-equal to the
  previous reference, and four replays on two builds of that tree produced
  identical rows. `paged` MRR@10 0.272290 -> 0.371601, hit@5 0.381579 ->
  0.486842, census-weighted MRR 0.308773 -> 0.455716, `identifier` shape
  0.182540 -> 0.601852, `phrase_present_not_surfaced` 0.178571 -> 0.933333.
  Then `--mode record-reference --manifest-changed --old-score <old-manifest
  replay> --base-ref <first step>` with the 93-row manifest: all 76 old rows
  are byte-equal to the first step, so the remaining change is the new rows:
  `paged` MRR@10 0.371601 -> 0.338471 (93 rows), hit@5 0.486842 -> 0.451613,
  census-weighted MRR 0.455716 -> 0.335822, `identifier` shape 0.601852 ->
  0.401984, `identifier_not_definition_first` 1.000 -> 0.850 (the three new
  rows each rank their answer second), new `renamed_or_variant_token` 0.124008
  (ranks 2, miss, miss, miss, 1, miss, miss, miss, 9, 8, miss, miss, miss,
  miss in row-ID order). All 93 rows are byte-equal to the reference
  `9aaef09d5` recorded independently in a Linux aarch64 container at
  `597e3d13c`. Exact recall stayed 1.000 and concept recall 0.639423. The
  reference also gains the `split_rows_not_applicable` field, which the
  harness has written since the joined split-row change.
- 2026-10-01, when the eight hyphenated-literal rows were added (see
  Hyphenated-literal rows above). Recorded in a Linux aarch64 container on a
  release build of the unchanged engine (`aft 0.58.1`, binary sha256
  `0b2b34cce5ba...`), with `run_search_quality.py --mode record-reference
  --manifest-changed`. The same binary with the old manifest reproduced all 68
  reference rows byte-for-byte, and with the new manifest those 68 rows are
  byte-equal again. `paged` real-query MRR@10 0.297672 -> 0.272290 (76 rows),
  hit@5 0.426471 -> 0.381579, census-weighted MRR 0.436247 -> 0.308773; the
  `phrase_present_not_surfaced` mechanism 0.666667 -> 0.178571 and the
  `identifier` shape 0.283333 -> 0.182540. No old row moved: the changes are
  solely the new rows, whose answer ranks are, in row-ID order, 6, miss, miss,
  miss, miss, 7, miss, 7 (MRR@10 0.056548 across the eight). Exact recall
  stayed 1.000 and concept recall 0.639423, re-measured in the recording run.
- 2026-09-29, when 12 query/pattern rows were added (see Split query/pattern
  rows above). Recorded in Linux x86_64 on the unchanged local-main release
  build `d361e160d142` (`aft 0.58.0`, binary sha256
  `0c8b5d45de80313d8842d31a4ddc2b68a156c7824bc90a04ba635db180df0325`).
  The binary's invalid-pattern probe reported legacy/ignored input, so all
  new rows are joined-form references. The old manifest replay reproduced all
  56 reference rows byte-for-byte, and the expanded replay preserved those
  same 56 rows byte-for-byte. A second expanded replay reproduced all 68 rows.
  Exact recall stayed 1.000 and concept recall stayed 0.639423, re-measured.
  `paged` real-query MRR@10 0.317708 -> 0.297672, hit@5 0.446429 -> 0.426471;
  the added split mechanism/shape/fixture group has MRR@10 0.204167,
  hit@1 0.166667, hit@5 0.333333. Census-weighted report-only MRR
  0.434615 -> 0.436247. No old answer moved: aggregate changes are solely
  the addition of new rows, not a ranking change. Joined answer ranks, in
  row-ID order: miss, miss, miss, miss, miss, 1, 5, miss, 4, 1, miss, miss.
  R6's held semantic build returned `complete: false`; the initial index reports
  `loading / embedding_symbols` while unpublished, and search reports `building`.
  The gate was green with a `non_ranking`/`harness` descriptor against
  `d361e160`, the local-main base. `origin/main` was still behind that base's
  engine-unwired per-checkout-index change, so the local-main base avoids mixing
  its descriptor class with this benchmark change.

  Recording procedure: old-manifest `cost-gate.sh --search-quality --mode verify`
  on that binary, then `--mode record-reference --manifest-changed --old-score
  <old-score> --base-ref d361e160`. The real-query stage initially failed because
  the fixture also held model initialization; after exempting the initialization
  probe, awaiting `embedding_symbols`, and permitting the explicit building
  response for that fixture client only, the failed stage was rerun and the
  resulting score recorded with `search_quality.py` using the same arguments.
  A full `cost-gate.sh --search-quality --mode evaluate` replay then passed.

- 2026-09-18, after ranking slice 3, and 2026-09-20, after the `includeTests`
  coercion moved the capability schema hash. Both are written up in
  `docs/investigations/search-grep-sweep-2026-09-17.md`.
- 2026-09-21, after the answer-key corpus exclusion in commit `0b14e8b2d`. That
  commit stopped the benchmark's own fixtures and recorded reports from being
  indexed for the real-query replay, which changed the candidate pool and moved
  26 of the 43 rows; see the corpus hygiene section below. Cause established by
  running the replay twice on one binary, once with and once without the
  evidence-tree ignore list, against the same manifest, vector pack and pinned
  tree digest: without it every row is byte-equal to the old reference, with it
  the score reproduces the numbers CI reported. Re-recorded on a release build of
  `aft 0.56.2`: 43 rows, `paged`, MRR@10 0.187984 -> 0.188760, census-weighted
  MRR 0.151837 -> 0.152462. Exact recall and concept recall were re-measured in
  the same run and are unchanged at 1.000; the ignore list is copied only into
  the real-query evidence tree, and those two families score against the pinned
  external clones and the offline vector pack instead.
- 2026-09-25, after JSON data files were demoted below source in `aft_search`
  (source-before-data inside each exact evidence kind, and a 20-position
  demotion in the lexical and semantic lanes). Cause established by replaying
  the unchanged reference on the base binary first (all 43 rows byte-equal),
  then replaying the change twice on one release build of `aft 0.57.2`: the two
  runs are byte-equal to each other. 35 rows changed bytes; two moved their
  opened file, both upward: `followup-census:7617` 4 -> 3 and
  `followup-census:7956` 6 -> 5. No row's opened file moved down. `paged`
  MRR@10 0.188760 -> 0.191473, hit@5 0.325581 -> 0.348837, census-weighted
  MRR 0.152462 -> 0.152885, `phrase_present_not_surfaced` MRR@10
  0.625 -> 0.667. Exact recall and concept recall unchanged at 1.000.
- 2026-09-26, after the vector pack moved from hashed 8-number stand-ins to
  real all-MiniLM-L6-v2 vectors (`real-query-vectors.bin`). Recorded with
  `record-reference --manifest-changed` on the unchanged engine, in a Linux
  aarch64 container (`aft 0.57.2`, binary sha256 `a348ac53d8f6...`). The old
  score, taken on the same binary with the old pack, reproduced the old
  reference on all 49 rows, and three new-pack runs were byte-identical. 30 of
  49 rows changed bytes. Four moved their opened file: `followup-census:14613`
  2 -> 3, `15174` 2 -> 3, `14964` unranked -> 8, `18091` unranked -> 2.
  `paged` MRR@10 0.214286 -> 0.220238, hit@5 0.346939 -> 0.367347,
  census-weighted MRR 0.158049 -> 0.162052. By mechanism,
  `index_stale_or_missing` 0.139394 -> 0.120455 and `other` 0.100 -> 0.200;
  every other mechanism is unchanged, including `wrong_lane_nl` at 0.328205.
  Recorded on aarch64. Every ranking step is plain IEEE float arithmetic,
  which gives the same bits on x86_64. The one exception is the lexical
  lane's `ln()` of a file's trigram count (`search_index.rs`,
  `lexical_score_from_postings`), which calls the platform's libm (glibc
  `logf`, which has an FMA variant on x86_64). A CI-only mismatch on
  lexical-heavy rows would point there first.
- 2026-09-26, when concept recall began running AFT (see Concept recall
  above). Only the concept family and its fixture groups moved. The engine
  was unchanged. Recorded in a Linux aarch64 container on one release build
  of the base commit (`aft 0.57.2`, binary sha256 `17fba2ca8c9b...`), with
  `search_quality.py --mode record-reference --manifest-changed`. The
  manifest itself did not change; that mode is used because it also checks
  that a score taken with the old harness on the same binary is green against
  the old reference, and it was: all 49 real-query rows were byte-equal.
  Three replays with the new harness were byte-identical (concept score and
  full score). Concept MRR@10 1.000 -> 0.542857, hit@1 1.000 -> 0.428571,
  hit@5 1.000 -> 0.678571. By fixture group, MRR@10 is identifier 0.753333,
  mixed 0.425, natural-language 0.283333, path 1.000, error-code 0.5625,
  generic-file 0.000. Every case scored 1.000 before; the table gives each
  case's answer rank after. Real-query and exact-recall numbers are unchanged.

  | Case | Group | Rank | MRR@10 |
  | --- | --- | --- | --- |
  | `useState` | identifier | 3 | 0.333 |
  | `aft_safety_history` | identifier | 5 | 0.200 |
  | `subagent_type` | identifier | none | 0.000 |
  | `LSPManager` | identifier | 1 | 1.000 |
  | `handle_grep` | identifier | 1 | 1.000 |
  | `BinaryBridge` | identifier | 1 | 1.000 |
  | `validate_storage_dir` | identifier | 1 | 1.000 |
  | `SemanticIndexFingerprint` | identifier | 1 | 1.000 |
  | `renderSemanticResult` | identifier | 1 | 1.000 |
  | `trust_project` | identifier | 1 | 1.000 |
  | `useState hook examples` | mixed | none | 0.000 |
  | `LSPManager initialization timeout` | mixed | 1 | 1.000 |
  | `semantic_search unavailable renderer` | mixed | 2 | 0.500 |
  | `bash long running reminder config` | mixed | 8 | 0.125 |
  | `workspace permission prompt flow` | mixed | 2 | 0.500 |
  | `how does semantic indexing persist across configure runs` | natural-language | 4 | 0.250 |
  | `where are background bash tasks restored after restart` | natural-language | 6 | 0.167 |
  | `how are configure warnings delivered asynchronously` | natural-language | 6 | 0.167 |
  | `where does lsp diagnostic polling wait for server responses` | natural-language | 3 | 0.333 |
  | `how are imports organized after edits` | natural-language | 2 | 0.500 |
  | `crates/aft/src/commands/grep.rs` | path | 1 | 1.000 |
  | `packages/opencode-plugin/src/tools/bash.ts` | path | 1 | 1.000 |
  | `packages/aft-bridge/src/bridge.ts` | path | 1 | 1.000 |
  | `ERR_PNPM_` | error-code | 1 | 1.000 |
  | `process group already terminated` | error-code | 8 | 0.125 |
  | `AFT plugin entry point` | generic-file | none | 0.000 |
  | `command module exports list` | generic-file | none | 0.000 |
  | `crate public API modules` | generic-file | none | 0.000 |
- 2026-09-26, after four concept answer keys were corrected or removed and
  `.alfonso/reports/search-fusion-quality.md` was excluded from the
  evidence-tree index (see Concept recall and the corpus hygiene section).
  The engine was unchanged: one release build in a Linux aarch64 container
  (`aft 0.57.2`, binary sha256 `17fba2ca8c9b...`, the same binary as the
  previous entry), `paged` profile. The base harness reproduced the old
  reference: every real-query row equal, and a concept score byte-identical
  to the one recorded in the previous entry. Two runs of
  `cost-gate.sh --search-quality --mode record-reference --dry-run
  --manifest-changed` on this harness gave byte-identical scores, recorded
  with `search_quality.py --mode record-reference --manifest-changed`; the
  second run evaluates `real_query_behavior:equal` against the new pair.

  Concept recall, 28 cases -> 26: MRR@10 0.542857 -> 0.639423, hit@1
  0.428571 -> 0.538462, hit@5 0.678571 -> 0.769231. The answer-key step
  alone gives 0.618269 (hit@1 0.5, hit@5 0.769231), the exclusion the rest.
  By group, MRR@10: identifier 0.753333 -> 0.842593 (nine cases), mixed
  0.425 -> 0.65625 (four), error-code 0.5625 -> 1.000; natural-language,
  path and generic-file unchanged. Rows that moved, with the answer rank
  before the change, after the answer-key fix, and after the exclusion:

  | Case | Before | Keys fixed | Report excluded | Why |
  | --- | --- | --- | --- | --- |
  | `process group already terminated` | 8 | 1 | 1 | answer is now `process.rs` |
  | `semantic_search unavailable renderer` | 2 | 2 | 1 | `readonly_artifacts.rs` went from 1 to 2; the report was not in this top ten, so this is the smaller candidate pool at work |
  | `aft_safety_history` | 5 | 5 | 4 | the report was ranked 1 |
  | `subagent_type` | none | removed | removed | |
  | `useState hook examples` | none | removed | removed | |

  The report also sat in the top ten for `useState` (at 5, answer at 3) and
  `LSPManager` (at 3, answer at 1); neither answer moved.

  Real query: no row's opened file moved, and every metric is unchanged
  (`paged` MRR@10 0.220238, census-weighted MRR 0.162052). Seven of 49 rows
  changed bytes. In each the report had been one of the candidates, and
  `retrieval_depth` drops by one: `followup-census:3184`, `10215`, `10672`,
  `17208`, `18091`, `19696` and `900002`. Two of them changed their ranked
  list as well. `900002` had the report at 5 and its opened file at 1
  (still 1). `18091` swapped two files at 5 and 6, and its opened file
  stays at 2. The drop of exactly one candidate in each changed row also
  shows that the root `.aftignore` written into the copy did not itself
  enter the index. Exact recall is unchanged at 1.000; it scores against the
  pinned external clones, not the evidence tree.
- 2026-09-29, when seven regex identifier rows were added
  (`followup-census:900003`-`900009`, mechanism
  `identifier_not_definition_first`, #375). Each query is an identifier
  alternation an agent ran in this repository, and its answer is the file
  that declares the identifiers; the row's `row_source` and
  `answer_key_basis` say where it came from and where the declaration sits
  at the pin. The engine was unchanged: one release build of the base commit
  in a Linux aarch64 container (`aft 0.58.0`, binary sha256
  `bdcbf20ba992...`), `paged` profile. That binary with the old manifest
  reproduced the old reference, all 49 rows byte-equal; with the new manifest
  the 49 rows are again byte-equal. The old regex route broke page invariance
  on these rows, so six of them carry `reference_not_page_invariant` (see
  Rows the reference may record as a miss) and five were recorded as misses;
  `900008` replayed invariantly and scored rank 1, `900009` (no flag) rank 1.
  Two recording runs gave scores that differ only in `baseline_sha256`, the
  recorder's note of the reference file present when each ran. Real-query `paged` MRR@10
  0.220238 -> 0.228423 (56 rows), census-weighted MRR 0.162052 -> 0.239927,
  the new `regex` shape and `identifier_not_definition_first` mechanism both
  MRR@10 0.285714. Exact recall (1.000) and concept recall are unchanged.
- 2026-09-29, after train 238 ranked the complete regex/literal candidate set
  before cutting each page (#375). Replayed the unchanged 56-row reference on
  release builds of `57fb117b9` and `2b7e26e4e` in the same Linux aarch64
  container. The old binary reproduced every reference row byte-for-byte;
  the new binary changed only the seven regex rows, `followup-census:900003`–
  `900009`. Rows `900003`–`900007` went from recorded page-invariance misses
  to invariant rank-1 hits. Row `900008` was already a scored rank-1 hit,
  but its retrieval depth fell 31 -> 3 and two lower-ranked paths swapped.
  Row `900009` kept its ranked paths and rank-1 hit while its retrieval depth
  fell 8 -> 2. The complete-set ranking removes scan-order paging defects
  without displacing any other row. Re-recorded with
  `scripts/telemetry/cost-gate.sh --search-quality --mode record-reference`
  on the release build of `2b7e26e4e` (Linux aarch64, binary sha256
  `11ade1376560...`). `paged` MRR@10 0.228423 -> 0.317708, hit@5 0.357143
  -> 0.446429, census-weighted MRR 0.239927 -> 0.434615; the
  `identifier_not_definition_first` MRR@10 rose 0.285714 -> 1.000000.
  Exact recall stayed 1.000 and concept recall stayed 0.639423, both
  re-measured in the recording run. The six `reference_not_page_invariant`
  flags remain for a separate change.

## Measuring a reranker (report-only)

`search.rerank` is off in the product and in every runner above. Environment
variables, read by `bench_rerank.py`, make the exact-recall, concept-recall and
real-query runners configure one instead, without changing a run when unset:

- `AFT_SEARCH_BENCH_RERANK`: the `search.rerank` block as JSON, for example
  `{"backend": "onnx", "model": "gte-reranker-modernbert-base", "timeout_ms": 15000}`
  or `{"backend": "remote", "endpoint": "http://127.0.0.1:8091", "model": "..."}`.
- `AFT_SEARCH_BENCH_RERANK_CACHE`: the model cache the ONNX reranker reads
  (it becomes the AFT process's `FASTEMBED_CACHE_DIR`). The pack replays block
  downloads, so put the pinned snapshot there first, and nothing else: the
  replay must not find an embedding model to fall back on. The ONNX backend
  also needs `ORT_DYLIB_PATH` pointing at the managed ONNX Runtime.
- `AFT_SEARCH_BENCH_RERANK_LOG`: a JSON-lines file of every search (latency
  and the `rerank skipped: ...` note when there was one), plus each runner's
  warm-up. The scores keep their normal shape; the log is how a reader sees
  whether the reranker ran or was skipped.
- `AFT_SEARCH_BENCH_SETTLE_SECONDS`: after the index reports ready, wait at
  least this long and until the semantic entry count is steady. It works
  around the macOS watcher burst described above: with `30`, a macOS
  `paged` replay of the unchanged engine reproduced the Linux reference
  byte-for-byte on all 68 rows.

Before the scored rows, each runner lets the reranker score the tuning-only
split rows' queries (never scored, and present in the vector packs), so no row
is measured against a backend that is still loading or on its slow first
inference. Use a generous `timeout_ms` to measure ranking; with the default
1.5 s a CPU backend times out under load, and a timed-out list keeps fused
order.

`run_rerank_probe.py` measures the rest on the same pinned AFT evidence tree
and vector packs as the real-query replay:
config-to-installed time and memory on a tiny project, cold and warm
per-search latency, resident memory and CPU time sampled every 100 ms (also
for an out-of-process backend with `--extra-pid`), in-process repeat and
paging consistency, and, with `--proxy-upstream`, the backend stalling,
answering 503, recovering and being killed. `rerank_score_proxy.py` is the
loopback adapter it puts in front of `llama-server --reranking` for that.

## Prefrontal search-miss rows

`prefrontal-search-fixtures.json` holds real `aft_search` queries an agent
typed while working in the prefrontal repository, each with its known answer
as file and line ranges read at the commit pinned in `corpus/prefrontal.toml`,
and a `failure_class` tag. `run_prefrontal_search.py` replays them and records,
per row, the rank of each known answer and every result ranked above it.

```bash
python3 benchmarks/aft-search/provision_corpus.py --corpus benchmarks/aft-search/corpus/prefrontal.toml
cd benchmarks/aft-search
python3 run_prefrontal_search.py --out results/prefrontal-search-<date>.json
```

The rows sit outside the real-query gate because the gate cannot hold them:
every real-query row must share AFT's own pinned tree and vector pack, and a
row scores against one opened file with no line range. prefrontal also stays
out of `corpus/corpus.toml`, which the exact-recall gate reads and which needs
sentence and pair fixtures for every repository it lists. The runner calls the
public `search` tool with the live local embedding model, the same surface and
model the agent used, so it needs prefrontal access and the managed ONNX
Runtime. It is report-only and never fails on a ranking outcome.
`results/prefrontal-search-baseline.json` is the first recorded run; compare a
ranking change against it row by row.

## Recall audit and named cases

`run_recall_audit.py` says where each known answer is lost: never indexed,
not produced by any lane that ran, produced beyond a candidate limit, ranked
below the page, or shown through another span of the same file. It replays the
real-query rows (with the gate's vector pack, the live local model, or both),
the prefrontal rows, and `named-case-fixtures.json`, and records each reply's
confidence label beside whether the answer reached the top 5.

It relies on a benchmark-only switch in the binary: `AFT_SEARCH_RECALL_AUDIT=1`
in the aft process environment adds a `recall_audit` object to engine-ranked
search replies, and `AFT_SEARCH_RECALL_AUDIT_TARGETS` names the files to
report index coverage and unlimited-depth lane ranks for. It is not a tool
parameter. The runner starts its own binaries with temporary storage and never
uses a shared daemon.

`named-case-fixtures.json` holds report-only rows for failure modes the gate
has no row for: punctuated prose, prose with one exact fragment, op-string
dispatch, a partial anchor that outranks the answer, and validated no-answer
queries. Each row states how its answer, or its absence, was verified.

```bash
python3 run_recall_audit.py --real-query-backend both \
  --out results/recall-audit-<date>.json --markdown .bench/recall-audit/tables.md
python3 -m unittest -v test_run_recall_audit
```

The first run is written up in
`docs/investigations/search-recall-audit-2026-09.md`.

## Search-fusion quality sub-benchmark

`run-fusion-quality` is a focused investigation harness for hybrid fusion
ranking. It runs the existing in-tree `fixtures.json` plus
`identifier-fusion-fixtures.json`, asks `aft_search` for a wide top-100
candidate pool, and applies bench-only offline rerankers (RRF, exact-identifier
first, and uncapped identifier lexical score) without changing production Rust.
From `benchmarks/aft-search`:

```bash
python3 run-fusion-quality \
  --binary ../../target/release/aft \
  --project-root ../.. \
  --out results/search-fusion-quality.json \
  --summary results/search-fusion-quality-summary.tsv
```

The script auto-detects the managed ONNX Runtime at
`~/.local/share/cortexkit/aft/onnxruntime/1.24.4/` when `ORT_DYLIB_PATH` is not
set (on Windows, `%LOCALAPPDATA%\cortexkit\aft\onnxruntime\1.24.4\onnxruntime.dll`).
It writes a detailed JSON run manifest plus a TSV aggregate summary used by
`.alfonso/reports/search-fusion-quality.md`.

## Corpus hygiene: the benchmark's own answer key

`fixtures.json`, `identifier-fusion-fixtures.json`, `baseline.json` and the
recorded reports each contain every fixture's query **and** its
`expected_top_files`. When they are part of the indexed corpus they become
self-fulfilling exact hits for their own queries, crowd the top of the result
list, and push down the file the fixture is actually looking for.

`.aftignore` in this directory keeps them out of the index AFT builds, matching
the exclusion `run-fusion-quality` already applies to its bench-only
exact-match oracle. `run_real_query.py` copies the same list into the runtime
copy of the evidence tree, after the pinned digest has been verified, so the
digest is unaffected.

The pinned tree also carries `.alfonso/reports/search-fusion-quality.md`, a
report that quotes the concept fixtures' queries. It ranked first for
`aft_safety_history` and `subagent_type`. The list above cannot reach it: an
`.aftignore` only applies below its own directory, as a `.gitignore` does.
The repository's `.gitignore` covers `.alfonso/`, but the evidence-tree copy
has no `.git` directory, so AFT does not apply it there. (The comment in this
directory's `.aftignore` says otherwise; it holds for the live checkout, not
the copy. That file is left unchanged here: it is copied into the evidence
tree, so editing it could change the indexed corpus too.) `evidence-root.aftignore` lists the
report, and `run_real_query.py` writes it as `.aftignore` at the root of the
same runtime copy. Concept recall, the real-query replay and every other
runner that uses `runtime_evidence_tree` get the exclusion together. The file
also lists itself, so the only change to the candidate pool is the report.

Changing what the harness indexes is a ranking change, even when the query sets
do not intersect. The evidence-tree copy was added on the reasoning that no
real-query row shares a query with a fusion fixture, so it could not matter; that
does not follow. Non-intersecting queries say nothing about the corpus, and
removing documents changes the candidate pool every query competes in. It moved
26 of the 43 real-query rows. Twelve of them had an excluded file inside their
own recorded result list; the other 14 never named one and moved through the
pool alone. One row's metrics moved, because `results/aft-vera-suite-baseline.json`
had been ranked above the file that episode opened and pushed it out of the top
five; the other 25 rows changed bytes in the deeper recorded list without moving
a metric, which is why the reference asserts byte equality rather than metrics.
The change landed under a `non_ranking` descriptor, which does not compare rows,
so the drift went unmeasured until the next `engine_unwired` descriptor refused
on it. A commit that adds or removes an ignore entry, or otherwise changes which
files reach the index, belongs under a descriptor class that compares rows and
carries the re-recorded reference with it.

The deflation this prevents runs in the dangerous direction: a suppressed
baseline makes every candidate ranking change look better than it is. Any new
file in this directory that carries `expected_top_files` has to be added to
`.aftignore`; `test_harness_integrity.py` fails until it is.

`baseline.json` records where its numbers came from in a `measured_on` block,
the way the cost-gate baselines do, including whether the answer-key files were
excluded from the indexed corpus at capture time.

## Running on Windows

All stages except the real-query replay run on Windows.

- The NDJSON clients read the aft process's stdout on a reader thread rather
  than with `select.select()`, which on Windows accepts sockets only and fails
  a pipe with `WinError 10038`.
- AFT returns absolute paths in the `\\?\C:\...` verbatim form.
  `normalize_result_path` strips that prefix, without which every comparison
  against a relative `expected_top_files` entry misses and the run reports a
  clean 0.000 on a healthy index.
- `.gitattributes` pins the benchmark JSON to LF, so the vector pack still
  matches its `embedding_pack_sha256` under `core.autocrlf=true`.
- The real-query replay refuses to start and exits **3**: its vector pack and
  baseline are Unix-captured, and the text AFT embeds bakes the OS-native
  relative path into every chunk header, so a Windows run cannot reproduce
  them. Exit 3 is distinct from the exit 2 used for ordinary input faults.

The platform-independent cases live in `test_harness_integrity.py` and run
everywhere:

```bash
cd benchmarks/aft-search
python3 -m unittest -v test_harness_integrity
```

## Method notes

**Patches must be installed before the harness is imported.** The runners do
`from run import normalize_result_path`, which binds the name at import time.
Driving one of them from an external script and patching
`run.normalize_result_path` afterwards leaves the harness holding the original
function, and the run looks like an ordinary 0.000 rather than a failed patch.
The same applies to anything else imported by name. For the same reason a fix
belongs in the primitive rather than in a caller: `run_search_quality.py`
spawns each stage as its own `python <script>.py`, so a second client class
running as `__main__` never sees a patch applied to the first.

**A zero is refused, not reported.** The baseline and fusion-quality runners
refuse to write a report in which no fixture matched at all while the semantic
index held entries. A well-formed all-unmatched report is indistinguishable
from a catastrophic ranking regression, and the likelier cause is that result
paths never compared equal to expected paths.

## External Vera-comparable benchmark

After setup, run:

```bash
uv run python run_external.py
```

To write the committed baseline path explicitly:

```bash
uv run python run_external.py --out results/aft-vera-suite-baseline.json
```

The external runner:

1. Reads `corpus/corpus.toml` and `external-fixtures.json`.
2. Starts one `aft` process per corpus repo with `project_root` set to that clone.
3. Configures `search_index`, `semantic_search`, `experimental_search_index`, and
   `experimental_semantic_search` with a per-run temporary `storage_dir`.
4. Waits up to 600 seconds for the search and semantic indexes to report `ready`.
5. Runs `semantic_search(top_k=10)` for each task in that repo.
6. Scores results with line-range overlap by default.
7. Writes `results/aft-vera-suite-<timestamp>.json` unless `--out` is supplied.

Use `--relevance-mode file-only` only when you intentionally want Vera's
file-path-only mode. The committed baseline uses the stricter default
`line-overlap` mode.

## Metrics and result JSON

`metrics.py` supports both fixture formats:

- In-tree fixtures: file-path match against `expected_top_files`.
- Vera fixtures: `ground_truth[{file_path,line_start,line_end,relevance}]` with a
  prediction relevant only when the file matches and the returned line range
  overlaps the ground-truth range by at least one line.

External result files mirror Vera's report shape at the top level:

- `tool_name`, `timestamp`, `version_info`: reproducibility metadata, including
  binary SHA, repo SHAs, top-k, relevance mode, and reranker status.
- `per_task`: one row per task with ground truth, top results, latency,
  `zero_results`, and `retrieval_metrics`.
- `per_category`: category aggregates for intent, symbol lookup, cross-file,
  disambiguation, and config tasks.
- `aggregate`: overall retrieval metrics and latency p50/p95.

Primary numbers to compare are Recall@1, Recall@5, Recall@10, MRR@10 (`mrr` in
the JSON), nDCG@10, and latency p50/p95.

## What Vera reports vs what we report

| System | Corpus | Reranker | Comparable MRR@10 |
| --- | --- | --- | --- |
| Vera v0.7.0 hybrid | Vera 21-task suite | Cross-encoder reranker on | 0.91 |
| Vera hybrid no-rerank | Vera 21-task suite | Off | 0.34 |
| AFT `aft_search` | Same pinned 21-task suite | Off | See `aggregate.retrieval.mrr` |

Vera's default published number includes a cross-encoder reranker; AFT currently
reports hybrid lexical+semantic retrieval without a reranker. For a fair product
comparison, use Vera's no-reranker baseline from
`Vera/benchmarks/results/final-suite/vera_hybrid_norerank_results.json`.

## Attribution

The external task definitions in `external-fixtures.json` are vendored from
Vera's `eval/tasks/*.json` corpus. The metric formulas in `metrics.py` mirror
Vera's `eval/src/metrics.rs` and are reimplemented independently here rather
than copied wholesale.

## Docker

Run the existing in-tree benchmark without host AFT/Bun/Rust installs:

```bash
bun run bench:aft-search
# or
make run-aft-search
```

The Docker image builds AFT from this checkout and writes `results/aft-search-docker.json` by default. For the external Vera-compatible corpus:

```bash
AFT_SEARCH_MODE=external AFT_SEARCH_OUT=results/aft-vera-docker.json bun run bench:aft-search
```

The compose file mounts `results/` and `.bench/` so reports and cloned corpora persist outside the container.
