# Inspect-truth: offline callgraph corpus rerun

Run the committed wrapper on a Linux host using the existing, unchanged
`run.py` collection and scoring functions. It makes **fresh collections of both
binaries** with separate Agent Foundational Toolkit (AFT) index/inspection caches, then
compares both against the same frozen
per-corpus-pin oracle records. It never fetches repositories, installs packages,
compiles Rust/Go, or downloads oracle dependencies.

```sh
BASE_BIN=/srv/binaries/aft-13dd9cb71 \
CAND_BIN=/srv/binaries/aft-candidate \
CORPUS_DIR=/srv/callgraph-corpus \
OUT_DIR=/srv/reports/callgraph-new-run \
bash benchmarks/inspect-truth/rerun-callgraph-corpus.sh
```

Required inputs are `BASE_BIN`, `CAND_BIN`, and `CORPUS_DIR`. Supply Linux
**release** binaries. `BASE_BIN` must be the inspect-truth accuracy baseline's
0.58.2 build from
`13dd9cb71a7bbf9076377ac1ba1717dabf15e586`; the wrapper checks its version but
cannot prove its source revision or optimization profile. `CAND_REV` is optional:
set it only when it identifies the supplied binary. Otherwise the candidate is
identified by its SHA-256, not by the script checkout's HEAD. Both binary hashes
and version output are recorded. Both binaries and every frozen input record are
hashed again after collection; a changed executable or oracle/judgment capture
invalidates the run instead of silently mixing generations.

`OUT_DIR` is optional and must not already exist. The default is a new unique
directory under `$CORPUS_DIR/_reruns/`. It may not be inside a source repository
or the imported `_results/` tree. Existing captures and judgments are never
replaced. `PYTHON` optionally selects the interpreter. `--help` prints usage.

## Seven-repository diagnostics and footer comparison

This separate pass needs only the **candidate release binary**, not the historical
liveness baseline. It reuses `run.py`'s seeded sampling (60 paths per configured
language domain),
NDJSON sessions, reference invocations, parsers and diagnostic matcher. It does
not change liveness scoring or judgments and leaves the four invalid TypeORM
codemod-fixture exclusions listed in `corpus.json` untouched.

```sh
bun run --cwd packages/aft-bridge build
python3 benchmarks/inspect-truth/diagnostics.py \
  --aft-bin target/release/aft \
  --corpus-root target/inspect-follow-up/corpus \
  --output-dir local-ignore/inspect-follow-up/diagnostics \
  --sample 60
```

Omitting `--repo` selects all seven entries in `corpus.json`. Fetch/install the
pinned corpus first with `python3 benchmarks/inspect-truth/run.py fetch
--corpus-root target/inspect-follow-up/corpus`. Reference tools and language
servers must be available where `run.py` expects them: Fallow, Pyright and Go
on PATH; `knip`/fallback `tsc` in `<corpus-root>/.tools/node/node_modules/.bin/`;
Vulture in `<corpus-root>/.tools/py/bin/`; and staticcheck/deadcode in
`<corpus-root>/.tools/go-bin/`. TypeScript normally comes from each project's
installed dependencies. Rust needs Cargo and a working rust-analyzer toolchain. The
reference invocations also collect their usual liveness outputs, but this runner
compares **diagnostics only**. Cargo checks are serialized and finish before AFT
starts rust-analyzer. For AFT plugin references, the collector runs from each
configured Fallow working directory and converts reported paths back to
repository-relative paths. The runner may install corpus dependencies with
`run.py`'s configured install commands; provide external reference tools and
language-server executables separately.

Output must be a new directory. Each repository retains the sampled paths, raw
NDJSON replies, reference output/exit metadata, observed agreements/disagreements,
which diagnostic tools were missing/incomplete and which reference working
directories failed, and status snapshots before and after an unscoped diagnostic
read. The footer goes through the production OpenCode `coerceAftStatus` parser
and `formatAftStatusSegment` formatter via Bun, not a Python reconstruction.
`results.json` combines all seven records. Exit 1 means at least one diagnostic
comparison or footer capture is incomplete; unavailable/timed-out producers
remain named and an observed empty list is **not** reported as an authoritative
zero. Source review of disagreement examples belongs in the campaign report.

On the remote read-only bind mount only, add `--same-mode-config-restore`. This
skips chmod exclusively for this checkout's `.cortexkit/aft.jsonc` when its full
mode already equals the saved mode; all other paths/mode changes still call
chmod. The skipped calls are recorded. The original config bytes are written
back, and `run.py` verifies that Git status is unchanged.

Regression checks: `python3 benchmarks/inspect-truth/test_diagnostics.py -v`
(Python 3.9+, Bun, installed workspace dependencies).

## Exactly what to copy

Copy these paths from the delivered worktree into `CORPUS_DIR`, preserving the
layout **below** `local-ignore/callgraph/corpus/`:

### Pinned source snapshots (include their `.git` directories)

- `local-ignore/callgraph/corpus/ripgrep/`
  - pin: `3fce3b5bb0236da2df6d99672afb8a719642eca7`
- `local-ignore/callgraph/corpus/axum/`
  - pin: `f8b02f22cf10bee707bda19b58265b9e33677535`
- `local-ignore/callgraph/corpus/soft-serve/`
  - pin: `37685d36f5b7bf0e32217ddd7c8e045c57772619`

The wrapper checks each Git HEAD and refuses tracked source changes. No checkout
or fetch is performed. A standalone `.git` directory must be copied, not a
worktree `.git` pointer to a directory that will disappear on the source host.
The collector's temporary `.cortexkit/aft.jsonc` is restored after each binary,
including on failure.

### Frozen baseline and judgment origins

For **each** directory below, copy at least `aft.json` and `oracle.json`:

- `local-ignore/callgraph/corpus/_results/ripgrep/13dd9cb71/`
- `local-ignore/callgraph/corpus/_results/ripgrep/7306b6953ace4ad20e8b3108486e6b0d831d8af5/`
- `local-ignore/callgraph/corpus/_results/axum/13dd9cb71/`
- `local-ignore/callgraph/corpus/_results/axum/7306b6953ace4ad20e8b3108486e6b0d831d8af5/`
- `local-ignore/callgraph/corpus/_results/soft-serve/13dd9cb71/`
- `local-ignore/callgraph/corpus/_results/soft-serve/715f01b05062908be3f9cd147f0e4399c40274ec/`
- `local-ignore/callgraph/corpus/_results/soft-serve/a37cdbe83ec281fd7a13fe76b83d5b1eae96deae/`

The `13dd9cb71` directories contain the original baseline captures and the frozen
oracle denominators. `7306b6953...` contains the Rust scanner revision's judgment
origins; `715f01b05...` contains the Go scanner revision's origins;
`a37cdbe83...` contains the later callgraph revision's 32 annotated Go rows.
These identifiers are AFT source revisions, not new corpus source pins.

Whole directories may be copied, but their old `aft-storage-*`, build outputs,
and oracle tool caches are not needed. No other `local-ignore/` path is required:
the baseline source archive, previous score directories, `.tools/`, and the
213581cc8/9e8c83db captures are optional historical evidence, not runner inputs.

The seven records are required because existing judgments validate against their
**own original baseline/after captures**, even when a newer candidate has removed
those findings. The wrapper relocates only selected `after_outputs` paths in
runtime copies of the committed judgment descriptors. It preserves every entry,
revision identity and scoring rule; no `target/` symlink or source edit is needed.
The original 13dd9cb71 capture remains the authority for judgment validation and
baseline truncated-scope exclusions; the supplied baseline binary is collected
fresh and supplies the **before** rows displayed in the new table.

### Committed harness files

Use this repository checkout at the delivered runner revision. For a smaller
offline copy, preserve this relative layout:

- `benchmarks/inspect-truth/rerun-callgraph-corpus.sh`
- `benchmarks/inspect-truth/run.py`
- `benchmarks/inspect-truth/corpus.json`
- `benchmarks/inspect-truth/judgments/*.json` (all descriptors)
- `benchmarks/inspect-field-audit/run_audit.py` (the existing NDJSON client)

Host dependencies: Bash, Python 3.9+, Git, and ripgrep (`rg`) on PATH. Go oracle
executables are **not needed** when the copied frozen `oracle.json` records are
present. The oracle findings were collected once for each source revision pinned
above and are deliberately reused, so
both fresh binary collections use identical oracle denominators. No Cargo
installation, Go module cache, network access, or prebuilt local AFT cache is
required by this runner. Diagnostic sampling is disabled, as in the reported
callgraph corpus run.

## Outputs and exit status

The new output directory contains:

- `raw/<repo>/before/` and `raw/<repo>/after/`: fresh `aft.json`, the identical
  frozen `oracle.json`, separate AFT storage, and scorer disagreement lists;
- `results.json` and `results.md`: the unchanged scorer's per-project/language/
  category table (dead-code counts, precision, recall, judged/unjudged counts,
  before/after sides, and the cell rule);
- `cell-diff.json` and `cell-diff.md`: one before/after row per cell, including
  counts, precision, recall and annotation counts. Numeric delta is **after minus
  before**. `lowered` is delegated to the existing `cell_changes()` rule, not a
  new threshold or scoring interpretation;
- `judgments/`: relocated runtime descriptors; committed originals are unchanged;
- `run.json`: binary hashes/versions, corpus pins, frozen-input hashes, candidate
  identity, incomplete buckets and the existing cell-rule failures.

Exit **0** means all compared buckets were scoreable and no existing metric cell
lowered. Exit **1** means the existing cell rule failed; reports are still written.
Exit **2** means invalid/missing inputs, failed collection, or an unknown bucket;
an unknown count is never claimed as zero or a successful accuracy run.
Passing this cell rule does not itself satisfy numeric targets such as Go
precision >= 0.6: inspect the table and retain row judgments for missed targets.

For runner regression checks (no real corpus collection):

```sh
python3 benchmarks/inspect-truth/test_score.py -v
bash -n benchmarks/inspect-truth/rerun-callgraph-corpus.sh
```
