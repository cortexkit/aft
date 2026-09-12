# opencode views branch-switch drill

## Finding

The isolated views-on subject exercises content-addressed publication without restarting or mutating the live daemon. Views-on used 0 embed batches across the four switches; the legacy arm used 14.

## Run 3 — isolated views-on, warm owned baseline

Observed at `2026-09-12T15:11:02Z` against `5716f8ba60e7`.

- A: first-parent commit `a085bf62a459` (300 changed files)
- B: branch `refs/remotes/upstream/v2-timeouts` (`b85cf3d67fe3`, 298 changed files)
- Views-on subject: standalone AFT with isolated view storage; warm-up `3857 ms`.
- Views-off subject: standalone AFT on an independent baseline clone and isolated storage; warm-up `3159 ms`.

`cpu_s` and `rss_delta_mb` use the active standalone subject PID for each row. PID changes are recorded as defects rather than subtracting unrelated processes.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | 72491 | 0 | 0 | 57.79 | 2534.734 | 72738 | — | — | 3 | 42.16 | 2732.812 | 36871 |
| a085bf62a459→HEAD | 72000 | 0 | 0 | 58.3 | 453.969 | 71999 | — | — | 4 | 41.78 | 140.766 | 35297 |
| HEAD→refs/remotes/upstream/v2-timeouts | 64156 | 0 | 0 | 51.97 | 318.406 | 64156 | — | — | 3 | 47.56 | 14.875 | 39241 |
| refs/remotes/upstream/v2-timeouts→HEAD | 60874 | 0 | 0 | 49.43 | -3.188 | 61087 | — | — | 4 | 39.6 | 8.703 | 33031 |

### Four mechanisms

1. **Forward correctness had both a probe defect and a publication defect.** The Run 2 search query was free text and candidate selection proved only that a name existed on the target; `callers` needs a symbol with a resolvable call site. The drill now uses an exact word-boundary query, requires at least two target occurrences, and verifies both search and a non-empty callers result. A pre-fix instrumented run still showed the product defect: status reported search/semantic `ready` after 934 ms, then `index_event ... outcome=pending ... pending_paths=1`, repeated `callers ... symbol_not_found` for 300 s, and no published event. The semantic-refresh completion path did not retry the pending view publication, so callgraph queries remained pinned to HEAD. Completion now publishes the pending paths; all Run 3 probes converge.
2. **Switch-back embeddings came from the legacy semantic watcher worker.** Views publication did not suppress the resident `SemanticIndex` refresh, which embedded every watcher-invalidated path from the live checkout. The worker now derives the view semantic full key from source bytes, path, producer version, and model fingerprint, loads an existing `SemanticBlob`, and embeds only misses. Both return legs report zero embed calls; this final warm run also reused vectors on both forward legs.
3. **The missing generations were failed publications, not valid no-ops.** The target fingerprints are `4b5c2543317f465023...` (A) and `2d6d879d2a496ba71f...` (B), distinct from HEAD `322b78e53d463f91c...`. Run 2 retained the HEAD fingerprint after `outcome=pending`; it had not published an identical manifest. The drill now classifies publication by manifest fingerprint rather than generation alone, and semantic completion publishes the distinct target manifest.
4. **The puts count mixed roots.** Publication counters without `root=` admitted concurrent work from other roots. View publication phase events and publication summaries now use the same `root=<canonical checkout>` grammar as other `index_event` lines, and the drill accepts counters/events only for the measured root. Run 3 records zero blob puts on every switch.

### Cost attribution

The earlier views-on sample was 281 CPU-s and +1.3 GB RSS versus 125 CPU-s for legacy. Run 3 phase events rule out blob insertion and embedding as the views-only cause: every views row has `blob_puts=0` and zero embed batches. On the two forward publications, `derived.sqlite` materialization took 43,767 ms and 35,783 ms, versus only 1,701/2,460 ms for manifest assembly and 208/285 ms for blob lookup; pointer publication added 7,263/4,380 ms. Materialization therefore consumed 82-83% of the published phase. The checkpointed database is 269,889,536 bytes (257.4 MiB); building a temporary generation alongside the published one accounts for about 514.8 MiB, matching the known ~517 MiB materialization footprint. The remaining RSS variation is process cache/allocator residency. The follow-up target is consequently incremental `derived.sqlite` materialization: remove the 35.8-43.8 s full rewrite and roughly 257 MiB temporary generation per switch.

### Run 3 defects

No correctness, publication, PID-change, or switch-back reuse defect observed.

## Run 1 — confounded (historical)

Run 1 used a standalone views-on process and a cold read-only baseline. Its parity divergence and timeout rows are readiness artifacts, not views parity findings. The raw record remains in `branch-drill-run1-confounded.json`.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | — | 0 | 3 | 103.91 | -288.0 | timeout | 242.74 | 13.562 | timeout |
| a085bf62a459→HEAD | 111284 | 0 | 4 | 111.97 | 2905.906 | 4637 | 224.99 | -4.797 | timeout |
| HEAD→refs/heads/dev | — | 0 | 0 | 81.07 | -3030.219 | timeout | 214.63 | -6.328 | timeout |
| refs/heads/dev→HEAD | 212902 | 0 | 0 | 156.48 | 2407.453 | timeout | 3.11 | 10.719 | 7504 |
