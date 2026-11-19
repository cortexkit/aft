# Call-graph exactness gates

This is an offline, fail-closed gate for a call-graph builder change, not a
performance benchmark. It imports the index matrix's manifest and **the same
`checkout()` implementation**, so pins and Git checkout validation cannot drift.
Do not refresh the matrix pins to make an exactness failure disappear.

## Build the reference and candidate

Run from the repository root on the build server. Use one
`linux,16c,32gb,net` job at a time (build-server jobs are capped at 32 GB; a
project whose comparison needs more runs on a rented machine). Commit the
candidate before submitting the job.
`build.py` archives the named source revision and overlays only the test probe,
its feature-gated module declarations, the feature-gated negative-control hook,
and the dead-code comparison entry point. It does **not** copy today's resolver
over the reference. Each source archive has a separate Cargo target directory.

```sh
git log -1
python3 scripts/telemetry/callgraph-exactness/build.py \
  cb3a5ae1badbb30716ad1de187b903ab47e90638 \
  target/measurements/exactness/reference-build
python3 scripts/telemetry/callgraph-exactness/build.py \
  HEAD target/measurements/exactness/candidate-build
python3 scripts/telemetry/callgraph-exactness/run.py \
  --reference target/measurements/exactness/reference-build/probe \
  --candidate target/measurements/exactness/candidate-build/probe \
  --reference-source cb3a5ae1badbb30716ad1de187b903ab47e90638 \
  --candidate-source "$(git rev-parse HEAD)" \
  --repository neovim/neovim \
  --repository apache/superset \
  --repository kubernetes/kubernetes \
  --results target/measurements/exactness/pins \
  --threads 1 16 --build-cache-bytes 4096 --spill-bytes 4096 \
  --negative-control
```

Use a fresh output directory for every run. The driver refuses to overwrite
results. A process lock prevents overlapping runs of the kit in one checkout.
`--repository` can select a single matrix pin. `--sample` changes the number of
symbols per stratum (default three). Probe deadlines are per process, not per
project; `--timeout-s` defaults to four hours. The builder archives must still
have the helper's integration anchors; a later refactor that removes an anchor
must explicitly integrate the hook rather than silently skipping it.

The default build profile is `ci-storm`, an optimized test build. These binaries
enable `test-hooks` and must never be shipped as production binaries.

For a reference-against-itself proof, pass the reference probe as both binaries
and the legacy SHA as both source identities. The negative-control invocation
sets `AFT_EXACTNESS_FLIP_SITE=exactness_gate/service.ts` for **only** the broken
process. Before storage, it changes one real method site's target symbol. A pass
requires a failing comparison naming that file and a log confirming the flip.

## What is compared

* **Published fragment column:** every path, content-record key and optional
  fragment digest, including nonparsed paths. A fragment digest covers the
  fragment's bytes and fact subjects, not merely its target list.
* **Raw cold sections:** content and graph sections are compared byte for byte
  by `(section kind, key space)`, including their versions and item counts.
  This includes both slice sections and **all** term lookup/posting key spaces,
  not just the terms used by the sampled queries. Segment headers, timestamps,
  physical offsets and the physical order of sections are not compared.
* **Refresh sections:** a delta publication and a cold publication cannot have
  identical segment-local ordinals or obsolete units. Both are copy-only folded
  first. The fold copies active stored units and their existing postings; it
  does not resolve source again. The folded sections are then compared byte for
  byte, again by section ID. Cold/cold comparisons also retain the pre-fold raw
  comparison so folding cannot hide a cold writer difference.
* **Full `FileResolution`:** the Rust probe decodes the reference artifact and
  compares it with every candidate stored resolution using derived `Eq`, not
  selected fields. Rows, ordered sites, targets, protected sets, provenance,
  dispatch, recorded binding reads and `site_dependencies` are covered. The
  reference artifact retains fragment encodings as well as path metadata.
* **Queries:** the actual serializable result adapters for callers, impact,
  trace-to, call-tree, trace-to-symbol and trace-data. JSON object keys are
  canonical; array order is preserved. Both test inclusion settings run. The
  reference chooses busiest symbols by incoming edge count, middle-ranked
  symbols and leaves, with deterministic path/name ties. Its query plan is
  reused verbatim by the candidate. Trace pairs derived from traversable calls
  and a known inter-file witness must have paths when tests are included, and
  also when tests are hidden if neither endpoint is a test file. Both modes
  still compare the reference and candidate answers byte for byte.
  A data-flow witness must have nonempty hops. Full dead-code input rows and
  actual uncached dead-code finding JSON are compared too; timestamps and phase
  measurements are outside the compared JSON.
* **Read health:** `StoredSlices::failures()` must be empty after all stored
  resolutions are read, after queries, and before refreshed changes publish.
  The production publication path also rejects a failed stored read during
  section writing. Empty graphs do not pass.

## Edits and restart semantics

Every project gets a deterministic mixed-language witness island. Round one
adds, deletes and renames files; changes a method, a receiver type, a base class,
a non-called required Go interface member, an import and `tsconfig.json`.
Round two adds the missing Go implementation and changes the inherited receiver
again, on top of round one.

Native edits are derived from the reference's stored graph, not fixed source
locations: seeded file deletion/rename, a declaration of a busy member name in
a distinct new owner, removal of an existing declaration in the next round,
and a parameter-name change on the busiest callee (of the forty busiest) whose
signature has a parameter besides its receiver. Edits are
syntax-validated; rejected declarations and the selected locations are recorded.
The island and native edits are restored in `finally`, including failure paths.
`refresh_fanout` records changed fragments belonging to files not directly edited.

Each refresh is a new executable invocation over the preceding **published
store**. The probe refuses a missing publication rather than substituting a cold
build. It records the edited paths, refreshes, checks read health and publishes.
For each round and thread count, it compares against both a reference cold build
and an independent candidate cold build of the same edited tree. The second
round reopens the first refresh's publication, not the original store.

## Knobs and evidence

`--threads 1 16` controls actual builder threads. `--cache-bytes` controls the
DataLayer block cache. `--build-cache-bytes` and `--spill-bytes` are passed as
`AFT_CALLGRAPH_BUILD_CACHE_BYTES` and `AFT_CALLGRAPH_SPILL_BYTES`: reserved
determinism inputs which the legacy builder currently ignores. Implementations
of those budgets must wire these inputs before claiming a tiny-budget gate.
Decoded-slice cache misses/evictions are measured, not inferred from project size.

Each project directory retains `result.json`, native edit recipes, changed-path
lists, all store/snapshot artifacts, the reference query plans, probe logs,
per-path fragment metadata, raw/folded sections, resolution encodings, canonical
query JSONL, dead-code JSON and health/cache observations. `result.json` includes
per-probe wall time, binary hashes, source identities, query counts, positive-path
witness counts, read health and actual cache observations. Success ends with
`EXACTNESS GATES PASSED`; exceptions, missing artifacts, empty graphs and failed
positive witnesses are failures, never skipped checks.

Focused checks:

```sh
python3 -m unittest discover -s scripts/telemetry/callgraph-exactness -p 'test_*.py' -v
cargo check -p agent-file-tools --example callgraph_exactness --features test-hooks
cargo test -p agent-file-tools --lib -- data_layer::incremental_tests
cargo build --profile ci-storm -p agent-file-tools --example callgraph_exactness --features test-hooks
python3 scripts/telemetry/callgraph-exactness/smoke.py \
  --binary "$CARGO_TARGET_DIR/ci-storm/examples/callgraph_exactness" \
  --output target/measurements/exactness-smoke
```
