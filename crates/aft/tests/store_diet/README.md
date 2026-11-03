# Callgraph storage parity test tool

## Frozen baseline and scope

The runtime baseline is the named main commit in `baseline.json`.
That file records the source hash, corpus revisions, seed, recording
command and empty normalisation whitelist. Runtime storage code is unchanged;
source additions are the cfg(test) module and consumer-probe shims reproduced
by `baseline-harness.patch`.
No existing test assertion about a tool's returned output was edited.

## Capture storage

Generated JSON larger than 64 KiB (65,536 bytes) is stored as `*.json.gz`.
`capture_io.py` writes gzip at level 9 with a fixed zero modification time and
no filename in its header. Rust recording uses that same Python writer; ordinary
Rust checks decode gzip using dependencies already declared by the crate.
Re-recording unchanged bytes therefore does not add clock or path differences.

Tools accept either the physical `.json.gz` path or its logical `.json` name;
the logical name resolves to the compressed file when present. Having both forms
is an error, not a choice of which oracle to trust. Writers automatically select
the storage form and remove the obsolete alternative. Small reviewed metadata,
counts, required values, recording hashes, policy files and corpus sources stay
plain, as do both patches. `recording.json` hashes the physical stored files.

Compression changes only the container: Rust golden checks still compare the
complete decompressed bytes, including whitespace and order. Python checks load
the same decompressed content before their existing comparisons. Raw wire reply
lines remain unchanged. The tests cover fixed headers, byte-identical rewrites,
changed payloads and concatenated gzip members.

There are two complementary runners:

1. `harness.rs` builds real legacy and views-derived fixture databases, captures
   six adapter replies and seeded node/pair replies as bytes, compares table
   counts (including unresolved refs), and snapshots query plans. It reads the
   existing `tests/fixtures/callgraph` tree recursively; it does not rewrite those
   fixtures. The views path extracts the same blobs and materializes a database
   through the production materializer, then uses the same CallGraphStore reader
   with manifest_view set. It does not simulate borrowing/publication.
2. `parity.py` drives an actual standalone AFT binary over newline-delimited JSON
   (NDJSON), using canonical `tool_call` arguments rather than backend-only
   argument spellings. The replies include the real agent-facing formatted text
   as well as structured results. It sends one
   request at a time, keeps push frames separate, waits for its matching reply,
   uses isolated empty caches, and compares complete reply lines as bytes. Its
   seeded manifest includes callers/call_tree/impact (up to 500 symbols), three
   trace operations (up to 100 pairs), zoom/search enrichment and all three
   graph-backed inspect categories. Borrowed queries use `--views` with an
   explicitly prepared owner/base cache as described below; borrowing provenance
   is not normalised.

The fixture query manifests are generated from the committed seeded fixture
symbols, not from a candidate's possibly changed node set. The seed and sample
algorithm are in the tool. `harness.rs` uses BLAKE3 ordering for its adapter
sample; the standalone manifest uses SHA-256 ordering, independently committed
before a candidate capture. Each manifest supplies a stable request id.

## Reproduce adapter goldens

Record or re-record every baseline artifact with one command:

```
python3 crates/aft/tests/store_diet/record.py --baseline MAIN_SHA
```

The recorder checks out that exact commit in a scratch clone, installs only the
test-scaffolding patch, generates the schema/corpus inventories, records and
self-replays all goldens, then applies the trace-only patch and verifies replies
again. It publishes only after the whole run succeeds. `recording.json` names the
commit and output hashes. Review regenerated trace exemptions before committing.
Use `--output-dir target/scratch/oracles` on a build server; the recorder also
writes a SHA-keyed tarball for returning the complete artifact set.
Wire caches use a shorter, marked sibling directory under `target/scratch`:
SQLite's Unix virtual file system limits complete filenames to 512 bytes, and derived filenames
already consume about half of that budget. Cache contents never leave the
checkout, and only directories marked with the selected full SHA are reset.

The committed recording uses main at `a2f2cddb98e5f0748b6bb95e41a91500b1c01062`,
including selective re-resolution's stored target identity/provenance tie-breaks
on four edge queries. The earlier `998c896` baseline omitted those keys, allowing
refresh and cold builds to return tied edges in different orders. Its recordings
were provisional because reply order could drift before any storage encoding
changed. Future
resolver changes with an intentional answer/version change require another
named-main recording before testing the encoding candidate.

After recording, ordinary candidate checks are read-only:

```
cargo test -p agent-file-tools --lib -- store_diet
```

The recorder alone enables recording and validates its checkout SHA. Ordinary test runs
never update a golden. The `legacy-*` and `views-*` captures are separate baseline
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

Repeat with `--views` on each capture. Ordinary cold-capture cache paths must not
exist before capture. For borrowing, first build/publish an owner's graph with
the corresponding executable into each mode's separate cache. Capture its linked
borrower root with `--prepared-cache --require-borrowed --views --config POLICY.json`.
The policy file supplies that workload's existing copy/overlay settings; it is
merged into the default user config without changing `--views`. Prepared mode
retains the owner state, and require-borrowed fails unless an actual reply has
`borrowed_callgraph` provenance. A fresh empty cache cannot test borrowing.
Comparison also checks the mode, policy hash and prepared/borrowed flags.
The tool refuses an empty comparison or a
manifest-membership mismatch. Configure replies are admission/setup, not query
results; they are checked for success but excluded from the measured reply set.
All query replies, including expected errors, remain byte-for-byte oracles.
No fields are currently normalised. In particular, an inspect response still
reporting analysis in progress or a search timing difference will fail replay:
settle the baseline workload and establish any permitted metadata whitelist
with baseline-versus-baseline evidence before using a candidate.

The runner retries inspect's transient `inspect_not_fresh` error but preserves
finalized PARTIAL replies and their incomplete-analysis notes. It also retries
while the runtime is still assigning the project's stable repository identity
or building its graph. If the only
remaining differences are top-level `search_ms` or `tier2_last_run` metadata,
`prove-normalisation BASE_CAPTURE REPLAY_CAPTURE PROOF.json` checks that both
captures used the same baseline executable, proves all other bytes equal, and
writes the observed whitelist. Commit that proof and whitelist before recording
a candidate. Pass `--normalisation PROOF.json` to subsequent comparisons. The
normaliser replaces only the selected top-level numeric value spans: nested row
fields, whitespace, order, counts, notes and provenance remain byte-identical.
No other field is admitted by the tool.

Git must preserve the frozen input and oracle bytes as well. `.gitattributes`
disables checkout text conversion for the corpus, original callgraph fixtures,
raw copied sample inputs, captures, patches and wire replies. The recorder also
sets `core.autocrlf=false` and `core.eol=lf` in its scratch clone, because an older
baseline predates that attribute policy. Replies are not line-ending or path
normalised to compensate for a checkout difference.

For the committed fixture sample:

```
python3 crates/aft/tests/store_diet/parity.py fixture-manifest crates/aft/tests/store_diet/legacy-seeded.json.gz crates/aft/tests/store_diet/legacy-query-manifest.json.gz
python3 crates/aft/tests/store_diet/parity.py fixture-manifest crates/aft/tests/store_diet/views-seeded.json.gz crates/aft/tests/store_diet/views-query-manifest.json.gz
```

After the producer has exited, table counts can be captured from its finished
SQLite database (not from a second raw descriptor on a live database):

```
python3 crates/aft/tests/store_diet/parity.py counts FINISHED.sqlite counts.json
```

Compare those JSON bytes as well as replies. Missing tables are explicit nulls,
not silently converted to zero. Format/salt metadata and registry session/usage
rows are not graph facts and are excluded from the graph-row count oracle.
`view_unknown_live` records unresolved sites and the file/symbol targets those
sites may call. Collapsing it into unique target pairs first requires proving
that every reader needs only those pairs and that site-count notes still use
the summed original site counts. Its comparator must then check both distinct
pairs and summed site counts before replacing the current row-count oracle.

## Inventory and plans

- `statements.json.gz`: static SQL literal census across crates/aft/src, including
  test-only and incomplete-format fragments. Stable ids hash exact SQL bytes;
  source citations are not identities. Lexical owner/test-scope annotations aid
  review but do not replace Rust's compiler. Baseline/candidate text mappings are
  explicit. Supported batch templates include size 1 and maximum 499 pairs or
  999 node ids. Stable-row writes use their actual caller column arrays.
- `order_sql.json.gz`, `source_inventory.json.gz`, `order.md`: ordered and unordered
  SQL, Rust sort/set/dedup sites, replacement-key rules and traversal-state
  classification. The actual baseline call_tree is per-path visited.
- `table_encoding.json.gz`, `identities.md`: per-column encoding and complete
  identity-input obligations, including fields not retained in finished stores.
- `sweep_references.json`, `sweep.md`: equality probes, current supporting
  indexes and explicitly proposed indexes for every interned reference.
- `trace_helpers.json`, `trace.md`, `baseline-trace.patch`: open seams, trace
  patch and coverage procedure.
- `baseline-plans.json.gz`: actual bundled-SQLite plans for the fixture's ready
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

`capture` records a baseline, not an encoding change's replacement oracle. For
subsequent changes, retain each row's `id` and `baseline`, edit its `candidate`
text explicitly, then use `derive` to update the dependent inventories. A new
statement has `baseline: null` and an id of `new-sql-` plus the first 20 hex
characters of SHA-256(candidate text). `check` matches current source text to
candidate text without changing the baseline identity.

The Python tools use only the standard library. Plan-guard tests deliberately
exercise red cases (SEARCH to SCAN, fewer equality columns, new unindexed join,
unavailable plan), and byte-comparison tests change row order to prove it is
not ignored. The source census is a conservative static aid, not a Rust parser:
new dynamic SQL constructors still require an explicit reviewed expansion.

`CreatedFilePatterns::query` has no application maximum: package exports and
tsconfig aliases can add arbitrarily many distinct JS patterns. Its executable
variants cover empty, single and multiple OR terms in both language predicates,
with and without the Rust crate-directory predicate. These are shape samples,
not an invented maximum. Fixed-size frontier/node batches still include their
actual maximum (499 pairs or 999 ids). SQLite's expression/parameter limits are
not an application batch-size contract.

The small six-operation wire golden uses `--config wire-policy.json` (inspection
background work disabled). This is an input policy, not reply normalization:
otherwise asynchronous health counts may first become available between two
calls and append a new `[AFT ...]` status note only to the slower replay. The
trace replay exposed that baseline timing dependency. No status note is stripped
or masked. The full inspect/search/zoom manifest must use inspection enabled;
its baseline-versus-baseline replay remains the authority for any additional
settling policy before a candidate is tested.
