# Search-quality reference re-recorded with the symbol-range attribution rows

Two parked engine changes adjust which lines belong to a symbol: one clips
approximate symbol spans at the next declaration (A, `10c2c70fa8`), the other
detaches file-level, detached and trailing comments from symbol ranges (B,
`cd1ab24042`, stacked on A). Neither could be judged by the search-quality gate:
A moved no named mechanism, and B embeds three chunk texts that the frozen vector
pack did not contain, so its replay stopped with `vector_missing`. This harness-only
change adds the rows and vectors both need and re-records the reference on the
unchanged engine. No engine code changes.

## Vectors

`split-query-vectors.bin` grows from 91 to 99 vectors (sha256
`762e56510e4f28505c24341d6b9d5b66755a5ac965f1bafe5d654edf55861310`). The main pack
`real-query-vectors.bin` (`7c0e9a72…`) and every existing supplement vector keep
their bytes. The new vectors are:

- the five query texts of the new rows;
- the corpus vectors of the three chunk texts listed in
  `benchmarks/aft-search/candidate-chunk-texts.json`, each checked against its
  digest (`1281a3a1…`, `1863c548…`, `4d5d3fbc…`). Those are
  `format_test.rs::format_integration_add_import_with_format`,
  `tests/fixtures/sample.rs::public_function` and
  `format_test.rs::format_integration_edit_symbol_with_format` as B chunks them.
  The unchanged engine never asks for them.

They were embedded through `capture_split_query_vectors.py`, which now also reads
`candidate-chunk-texts.json`. It used the pinned `Qdrant/all-MiniLM-L6-v2-onnx`
snapshot `5f1b8cd7…`, with `model.onnx` `bbd7b466…` and `tokenizer.json`
`da0e7993…` both verified by `minilm_embedder`. The runtime was Python 3.12.16,
onnxruntime 1.24.4, tokenizers 0.22.2 and numpy 2.5.4 on Linux x86_64 (remote job
`01a17bdb-eee4-7248-a9e6-7239ae77a539`). The pack header's `source` block matches
the existing pack's, so no metadata changed. With the new pack, the A+B engine
replayed concept recall and the candidate rows without a single refused text.

## Rows

All five rows are constructed from pinned repository content (`row_source` says
so). They use mechanism `other` and are marked with `range_attribution_kind`. Each
was kept because its answer passed relevance review and it is a fair query for
range attribution. None was chosen by which way an engine change moves it.
Router shapes come from `query-shape-helper`.

| Row | Query | Answer | Shape | include_tests | main | A | A+B |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 940001 | `useState` | `crates/aft/src/commands/add_import.rs` | identifier | false | 3 | 2 | 2 |
| 940002 | where are the tree-sitter symbol extraction tests for Rust source files | `crates/aft/src/parser.rs` | natural_language | true | 10 | 10 | miss |
| 940003 | which tests check that tree-sitter extracts every symbol from a sample Rust module | `crates/aft/src/parser.rs` | natural_language | true | 4 | 4 | 3 |
| 940004 | add_import on a Rust file reports whether the response was formatted | `crates/aft/tests/integration/format_test.rs` | mixed | true | miss | miss | miss |
| 940005 | edit_symbol on a Rust file verifies the formatted field in the response | `crates/aft/tests/integration/format_test.rs` | mixed | true | miss | miss | miss |

The ranks come from the real-query runner (paged profile, page-invariance plans
included). Two separate remote jobs gave identical ranks for each engine.

- **940001** promotes the reviewed concept-recall case `useState` and keeps its
  non-test answer. Under A, `add_import.rs` moves above `imports/mod.rs`.
- **940002 / 940003** ask for the parser tests that use `tests/fixtures/sample.rs`.
  On the unchanged engine, `public_function`'s chunk carries the fixture's
  description line (`/// Sample Rust module for tree-sitter symbol extraction
  tests.`, separated by a blank line), and `sample.rs` ranks first. B detaches that
  line, and `sample.rs` leaves the top ten. For 940003, `parser.rs` rises from 4 to
  3. For 940002, `parser.rs` falls out of the top ten.
- **940004 / 940005** paraphrase the `///` lines above two `format_test.rs` tests.
  The answer misses in every engine.

On the pinned tree, B changes exactly those three chunks. Rows built on synthetic
fixture files (a detached Rust doc, a C trailing comment, a license banner, a Ruby
magic comment, and a cross-declaration occurrence decoy for A) cannot be gate rows: every included row must share the single pinned evidence
tree digest, and those fixtures are not in it.

## Recording

The reference was recorded on remote job `01a17c1b-dd11-7419-b694-edfc59121351`
(Linux x86_64, 4 vCPU) from a release build of `2a09cf8419`. That commit is the
unchanged engine at `ecdf3f897a` plus this harness change: `aft 0.62.0`, binary
sha256 `3948ee012011044204d0ba1b6390b137f583a090caaaaf22fd7424c6e16cb65a`. The
command was
`AFT_BINARY_PATH=target/release/aft scripts/telemetry/cost-gate.sh --search-quality --mode record-reference`,
with the corpus provisioned by `provision_corpus.py`, using the `paged` profile.

- Reference sha256 `1b3c4c0cca82cf3a8a4ff571309c77d5080e7ac584590abfaaea6b028c868d56`;
  manifest sha256 `f505399cca50f214066d0183a2228bc30ec680bfc99f1c1370708dfd64a87308`.
- All 103 previous rows are byte-equal to the previous reference. The only change
  is the five added rows, scored 3, 10, 4, miss and miss.
- Real-query MRR@10 0.465060 -> 0.449857 (108 rows); hit@1 0.378641 -> 0.361111;
  hit@5 0.582524 -> 0.574074.
- `other` MRR@10 0.200000 -> 0.168333 (hit@5 0.4 unchanged). The shape `identifier`
  goes 0.490476 -> 0.486111, `mixed` 0.393871 -> 0.366708, and `natural_language`
  0.333333 -> 0.318254.
- Exact recall (1.000) and concept recall (0.668269) are unchanged.
- A same-binary `--mode evaluate` against the new reference with this change's
  `non_ranking` descriptor exited 0 with every row byte-equal.

## What the parked changes would now measure

Both were evaluated in the same job, against the reference above. The engine
paths were passed to `total_gate` explicitly, because the remote copy cannot
commit the patched source.

**A** improves `other` MRR@10 from 0.168333 to 0.185000 (hit@5 0.4 unchanged),
concept recall from 0.668269 to 0.674679, real-query MRR@10 from 0.449857 to
0.451400, and the `identifier` shape from 0.486111 to 0.490741. Only 940001 changes
rank. The only gate failures are the pre-existing `split_rank1_required` misses on
`910001` and `910002`, which miss on the unchanged engine too. Both qualify for the
`unreachable_split_rows` waiver used by `train-identifier-prose-ranking.json`: the
split rank equals the prose rank, and the pattern matches no line of the answer
file. A's descriptor would therefore be
`{"slice_class":"ranking","targeted_mechanism":"other","kind":"ranking","fixtures":["followup-census:940001"]}`
plus those two waivers, with expected strict improvement `other` 0.168333 -> 0.185000.

**B** replayed against a reference recorded from A in the same job (A+B versus A).
It moves 940003 from 4 to 3 and 940002 from 10 to a miss. `other` falls from
0.185000 to 0.183333, real-query MRR@10 from 0.451400 to 0.451246, and the
`natural_language` shape from 0.318254 to 0.317460. Concept recall is unchanged
from A. The gate fails with `targeted mechanism other did not improve` and
`real_query.mrr_at_10 below reference`, besides the waivable split rows. **B stays
parked.** Before it can land, its effect on 940002 must be explained or fixed. In
that row, removing the fixture's description from `public_function` also drops
`parser.rs` out of the top ten. Rust itself attaches a `///` comment to the next
item across blank lines, so treating a blank-line-separated `///` block as
detached is a policy choice that this row measures. B would target `other` with
fixtures 940002-940005. It needs a strict improvement over the A reference, which
it does not have today.
