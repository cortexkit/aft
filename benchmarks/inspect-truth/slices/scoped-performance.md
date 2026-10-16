# Scoped inspect: exact rollup reuse and lazy runtime notes

## Change and boundaries

Baseline: `3044304035317f5fbede501e87c2ef8fd26504ec` (`origin/main` when
work began). A preceding TypeORM elapsed-time profile identified whole-project
scope reconstruction and runtime-note producer discovery as the dominant costs.
The reproduction below uses only this baseline and the pinned TypeORM source.

For a freshness-verified legacy scoped cache read, the `dead_code` analysis
category reuses the existing
callgraph projection and its existing uncapped rollup. Reuse requires:

- the same generation, durable graph revision, project root, and snapshot;
- the same actual contribution JSON hashes, not just source hashes;
- the same configuration/manifest/resolver contribution-set key;
- the same public API files, project roles, and uncapped presentation state.

A miss rebuilds the global rollup and retains it in the existing projection
slot. A missing graph still reports unavailable. Stale and last-complete reads
retain cold reconstruction. No persistent format, contribution-cache version,
or new cache is introduced. The snapshot-pointer check also prevents a result
computed from one projection being installed on a concurrently replaced one.

Other categories still reconstruct their scoped summaries: those summaries are
in the response even when their drill-down sections were not requested. Their
redundant project inventory walks are removed because their language-coverage
fields are removed by the existing scoped filter. Contributions are **not**
filtered before global import, reachability, cycle, or duplicate analysis:
outside callers and cross-boundary groups must retain their meaning.

Runtime-note selection first checks the raw client-note roster under a short
LSP-manager guard. If no client has a note, it skips producer discovery. If notes
exist, it releases that guard, resolves scoped producer keys without the LSP
lock, and reacquires the guard only to select notes. A server starting between
the two observations may contribute its note to the later selection; an empty
answer may instead reflect the first snapshot. Both retain honest observations
without holding the LSP manager during a project/tsconfig walk. Sorting,
deduplication, structured notes, text rendering, diagnostics authority, and
producer-failure reporting are unchanged.

This is a warm-path improvement, **not a claim that every category or cold
request is now strictly proportional to scope size**. Freshness verification,
contribution decoding/hashing, and the other four categories still have global
work. New global liveness analysis remains necessary after invalidation.

## Response parity

`scoped_parity.py` compares complete response bodies after removing **only**
the request `id` and `scanner_state.tier2_last_run`. Counts, findings, object and list ordering,
envelopes, gaps, completion, notes, and rendered text remain in the comparison.
All eight scope/section cases below produced identical before/after response
bodies, as did every timing pair for the `packages`/`dead_code` request:

| Case | Scope | Sections | topK |
|---|---|---|---:|
| 0 | `packages` | dead_code | 100 |
| 1 | `packages/typeorm/src/index.ts` | dead_code | 100 |
| 2 | `.` | dead_code | 100 |
| 3 | `packages/typeorm/src` | dead_code, unused_exports | 7 |
| 4 | `packages/legacy-naming-strategies/src` | all | 100 |
| 5 | `packages/typeorm/src/index.ts` | omitted | 100 |
| 6 | `packages/typeorm/src/index.ts` | diagnostics | 100 |
| 7 | unscoped project | dead_code | 100 |

Diagnostic parity uses disabled producers, preserving their honest coverage
response rather than depending on live language-server timing. Timing requests
leave servers configured normally and omit diagnostics. Runtime-note tests also
exercise a live mock transport with and without a raw note; the existing scoped
Rust/earlier-TypeScript-note integration test passed. This is not a live
TypeScript diagnostics certification.

Corpus: TypeORM `17e858da8d2becab1ca47826f670a4660da70142`, extracted from
`git archive`; dependencies were not installed. Semantic/trigram indexes were
disabled and the callgraph index remained enabled. The same four intentionally
invalid `column-readonly` codemod fixture paths named in the original profile
were excluded on both sides. Other invalid fixtures and incomplete analysis
were not suppressed.

## Interleaved release measurements

One `linux,4c` job measured both binaries in warm processes, after one scoped
warm-up each. Order alternated B/A, A/B, B/A, A/B, B/A. Toolchain:
`rustc 1.99.0 (b940084d7 2026-09-28)`, Python 3.14.4; release builds used four
Cargo jobs and identical temporary stderr-only phase probes.

| Iteration | Before (s) | After (s) |
|---|---:|---:|
| 0 | 7.414788 | 3.207455 |
| 1 | 7.820584 | 3.408462 |
| 2 | 7.815236 | 3.706931 |
| 3 | 9.824751 | 3.711819 |
| 4 | 8.514215 | 3.305484 |
| **Median** | **7.820584** | **3.408462** |

The median reduction is **56.42%**, or **2.29x** faster. The table below uses
means so its non-nested rows are additive. Decode timings are included inside
each scope phase; their individual measurements are retained in the JSON.

| Phase | Before mean (s) | After mean (s) |
|---|---:|---:|
| Dead-code scope reconstruction/filter | 4.488918 | 0.557454 |
| Unused-export scope reconstruction/filter | 1.024988 | 0.918544 |
| Duplicate scope reconstruction/filter | 0.310967 | 0.210186 |
| Cycle scope reconstruction/filter | 0.808502 | 0.836555 |
| Complexity scope reconstruction/filter | 0.120358 | 0.077371 |
| Fetch/decode five stored aggregates | 0.380093 | 0.345476 |
| Five freshness checks | 0.301011 | 0.285204 |
| Runtime-note selection | 0.636154 | 0.000001 |
| Render response | 0.000930 | 0.001134 |
| Inventory and remaining request/client work (residual) | 0.205992 | 0.236105 |
| **Total wall mean** | **8.277915** | **3.468030** |

The remaining global rollups are visible, not removed from the measurements.
Timing variation affected both binaries; this comparison is within one job,
not a claim that the older profile's absolute numbers reproduce unchanged.

Per-run wall samples, mean phases, parity hashes, toolchain, runner job identity,
and binary hashes are committed in
[`scoped-performance.json`](scoped-performance.json).
Baseline binary SHA-256:
`251d37b2c5a714205c5ef8fc179b51ffb662f30de5ca236978fb5296d1b6ff35`;
candidate:
`d222d4b81e41b590d7b0b5ddf0c3eafc3bd4166c9196cc1676adf6add3ba2164`.
Raw replies are written by the harness under its explicit output directory;
they are not checked into this repository.

## Reproduction

Prepare inputs locally inside this checkout from the two pinned Git commits:

```sh
mkdir -p local-ignore/scoped-inspect
git clone --no-checkout https://github.com/typeorm/typeorm.git local-ignore/scoped-inspect/typeorm-source
git -C local-ignore/scoped-inspect/typeorm-source archive --format=tar.gz \
  --output=../../../benchmarks/inspect-truth/typeorm-corpus.tar.gz 17e858da8d2becab1ca47826f670a4660da70142
python3 - <<'PY'
import json, subprocess
from pathlib import Path
paths = [
    'crates/aft/src/commands/inspect.rs',
    'crates/aft/src/inspect/manager.rs',
    'crates/aft/src/inspect/scanners/dead_code.rs',
    'crates/aft/src/lsp/manager.rs',
]
base = '3044304035317f5fbede501e87c2ef8fd26504ec'
sources = {p: subprocess.check_output(['git', 'show', f'{base}:{p}'], text=True) for p in paths}
Path('benchmarks/inspect-truth/scoped-baseline-sources.json').write_text(json.dumps(sources))
PY
```

Run on `linux,4c`, allowing enough time for two sequential release builds.
The source map and archive are temporary untracked inputs (so a remote runner
can synchronize them); remove them afterwards rather than committing them:

```sh
python3 benchmarks/inspect-truth/scoped_parity.py \
  --baseline-sources benchmarks/inspect-truth/scoped-baseline-sources.json \
  --corpus-archive benchmarks/inspect-truth/typeorm-corpus.tar.gz \
  --output local-ignore/scoped-inspect/reproduction --runs 5
```

If a runner deadline expires after both builds, the harness also saves a pair
of probe-instrumented release binaries under `target/scoped-parity-pair`; `--binaries-dir` can reuse those binaries in
a new output directory without another compile. This is a build cache, not the
location of retained measurement evidence. The successful final measurement
reused both binaries from the completed release builds of the final sources,
so neither process competed with this worker's compiler during timing.

## Verification notes

- Rust format check passed (rustfmt 1.10.0-stable).
- Inspect library tests: 350 passed, 4 ignored; runtime-note tests: 5 passed.
- Binary tests: 132 passed.
- Inspect integration tests: 122 passed, 4 ignored, one fixture precondition
  failed before calling inspect (`before.errors.is_some()`). The isolated retry
  passed without code changes. Real rust-analyzer tests can silently skip in
  this suite; no real-server certification is claimed.
- Windows `cargo check --tests`, with warnings denied, finished successfully;
  this is compile-only, not a Windows runtime test.
- Harness unit tests: 5 passed. Separate negative-test variants temporarily
  disabled cached aggregation reuse, contribution-byte and graph-revision
  identity checks, the no-note shortcut, summary/field-order comparison, and the
  unlocked-discovery and producer-barrier checks. Each caused its corresponding
  test to fail; each temporary change was restored from the staged implementation
  with an empty unstaged diff.
- Development unit runs exposed fixture mistakes in the added invalidation
  assertions (an absolute path passed to a relative-path deletion API, and a
  no-op refresh that correctly did not advance the graph revision). Those were
  corrected without changing existing response expectations. A two-build benchmark exceeded the runner's
  30-minute deadline after six passing parity cases; the completed final run is
  the only timing set used above.
- The follow-up `aft_inspect` diagnostic check covered both changed Rust files
  authoritatively with zero diagnostics. Other inspection categories remained
  unavailable because the editor's checkout graph view was not ready; explicit
  Rust test/compile gates are the authoritative verification.
- Release builds emitted the baseline unused `VecDeque` import warning in
  `cache_freshness.rs`; it is outside this change and was not modified.

## LSP lock-boundary regression

`scoped_runtime_note_discovery_releases_lsp_manager` runs the actual inspect
note-selection helper with a note-bearing mock client. Its discovery closure
calls `ctx.try_lsp()` and requires the manager lock to be available. Before the
lock-boundary fix it failed with `producer discovery must run without the LSP
manager guard`, while the three other selected note tests passed. A separate
mutation retaining the guard reproduced that single failure. The panic-closure
no-note test remains, now at the inspect helper so it also covers real guard
lifetimes rather than only a standalone manager.

The command-level `scope_producer_keys` only combines precomputed keys or calls
`inspect/diagnostics_category.rs::scope_producer_keys`. Its fallback uses scoped
coverage candidates and `scoped_diagnostics_sweep.rs::producer_keys_for_file`,
which consult registry configuration and workspace paths. These functions do
not receive an `AppContext`, call `ctx.lsp()`, or reacquire the LSP-manager lock.
The unchanged categories' remaining global work is still included in the phase
table above; this lock fix does not remove those costs.

## Controlling the producer race without dropping a response field

The first untraced 5+5 fresh-session observations showed **baseline pull=0**
(null=3, configure_warm=2) versus **candidate pull=4** (null=1). The initial
follow-up comparator stopped on that field difference; it was not ignored.
Temporary stderr-only tracing then showed baseline pull=2/null=3 and candidate
pull=3/null=2. All five accepted demands were raised by the existing
`runtime_drain.rs::drain_callgraph_store_events` Ready-install site during the
preceding project warm-up. The trace recorded every scheduler tick and response
read. No scoped reuse raised demand between its arrival and response read.

Tracing shifts timing, so these counts do **not** show that the candidate's
average demand timing shift is zero. The most likely elapsed-time shift versus
the original binary is the faster cached rollup and skipped no-note discovery;
with note-bearing clients, moving discovery outside the manager guard can also
let other LSP work progress sooner. The lock-only follow-up runs after payload
construction and does not itself write the scheduler reason. The barrier below
controls the startup race rather than claiming to remove its timing effect.

The identical barrier code in the original baseline and optimized candidate
release binaries now:

1. Seeds the real published stores using blocking inspect calls and a successful
   `callers` query for `DataSource::initialize` in its known TypeORM source file.
2. Closes that seed process, so its outstanding cold-producer events cannot
   mutate the measured session, and restarts from the same persisted stores.
3. Uses the same real navigation query and blocking project inspects until the
   graph is published, all five Tier-2 summaries have produced counts rather
   than pending/unavailable rows, and a scheduler refresh reason is observable.
   It does not substitute values for unknown observations or sleep for a guessed
   duration. Parse failures remain named incomplete analysis, not pending work.
4. Requires both arms to have the **same observed reason** before comparison.
   The final run reached `configure_warm` in both arms (51 observations before,
   43 after), and every later response still includes that field in the full
   comparator. No response value is overwritten and no normalization exception
   was added.

An earlier pull-only barrier timed out: the baseline legitimately selected
`configure_warm` after its cold delay instead of ever selecting `pull`. That
predicate was too specific, not evidence of failed analysis. The final barrier
accepts either real scheduler refresh, requires matched states, and uses the
cached-process restart to remove undrained cold producer events. Its negative
control rejects missing graphs, missing counts, null/unknown reasons, and
pending/building summaries.

The final controlled run passed all eight scope/section comparisons and five
interleaved timing pairs. The unchanged categories' remaining global work is
still approximately two seconds in the mean phase breakdown, and has not been
removed from either the response or the measurements.
