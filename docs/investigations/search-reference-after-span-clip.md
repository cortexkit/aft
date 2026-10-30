# Search-quality reference re-recorded after the symbol-range span-clip landing

The ranking landing #1055 clips approximate symbol spans at the next
declaration (`scan_symbols_in_text` in
`crates/aft/src/commands/semantic_search/exact_lane.rs`, a ranking-fenced
file). A ranking landing passes the gate against the old reference but does
not update it, so every later `engine_unwired` PR is byte-compared against
results the engine no longer produces. The views batch #1039 failed the
merge-queue search-quality gate with
`engine_unwired_mismatch:row=real_query.followup-census:3184`. This
harness-only change re-records the reference on the landed engine. No engine
code changes.

## Recording

The reference was recorded on remote job
`01a17f33-6e36-7425-bfe7-b34479ae06c` (Linux x86_64, 16 vCPUs) from a release
build of main commit `61a6753544` (merge of #916 declare-tool-result-format,
which declares route-negotiated tool-result support in catalogs; includes the
#1055 span-clip engine change `f4ab22d2e3` plus that non-ranking tool-result
change and the flake-batch-6 test-only timing and race fixes for merge-queue
flakes, per `benchmarks/aft-search/slice-descriptors/land-flake-batch-6.json`).
The corpus was provisioned with
`python3 benchmarks/aft-search/provision_corpus.py`, then
`AFT_BINARY_PATH=target/release/aft scripts/telemetry/cost-gate.sh
--search-quality --mode record-reference` with the `paged` profile.

- Binary `aft 0.62.0`, sha256
  `6608dd029ee5627f0cfef07fa30f4ebc30bf15e7e9378cb14cda6cc73f98854f`.
- Reference sha256
  `f5fcf59abe744d9233d417295f60d65257b0958c59b9518c9fbf4b7b5531fb5a`
  (was `1b3c4c0cca82cf3a8a4ff571309c77d5080e7ac584590abfaaea6b028c868d56`);
  sidecar file sha256
  `931570be737209e27d3afe50aa14897048013cedb4f18735f7b24c414d1fd9e5`.
  The manifest itself is unchanged, so the sidecar keeps
  `manifest_sha256`
  `f505399cca50f214066d0183a2228bc30ec680bfc99f1c1370708dfd64a87308`
  with the new `reference_sha256`.
- All 108 rows are present; none was added or removed.

## Row-by-row diff against the old reference

Byte comparison finds nine differing rows. Six change behaviour (ranked
paths or metrics); three differ only below the top ten or in retrieval
depth, with identical top ten and metrics. All nine are consistent with the
span clip, which changes exact-lane chunk text and therefore exact-match
ranking at any depth. The aggregates move exactly as #1055 measured when it
landed (other MRR@10 0.168333 -> 0.185000, real-query MRR@10 0.449857 ->
0.451400, concept recall MRR@10 0.668269 -> 0.674679, identifier shape
MRR@10 0.486111 -> 0.490741; exact recall stays 1.0; no other mechanism or
shape moves).

Behaviour moves (old rank -> new rank, MRR@10 old -> new):

- `followup-census:940001` (mechanism `other`, shape `identifier`): rank 3
  -> 2, MRR 0.333333 -> 0.500000. `crates/aft/src/commands/add_import.rs`
  moves above `crates/aft/src/imports/mod.rs`. This is the fixture
  `land-symbol-range-span-clip.json` names.
- `followup-census:7972` (mechanism `wrong_lane_nl`): rank 1 -> 1, MRR 1.0
  -> 1.0. Top ten reorders from position 4 (`configure.rs`,
  `semantic_search/mod.rs`, `gh_shim.rs`, `main.rs`, ... becomes `main.rs`,
  `url_fetch_test.rs`, `configure.rs`, ...).
- `followup-census:17364` (mechanism `wrong_lane_nl`): rank 2 -> 2, MRR
  0.500000 -> 0.500000. Positions 3-5 reorder (`inspect/manager.rs`,
  `inspect_command_test.rs`, ... becomes `inspect_command_test.rs`,
  `diagnostics_category.rs`, `manager.rs`, ...).
- `followup-census:930001` (mechanism `wrong_lane_nl`): rank 1 -> 1, MRR
  1.0 -> 1.0. Same reorder as 7972 from position 4.
- `followup-census:930003` (mechanism `wrong_lane_nl`): rank 1 -> 1, MRR
  1.0 -> 1.0. Top ten reorders from position 2 (`configure.rs`, ... becomes
  `main.rs`, `url_fetch_test.rs`, `configure.rs`, ...).
- `followup-census:9372` (mechanism `not_a_search_failure`): miss -> miss,
  MRR 0.0 -> 0.0. Top ten reorders (`search_index.rs`, `migrate_storage.rs`,
  ... becomes `inspect/cache.rs`, `backup.rs`, `migrate_storage.rs`, ...).

Byte-only moves (top ten and metrics identical):

- `followup-census:3184` (mechanism `not_a_search_failure`): miss -> miss,
  MRR 0.0 -> 0.0. Only `retrieval_depth` changes, 304 -> 303. This is the
  row the #1039 merge-queue failure names: it is the first differing row in
  file order, so an `engine_unwired` check against the stale reference stops
  there even though the behaviour is unchanged.
- `followup-census:7656` (mechanism `topk_cut`): rank 5 -> 5, MRR 0.2 ->
  0.2. `retrieval_depth` 350 -> 349 and `page_zero_ranked_paths` reorders
  from position 19.
- `followup-census:9173` (mechanism `scope_mismatch`): miss -> miss, MRR
  0.0 -> 0.0. `page_zero_ranked_paths` reorders at position 45.

No other row differs. In particular, no row moves that #1055's evaluation
did not anticipate: its six behaviour rows are exactly the six above, and
the three byte-only rows keep their metrics and aggregates, so the gate's
`other` / `identifier` / concept / real-query deltas match #1055's
`72d69aec...` evaluation to six decimal places.
