# opencode views branch-switch drill

## Finding

The running daemon still refreshes the legacy semantic index across branch switches; views do not yet prevent switch-back re-embedding. Daemon log evidence from the original investigation recorded roughly 259 files at 08:14, 08:30, and 08:35Z. The corrected run independently captured a085bf62a459→HEAD: 272 files/4 batches.

## Run 2 — daemon views-on, warm owned baseline

Observed at `2026-09-12T12:35:49Z` against `5716f8ba60e7`.

- A: first-parent commit `a085bf62a459` (300 changed files)
- B: branch `refs/remotes/upstream/v2-timeouts` (`b85cf3d67fe3`, 298 changed files)
- Views-on subject: running AFT subc daemon; warm-up `637 ms`.
- Views-off subject: standalone AFT on an independent baseline clone and isolated storage; warm-up `2987 ms`.

`cpu_s` and `rss_delta_mb` use the active subject PID for each row. The daemon PID is resolved again at every views-on switch; PID changes are recorded as defects rather than subtracting unrelated processes.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | 38290 | 1942 | 0 | 280.97 | 1362.719 | timeout | — | — | 3 | 124.55 | 96.188 | timeout |
| a085bf62a459→HEAD | 55343 | 1942 | 4 | 62.0 | 607.594 | 56434 | — | — | 4 | 39.72 | 1870.0 | 29250 |
| HEAD→refs/remotes/upstream/v2-timeouts | — | 1954 | 0 | 279.1 | -1312.484 | timeout | — | — | 3 | 120.13 | -47.609 | timeout |
| refs/remotes/upstream/v2-timeouts→HEAD | — | 1944 | 0 | 25.6 | 251.188 | 11979 | — | — | 4 | 43.07 | 35.031 | 31333 |

### Run 2 observations

- The two forward-switch timeout labels mean the fully-ready stores did not agree with the selected target-only callgraph probe; they are post-readiness correctness failures, not cold-build artifacts.
- Publication-log attribution is imperfect when other view-enabled roots publish concurrently: puts rows should be read together with `publication_log_offset` and the retained JSON observations.

### Run 2 defects

- views-on HEAD→a085bf62a459 did not return both correct probes after full readiness
- a085bf62a459→HEAD reused HEAD with puts=1942 and embeds=4
- views-on HEAD→refs/remotes/upstream/v2-timeouts did not return both correct probes after full readiness
- HEAD→refs/remotes/upstream/v2-timeouts did not publish a new pointer generation
- refs/remotes/upstream/v2-timeouts→HEAD did not publish a new pointer generation
- refs/remotes/upstream/v2-timeouts→HEAD reused HEAD with puts=1944 and embeds=0
- views-off HEAD→a085bf62a459 did not return both correct probes after full readiness
- views-off HEAD→refs/remotes/upstream/v2-timeouts did not return both correct probes after full readiness

## Run 1 — confounded (historical)

Run 1 used a standalone views-on process and a cold read-only baseline. Its parity divergence and timeout rows are readiness artifacts, not views parity findings. The raw record remains in `branch-drill-run1-confounded.json`.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | — | 0 | 3 | 103.91 | -288.0 | timeout | 242.74 | 13.562 | timeout |
| a085bf62a459→HEAD | 111284 | 0 | 4 | 111.97 | 2905.906 | 4637 | 224.99 | -4.797 | timeout |
| HEAD→refs/heads/dev | — | 0 | 0 | 81.07 | -3030.219 | timeout | 214.63 | -6.328 | timeout |
| refs/heads/dev→HEAD | 212902 | 0 | 0 | 156.48 | 2407.453 | timeout | 3.11 | 10.719 | 7504 |
