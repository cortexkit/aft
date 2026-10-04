# Checkout-local Tier-2 inspection and cold-build disclosure

## Evidence

References describe the implementation delivered with this investigation. The
reported prefrontal daemon and its cold-reboot artifacts were not available in
the isolated worker checkout; the reported 57-second observation is not a
measurement of a complete build's duration.

- `crates/aft/src/commands/inspect.rs:556`: scoped requests previously skipped
  every Tier-2 worker, even when a checkout-local view was ready. They read only
  the inspect cache. That cache is checkout-scoped, not borrowed from the legacy
  callgraph artifact owner. Thus a cold worktree had no answer even though its
  view could supply the graph. The borrow-only legacy writer restriction is
  still appropriate; applying the scoped bypass to a ready view was not.
- `crates/aft/src/inspect/manager.rs:2085`: the pinned-view scan already provided
  the graph adapter. It now supplies the Oxc import graph as well (for cycles),
  and returns an ephemeral result rather than requiring an inspect cache writer
  or persisting under a temporary no-legacy-fallback configuration.
- `crates/aft/src/inspect/manager.rs:2435`: cached freshness compares source
  contribution metadata/content and the complete current file set. A stale
  outcome also covers a changed configuration/aggregate hash. Reboot alone
  does not imply stale data. The previous generic stat-verification message
  could not identify which condition differed in the live report; no claim
  about that particular cache's failed file is justified without its artifacts.
- `crates/aft/src/commands/inspect.rs:2247`: the scope headline previously read
  only metrics' `files`, defaulting to zero when metrics did not finish. The
  diagnostics coverage denominator was computed independently. This explains
  how zero source files and 606 authoritative diagnostic files could coexist;
  a regression reproduces exactly those numbers with an unavailable metrics
  scanner. Scoped accounting now uses the diagnostics candidate corpus, whose
  files are either authoritative or explicitly uncovered. Individual scanner
  counts still describe each scanner's actual work.
- `crates/aft/src/commands/inspect.rs:71`: a waiting operation gets half the
  remaining request work budget, capped at 60 seconds; five seconds are reserved
  for response assembly. A default 120-second request can therefore stop waiting
  near 57 seconds even while a background aggregate continues. This is a wait
  deadline, not evidence that the build has stalled or needs restarting.

## Behavior

Scoped and read-only worktrees can now compute from their own pinned view. The
fallback reader (`manager.rs:959`) verifies HEAD, content keys, and source-set
membership, including deleted tracked and newly untracked sources. It refuses
an older publication rather than falling back to the owner's graph. Borrowed
Tier-2 results are not served by this change, so no sibling checkout's findings
are presented as local findings.

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
the estimate, and the stale age rather than asking for a bare retry. Existing
rendering structure and phase/status headers are left unchanged.

## Regression coverage

- `scoped_inspect_views_worktree_reports_checkout_only_dead_function`: a real
  linked worktree retains a borrow-only legacy callgraph, publishes its edited
  view, reports a dead function absent from the owner, keeps a called function
  live, and refuses the view after another unpublished edit.
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
