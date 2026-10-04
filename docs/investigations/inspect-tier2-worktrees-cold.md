# Checkout-local Tier-2 inspection and cold-build disclosure

## Evidence

References describe the implementation delivered with this investigation. The
reported prefrontal daemon and its cold-reboot artifacts were not available in
the isolated worker checkout; the reported 57-second observation is not a
measurement of a complete build's duration.

- `crates/aft/src/commands/inspect.rs:523`: scoped requests previously skipped
  every Tier-2 worker, even when a checkout-local view was ready. They read only
  the inspect cache. That cache is checkout-scoped, not borrowed from the legacy
  callgraph artifact owner. Thus a cold worktree had no answer even though its
  view could supply the graph. The borrow-only legacy writer restriction is
  still appropriate; applying the scoped bypass to a ready view was not.
- `crates/aft/src/inspect/manager.rs:2123`: the pinned-view scan already provided
  the graph adapter. It now supplies the Oxc import graph as well (for cycles),
  and returns an ephemeral result rather than requiring an inspect cache writer
  or persisting under a temporary no-legacy-fallback configuration.
- `crates/aft/src/inspect/manager.rs:2485`: cached freshness compares source
  contribution metadata/content and the complete current file set. A stale
  outcome also covers a changed configuration/aggregate hash. Reboot alone
  does not imply stale data. The previous generic stat-verification message
  could not identify which condition differed in the live report; no claim
  about that particular cache's failed file is justified without its artifacts.
- `crates/aft/src/commands/inspect.rs:2312`: the scope headline previously read
  only metrics' `files`, defaulting to zero when metrics did not finish. The
  diagnostics coverage denominator was computed independently. This explains
  how zero source files and 606 authoritative diagnostic files could coexist;
  a regression reproduces exactly those numbers with an unavailable metrics
  scanner. Scoped accounting now prefers the diagnostic sweep's coverage corpus,
  whose files are either authoritative or explicitly uncovered, and falls back
  to metrics when no sweep supplied coverage. Individual scanner counts still
  describe each scanner's actual work. No diagnostics collection or authority
  rule is changed.
- `crates/aft/src/commands/inspect.rs:71`: a waiting operation gets half the
  remaining request work budget, capped at 60 seconds; five seconds are reserved
  for response assembly. A default 120-second request can therefore stop waiting
  near 57 seconds even while a background aggregate continues. This is a wait
  deadline, not evidence that the build has stalled or needs restarting.

## Behavior

Scoped and read-only worktrees can now compute from their own pinned view. The
fallback reader (`manager.rs:969`) verifies HEAD, content keys, and source-set
membership, including deleted tracked and newly untracked sources. It refuses
an older publication rather than falling back to the owner's graph. Borrowed
Tier-2 results are not served by this change, so no sibling checkout's findings
are presented as local findings.

An unscoped request holding the **inspect writer** remains on the existing
persisted contribution/reuse pipeline, including when views are enabled. The
ephemeral path serves scoped requests and requests without that writer. Inspect
ownership and legacy callgraph ownership are separate: a linked worktree can
have its own inspect writer while borrowing legacy callgraph artifacts.

An unfinished aggregate reports builder state, start/elapsed time, its current
activity, and an approximate remaining duration from the last completed scan in
the session. Warm cache reuses are not duration samples. On a cold start the
estimate is explicitly unavailable: without a completed sample there is no
honest completion prediction. Source scanning, freshness checks, callgraph
projection, and assembly update the activity description. Queue/gating state is
preserved. This does not extend the request's deadline or busy-wait for a build.

When the checkout has an older complete aggregate, it is returned under
`summary.<category>.last_complete`, explicitly `stale: true` with its age in
seconds and scope-filtered payload. It never becomes the current category's
count or findings. The local incomplete-category line names building activity,
the estimate, and the stale age rather than asking for a bare retry. The revision
is based on `e66e9b0acd915559b9fe87a9eaf3a116e49dd1e5` and leaves that delivery's
single status line and gap-line rendering intact. Building/stale disclosure adds
no wait stamp, phase name, or retry instruction to the text.

## Verification IO measurements

`views/read.rs:29,59,75` counts actual source reads, metadata checks, and bytes
passed to the callgraph key hash. The counters are test-only; they do not infer
IO from a proxy such as findings or elapsed time. These are **source-verification
IO**, not OS-wide syscall counts: Git/pin/database reads and ignore-walker
metadata are excluded. Existing initial/final root stat sweeps remain; this is
not a claim that the complete blocking inspect is now sublinear in repository
size.

| Corpus / per verification call | Extra per-source metadata checks | Source reads | Source bytes hashed |
| --- | ---: | ---: | ---: |
| 3,002-file fixture, previous cold or warm | 0 explicit (plus its walker) | 6,004 | 277,816 |
| 3,002-file fixture, revised cold using inspect's root stats | 3,002 | 3,002 | 138,908 |
| 3,002-file fixture, revised warm using inspect's root stats | 0 | 0 | 0 |
| 3,002-file fixture, revised warm without supplied stats | 3,002 | 0 | 0 |
| Worker repository, 3,215 source files, previous two-pass verifier | 0 explicit (walk excluded) | 6,430 | 92,987,012 |
| Worker repository, revised cold using root stats | 3,215 | 3,215 | 46,493,506 |
| Worker repository, revised warm using root stats | 0 | 0 | 0 |

The fixture exercises `current_checkout_view` against a real published graph.
The repository measurement exercises the same source-verification helper with
manifest keys built from the worker checkout's actual files; it does not build
or benchmark a full repository callgraph. A blocking request passes its already
captured root stats to the reader, avoiding an additional walk. In the fixture,
the existing initial/final root stat sweeps would still stat 6,004 source files
in total; the revised warm reader adds none to that cost.

The manager retains verified source metadata/key tuples. Size/mtime or a changed
manifest key triggers content verification; ordinary unchanged reads reuse the
tuple. The search verification invalidation ticket clears this memo on watcher
events and overflow/reconciliation, including edits preserving size and mtime.
Ticket changes during verification reject the reader rather than certify a
racing edit. Idle eviction releases the memo. A watcher invalidation deliberately
requires a conservative content recheck, not a guessed list of changed files.

`inspect_checkout_view_warm_verification_does_not_rehash_corpus` pins these
counts and rejects a watcher-reported same-size/same-mtime edit. Defeating the
memo changes warm reads from zero to 3,002 and reddens that exact test.
`inspect_views_owner_persists_and_reuses_tier2_contributions` proves that an owner
persists both source contributions and that its second unchanged request reaches
reuse without scanning any additional source files. Routing that request through
the ephemeral path reddens the persistence assertion (zero contributions rather
than two).

## Paired real rust-analyzer comparison

Tool versions: Cargo 1.99.0, rustc 1.99.0, rust-analyzer 1.99.0 (all the installed
2026-09-28 toolchain). The isolated samples required the real server with:

```text
AFT_TEST_REQUIRE_RUST_ANALYZER=1 cargo test -p agent-file-tools --test integration scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer
```

Unchanged base `205903927a5cc717199cb6c8085560e5070f8265`: **12 invocations,
11 passed, 1 failed**. Runs 1–6 and 8–12 passed; run 7 failed with
`rust-analyzer's own error for the removed field is missing`, while the rustc
field error was present. Testing was done by checking out the unchanged revision
inside the same worker worktree, with committed task changes preserved on its
branch, not by touching the operator's main checkout.

Task branch: **12 comparable invocations, 6 passed, 6 failed**: the original
delivery had 1 pass / 3 failures (two of those invocations were within its broader
inspect suite), the rebased pre-revision sample had 2 / 2, and the revised sample
had 3 / 1. All failures had the same missing analyzer-origin report, not a missing
compiler error. The small samples and different concurrency of the first four
invocations do not establish equal failure rates or prove a causal improvement.
They **do** establish that the unchanged base has the same failure. The test's
assertions remain intact. Redundant warm coverage injection was removed, leaving
diagnostics collection and sweeps unchanged; accounting still consumes their
existing coverage payload.

The later parallel inspect-suite gate also once lacked the compiler-origin
report in the analogous AFT-edit test (the analyzer-origin field error was
present). Its isolated required-server rerun passed. This additional observation
is recorded rather than weakening the assertion or claiming it was reproduced
on the unchanged base. The serialized inspect-command suite then passed all 100
selected tests (four existing ignores), excluding only the separately compared
outside-edit test. The three status-transport inspect tests also passed.

## Regression coverage

- `scoped_inspect_views_worktree_reports_checkout_only_dead_function`: a real
  linked worktree retains a borrow-only legacy callgraph, publishes its edited
  view, reports a dead function absent from the owner, keeps a called function
  live, and refuses the view after another unpublished edit. More than a hundred
  out-of-scope functions also prove the scope is applied to full contributions
  before the existing project findings cap; the ephemeral path matches the
  persisted path's scoping contract.
- `scoped_inspect_building_discloses_progress_and_last_complete`: deterministic
  file-release hooks hold a cold build and a later rebuild. Both scoped reads
  and the unscoped wait-deadline path disclose building activity; the later
  scoped read also carries the stale, aged result. No synthetic CPU load is used.
- `scoped_inspect_file_accounting_survives_unfinished_metrics`: metrics is
  unavailable but diagnostics covers 606 files; both the scope headline and
  diagnostic coverage report 606, without an empty-scope sentinel.

The original implementation failed all three named regressions before the
fixes. Mutation controls and exact gate results are recorded in the worker's
delivery declaration. These fixture checks do not measure prefrontal's complete
cold-build latency; that still requires the live root's build-start/build-ready
timestamps or a permitted cold run in that environment.
