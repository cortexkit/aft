# Callgraph blob extraction work census

## Inputs and method

The immutable inputs are tracked regular files from AFT revision
`be561ad598bba4576ae92fac9b9e0813b9233b06` and opencode revision
`5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`. Supported extensions are selected by
`parser::detect_language`. Both extractors receive the same source bytes,
language label and the runtime `ruled-callgraph-v2` extractor-version string for
the final parity comparison. The initial investigation used `work-census-v1`.

The baseline is an archived workspace with test-only instrumentation, frozen
before optimization. The final fixed-base workspace adds only the deterministic
caller-evidence fix and producer-version bump from commit
`8442ff65547bca2118a9c3690eb0dea7569f7130`; it retains all original extraction
algorithms. Parser invocations are intercepted by a test-only parser wrapper;
restricted Rust macro-body ranges are recorded separately from whole-file
parses. Node-kind copies and child-vector collections are counted at their
construction sites. Baseline ordinal and dispatch counters count candidate
visits; indexed lookup counters count consumed nodes, binary-search comparisons
and Fenwick bucket visits. Position counters count source bytes scanned by
position helpers and index construction, including call attribution and
structural references. Child-vector collections include empty vectors, so they
are not a count of heap allocations. The allocation tests additionally exercise
cursor traversal and call-kind lookup through the existing counting allocator.

## Baseline measurements

These are debug/test-profile, instrumented runs. Their wall times are contextual
measurements, not an idle-machine performance claim.

| File | Source bytes / lines | AST nodes | Whole-file / macro parses | Kind copies | Child-vector collections | Ordinal visits | Dispatch visits | Position bytes scanned | Call-kind vectors | Extract wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| AFT `crates/aft/src/subc/mod.rs` | 589,914 / 14,841 | 125,838 | 6 / 0 | 125,838 | 251,676 | 975,370,338 | 468,908,676 | 195,576,446 | 469,046,205 | 83.824 s |
| AFT `packages/opencode-plugin/test/load-matrix/load-matrix.ts` | 119,139 / 2,999 | 21,193 | 4 / 0 | 21,193 | 21,193 | 24,647,459 | 12,135,900 | 3,474,829 | 12,159,017 | 2.148 s |
| opencode `packages/client/src/promise/generated/types.ts` | 215,570 / 6,265 | 66,653 | 4 / 0 | 66,653 | 66,653 | 50,722,933 | 0 | 116,879,920 | 72,511 | 2.576 s |

Fixed-base corpus extraction produced 3,526 AFT blobs (1,078,460,157 serialized
bytes, 2,183.67 s) and 5,037 opencode blobs (938,552,340 serialized bytes,
397.97 s). Every supported regular file in each snapshot was extracted. Before
the determinism fix, the Rust sample took 84.231 s and the two full corpora
produced 1,076,172,901 and 931,275,084 bytes respectively.

The baseline work-budget run failed the six expected guards: whole-file parse
count, kind copies, child vectors, position scans, call-kind vectors and lookup
growth. The two-item-macro guard passed. Doubling the synthetic TypeScript file
from 64 to 128 functions increased ordinal plus dispatch visits from 561,472 to
2,245,248 (approximately 4×).

## Head measurements

The same three immutable source files were measured with the optimized extractor
before starting the corpus comparison.

| File | Whole-file / macro parses | Kind copies | Child-vector collections | Ordinal index visits | Dispatch index visits | Position bytes scanned | Call-kind vectors | Extract wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| AFT `crates/aft/src/subc/mod.rs` | 1 / 0 | 0 | 0 | 3,680,814 | 345,099 | 589,914 | 0 | 7.981 s |
| AFT `packages/opencode-plugin/test/load-matrix/load-matrix.ts` | 1 / 0 | 0 | 0 | 513,997 | 46,699 | 119,139 | 0 | 0.143 s |
| opencode `packages/client/src/promise/generated/types.ts` | 1 / 0 | 0 | 0 | 1,759,295 | 0 | 215,570 | 0 | 0.299 s |

The head work-budget run passed: 14 tests passed, none failed, and one corpus
probe was ignored. The filter includes four existing extraction-related tests
as well as the nine work tests and the determinism regression. Synthetic lookup work grew from 47,743 to
103,822 visits for 64 to 128 functions (approximately 2.17×). The macro fixture
asserted exactly two range parses with distinct ranges and retained both symbols.

These wall times are instrumented debug measurements under fleet load.

## Implementation

One caller-owned whole-file tree is shared by call-data extraction, nested Rust
imports, Rust modules, member completion and dispatch. Necessary item-position
macro-body range parses are preserved. AST kinds borrow static grammar names;
decoding owns those names without changing JSON. Traversal uses cursors, and
smallest-enclosing-range queries use an offline start sweep with reversed-end
Fenwick minima. A shared position index retains both historical column-clamping
conventions. Call-kind lists are static slices.

At the baseline revision, `collect_reexport_refs` scans source text rather than
parsing a tree. Dispatch, however, allocates call-kind vectors inside node and
reference-candidate predicates, not merely once per file.

## Pre-existing nondeterminism and producer rebuild

The initial AFT corpus comparison stopped at
`benchmarks/aft-search/author_real_query.py`. Baseline serialization has 319,954
bytes; head serialization has 319,970 bytes. Inspection of the saved payloads
found exactly two differing JSON values:

| JSON path | Baseline | Head |
| --- | --- | --- |
| `$.refs[29].caller_symbol` | `at_pin` | `pinned_domains` |
| `$.refs[30].caller_symbol` | `at_pin` | `pinned_domains` |

Five subsequent baseline extractions produced two distinct blobs: runs 1/4/5
selected `pinned_domains`; runs 2/3 selected `at_pin`. Five optimized extractions
also produced both blobs. Their SHA-256 values were identical across versions:
`9edd8871b96c25d6f87558e74156f9b76257bf1361884e94a86d1db3eba5fb7e`
and `36e12a418273a043c2ecc2ca08e48d83a105ca0b1e63296ea3259229743d797c`.

The cause was a pre-existing bug: `at_pin` is nested inside `pinned_domains`, so
call attribution emits evidence for both enclosing callers. Construction used
randomized `calls_by_symbol` iteration, while sorting and deduplication omitted
`caller_symbol`. Deduplication therefore retained whichever caller happened to
arrive first. The standalone fix sorts both caller maps and includes caller
identity in reference ordering and deduplication, preserving both references.

Because this intentionally changes bytes and evidence identity, producer keys
advance from `callgraph-v1` to `callgraph-v2` and from `ruled-callgraph-v1` to
`ruled-callgraph-v2`, causing a one-time rebuild. Payload schema stays 1: the JSON
layout is unchanged. This fix is the first task commit, before the optimization.

The new determinism test performs 32 extractions of the real failing file,
asserts both enclosing callers remain, and checks canonical caller-map iteration
across fresh hash seeds. It failed before the fix at attempt 1, passed after the
fix, and failed at attempt 3 when hash-order construction and caller-omitting
keys were restored with a `NON-VACUITY BREAK` mutation.

## Final serialized-byte parity and work mutations

Every file is byte-identical between the deterministic fixed-base extractor and
the optimized head: 3,526 AFT files and 5,037 opencode files, totaling 8,563 files.
The head comparison consumed 1,078,460,157 AFT bytes and 938,552,340 opencode
bytes. No normalization of caller fields or other payload data was used.

Seven independent work mutations each failed exactly one named guard, with the
other eight work tests passing. The temporary mutants actually performed a
second whole-file parse, repeated macro-body parses, copied kind strings,
collected child vectors, scanned every node for each query, constructed a second
position index, or allocated call-kind vectors. All mutants were restored from
the staged live state; the unstaged `git diff --stat` was empty after restoration.

## Required gates and environment limitations

All work mutations were restored before these commands. Toolchain versions are
listed below; Cargo commands used three-hour timeouts and were run sequentially.

| Gate | Result |
| --- | --- |
| `cargo test -p agent-file-tools --lib callgraph -- --test-threads=1` | 340 passed, 0 failed, 5 ignored |
| `cargo test -p agent-file-tools --lib views:: -- --test-threads=1` | 159 passed, 0 failed, 6 ignored |
| `cargo test -p agent-file-tools --bin aft -- callgraph` | 6 passed, 0 failed |
| `cargo test --features test-hooks -p agent-file-tools --test rest -- callgraph` | 83 passed, 0 failed, 6 ignored |
| `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu` | Finished successfully, including test targets; compile-only, not Windows runtime verification |
| `cargo fmt --all -- --check` | Passed, silent success |

The default parallel lib filter initially passed 338 tests and failed two
runtime-drain demand tests. Both passed individually, then the complete filter
passed with one test thread. No production code or assertions were changed for
those concurrency-sensitive failures.

The additional broad integration filter passed 84 tests, failed 10 and ignored
one. All failures came from fixtures under `tests/fixtures/callgraph` configured
inside this isolated Git worktree: operations reported
`callgraph_unavailable` / `read_only_store_not_built`. The representative
`callgraph_cross_file_tree` test also failed identically on the deterministic
unoptimized fixed base, proving an existing fixture/worktree limitation rather
than an optimization regression. The fixtures were not rewritten. The separate
persisted-store acceptance target and temporary-project integration cases pass.

`aft_inspect` returned partial diagnostics because its rust-analyzer Cargo check
had not completed within the inspection budget. The actual library/test
compilations and strict-warning Windows check are the authoritative compiler
gates. No idle-machine timing claim is made: the later timing-window check found
load averages about 39/44/46 on an 18-logical-CPU host, with other compiler jobs
still active. The measured debug wall times above are retained with that caveat.

## Commands and retained evidence

Toolchain: Cargo 1.99.0, rustc 1.99.0, rustfmt 1.10.0-stable. Cargo commands use
an explicit three-hour timeout. The baseline commands run from
`tmp/extraction/base` with `CARGO_TARGET_DIR` set to the worktree's `target/`.

- `cargo test -p agent-file-tools --lib extraction_ -- --nocapture`: baseline
  produced six expected budget failures; fixed head passed 14 tests with one ignored.
- `cargo test -p agent-file-tools --lib extraction_corpus_bytes_and_work -- --ignored --nocapture`:
  baseline passed once for each full corpus; head passed once for each of the
  three single-file work probes.
- The same corpus command with `AFT_EXTRACT_COMPARE=1`: both full corpora passed
  against the deterministic fixed base.
- `cargo fmt --all -- --check`: passed (silent success).

Ignored evidence is retained under `tmp/extraction/`: `baseline-tests-resumed.log`,
`baseline-aft-resumed.log`, `baseline-opencode-resumed.log`,
`head-work-tests-fixed.log`, `head-subc-work.log`, `head-load-matrix-work.log`,
`head-opencode-work.log`, `head-aft-byte-comparison.log`, and
`byte-mismatch-fields.txt`, `fixed-base-aft.log`, `fixed-base-opencode.log`,
`fixed-head-aft-parity.log`, `fixed-head-opencode-parity.log`,
`mutation-determinism.log` and `mutation-work-*.log`. The original differing payloads are
`baseline-aft/benchmarks/aft-search/author_real_query.py` and
`baseline-aft/benchmarks/aft-search/author_real_query.head.json`.
