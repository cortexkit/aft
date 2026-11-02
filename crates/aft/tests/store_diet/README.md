# Callgraph storage parity test tool

## Frozen baseline and scope

The runtime baseline is **998c896b05753dd3a83158e30f97779b89ef28e2**.
`baseline.json` records the source hash, corpus revisions, seed, recording
command and empty normalisation whitelist. Runtime storage code is unchanged;
the only source registration is a cfg(test) module in callgraph_store.
No existing test assertion about a tool's returned output was edited.

There are two complementary runners:

1. `harness.rs` builds real legacy and views-derived fixture databases, captures
   six adapter replies and seeded node/pair replies as bytes, compares table
   counts (including unresolved refs), and snapshots query plans. It reads the
   existing `tests/fixtures/callgraph` tree recursively; it does not rewrite those
   fixtures. The views path extracts the same blobs and materializes a database
   through the production materializer, then uses the same CallGraphStore reader
   with manifest_view set. It does not simulate borrowing/publication.
2. `parity.py` drives an actual standalone AFT binary over newline-delimited JSON
   (NDJSON). It sends one
   request at a time, keeps push frames separate, waits for its matching reply,
   uses isolated empty caches, and compares complete reply lines as bytes. Its
   seeded manifest includes callers/call_tree/impact (up to 500 symbols), three
   trace operations (up to 100 pairs), zoom/search enrichment and all three
   graph-backed inspect categories. A checkout configured to borrow a published
   callgraph from another checkout can be used as the root with `--views` after
   its owner's base is built and published; borrowing provenance is not normalised.

The fixture query manifests are generated from the committed seeded fixture
symbols, not from a candidate's possibly changed node set. The seed and sample
algorithm are in the tool. `harness.rs` uses BLAKE3 ordering for its adapter
sample; the standalone manifest uses SHA-256 ordering, independently committed
before a candidate capture. Each manifest supplies a stable request id.

## Reproduce adapter goldens

Copy `tests/store_diet` and the cfg(test) module registration onto the baseline
revision (test scaffolding only), then run:

```
AFT_STORE_DIET_RECORD=1 cargo test -p agent-file-tools --lib store_diet -- --nocapture
cargo test -p agent-file-tools --lib store_diet
```

The first command explicitly records, the second compares. Ordinary test runs
never update a golden. `legacy-*.json` and `views-*.json` are separate baseline
oracles: legacy/views need not agree with each other, but a candidate must agree
with its own mode's baseline. The main six-operation replies use actual resolved
symbols and a real data-flow expression. The seeded cases additionally preserve
empty results and errors for node names not accepted as operation origins.

## Standalone baseline/candidate replay

Build the baseline executable from the recorded revision, optionally with the
trace-only patch described in `trace.md`. Build the candidate separately with
the same toolchain/profile. Do not index the patched baseline's own source as a
reference corpus: index the same frozen source tree with both executables.
For each frozen reference tree, export the baseline node sample as a JSON array
of `relative-path + NUL + scoped-name` strings, then generate the manifest once:

```
python3 crates/aft/tests/store_diet/parity.py manifest symbols.json queries.json
python3 crates/aft/tests/store_diet/parity.py capture BASELINE TREE EMPTY_BASE_CACHE queries.json base-replies
python3 crates/aft/tests/store_diet/parity.py capture BASELINE TREE EMPTY_REPLAY_CACHE queries.json replay-replies
python3 crates/aft/tests/store_diet/parity.py compare base-replies replay-replies
python3 crates/aft/tests/store_diet/parity.py capture CANDIDATE TREE EMPTY_CANDIDATE_CACHE queries.json candidate-replies
python3 crates/aft/tests/store_diet/parity.py compare base-replies candidate-replies
```

Repeat with `--views` on each capture, and with a prepared borrower root. Cache
paths must not exist before capture. The tool refuses an empty comparison or a
manifest-membership mismatch. Configure replies are admission/setup, not query
results; they are checked for success but excluded from the measured reply set.
All query replies, including expected errors, remain byte-for-byte oracles.
No fields are currently normalised. In particular, an inspect response still
reporting analysis in progress or a search timing difference will fail replay:
settle the baseline workload and establish any permitted metadata whitelist
with baseline-versus-baseline evidence before using a candidate.

The runner waits for inspect to report `fresh` before recording its query reply,
and retries while the runtime is still assigning the project's stable repository
identity or building its graph. If the only
remaining differences are top-level `search_ms` or `tier2_last_run` metadata,
`prove-normalisation BASE_CAPTURE REPLAY_CAPTURE PROOF.json` checks that both
captures used the same baseline executable, proves all other bytes equal, and
writes the observed whitelist. Commit that proof and whitelist before recording
a candidate. Pass `--normalisation PROOF.json` to subsequent comparisons. The
normaliser replaces only the selected top-level numeric value spans: nested row
fields, whitespace, order, counts, notes and provenance remain byte-identical.
No other field is admitted by the tool.

For the committed fixture sample:

```
python3 crates/aft/tests/store_diet/parity.py fixture-manifest crates/aft/tests/store_diet/legacy-seeded.json crates/aft/tests/store_diet/legacy-query-manifest.json
python3 crates/aft/tests/store_diet/parity.py fixture-manifest crates/aft/tests/store_diet/views-seeded.json crates/aft/tests/store_diet/views-query-manifest.json
```

After the producer has exited, table counts can be captured from its finished
SQLite database (not from a second raw descriptor on a live database):

```
python3 crates/aft/tests/store_diet/parity.py counts FINISHED.sqlite counts.json
```

Compare those JSON bytes as well as replies. Missing tables are explicit nulls,
not silently converted to zero. A future unique-pair encoding of
view_unknown_live needs a reader-proven distinct-pair/site-count comparator
before changing its current row-count oracle.

## Inventory and plans

- `statements.json`: static SQL literal census across crates/aft/src, including
  test-only and incomplete-format fragments. Stable ids hash exact SQL bytes;
  source citations are not identities. Baseline/candidate text mappings are
  explicit. Supported batch templates include size 1 and maximum 499 pairs or
  999 node ids. Stable-row writes use their actual caller column arrays.
- `order_sql.json`, `source_inventory.json`, `order.md`: ordered and unordered
  SQL, Rust sort/set/dedup sites, replacement-key rules and traversal-state
  classification. The actual baseline call_tree is per-path visited.
- `table_encoding.json`, `identities.md`: per-column encoding and complete
  identity-input obligations, including fields not retained in finished stores.
- `sweep_references.json`, `sweep.md`: equality probes, current supporting
  indexes and explicitly proposed indexes for every interned reference.
- `trace_helpers.json`, `trace.md`, `baseline-trace.patch`: open seams, trace
  patch and coverage procedure.
- `baseline-plans.json`: actual bundled-SQLite plans for the fixture's ready
  legacy/views schemas, with a separate in-memory schema for other databases
  and transaction-local tables. That auxiliary schema copies the graph schema
  and adds static CREATE/ADD COLUMN declarations from the census; graph queries
  always try the real fixture first. Unavailable schema/format/test-only variants are
  explicitly recorded as `unprepared`, never mislabeled as SEARCH. This is a
  visible coverage gap, not a plan guarantee for that statement. The exact
  snapshot test detects any change to a prepared plan or its availability.
  `inventory.py compare-plans` permits stronger plans but rejects additional
  scans, lost equality constraints and non-primary-key new joins. Identical
  explicitly unavailable baseline entries remain unavailable, not counted as
  prepared plans. Newly added statements cannot use that exception.

Regenerate only after reviewing a source/inventory change:

```
python3 crates/aft/tests/store_diet/inventory.py capture
python3 crates/aft/tests/store_diet/inventory.py check
python3 crates/aft/tests/store_diet/inventory_test.py
python3 crates/aft/tests/store_diet/inventory.py compare-plans BASELINE_PLANS.json CANDIDATE_PLANS.json
```

The Python tools use only the standard library. Plan-guard tests deliberately
exercise red cases (SEARCH to SCAN, fewer equality columns, new unindexed join,
unavailable plan), and byte-comparison tests change row order to prove it is
not ignored. The source census is a conservative static aid, not a Rust parser:
new dynamic SQL constructors still require an explicit reviewed expansion.
