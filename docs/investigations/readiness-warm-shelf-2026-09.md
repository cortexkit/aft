# Readiness warm-up: does a warm shelf make the first answered call earlier?

Measured 2026-09-23 on an Apple M5 Max (18 cores), release build, with
`readiness_warm_burst_bench` (kept at tag `keep/readiness-warm-shelf-bench`,
`crates/aft/examples/`). Storage was private to the bench
(`AFT_STORAGE_DIR=/tmp/aft-warm-bench/storage`); no daemon was involved.

**Result: no. Warm-then-flip is later than flip-then-lazy at both p50 and p99,
so production keeps `NoWarmer`.**

## What was compared

A burst of first calls from clients that all reopen at process start, spread
round-robin over 24 real git roots of 89 to 3591 indexed files (6 or 7
sessions per root). Each client makes one `grep`-shaped first call. Tool calls
run on 8 workers (the executor's default pool on this host); post-bind loads
take one of the 2 cold-build slots.

* **flip-then-lazy (today):** ready at once. Each root's first bind starts the
  writable post-bind search load (HEAD probe, `cache.bin` decode, strict
  verify, since a fresh process has no verify memo). A call arriving before
  the load finishes is answered by the fallback directory walk, as `grep` does.
* **warm-then-flip:** warm-up decodes every root's `cache.bin` onto a shelf
  through the same 2 slots, under the 10 s plain-start budget, then flips.
  After the flip each root's post-bind load adopts its entry, which skips the
  decode but still probes HEAD, re-checks the ignore-rules fingerprint and
  verifies strictly.

The refusal window is modelled, not driven through a daemon: every client
retries `module_warming` on the schedule of `@cortexkit/subc-client` 0.8.1
`openCachedRoute` (attempts at 0, 100, 300, 700, 1500 and 3100 ms; the sixth
refusal fails the open). Decode, HEAD probe, fingerprint, strict verify,
indexed grep and fallback grep are the real code on real roots. Adopted and
disk-loaded indexes returned identical uncapped, sorted match sets for every
root (the bench asserts this).

## Burst results

144 first calls over 24 roots, pattern `TODO`, 3 interleaved repetitions:

| arm | answered | failed opens | p50 ms | p99 ms |
|---|---:|---:|---:|---:|
| flip-then-lazy | 432 | 0 | 1234 | 2485 |
| warm-then-flip | 432 | 0 | 2855 | 4178 |

170 first calls over 24 roots (the busy-fleet route count), pattern `fn `, 2
repetitions:

| arm | answered | failed opens | p50 ms | p99 ms |
|---|---:|---:|---:|---:|
| flip-then-lazy | 340 | 0 | 1400 | 3717 |
| warm-then-flip | 340 | 0 | 2941 | 5220 |

The warm arm flipped after 0.8 to 1.8 s. Its roots also became indexed later
(index-ready p50 3.2 to 4.4 s against 2.2 to 3.8 s), because their post-bind
loads cannot start before the flip.

## Why the shelf cannot win

1. **No first call waits for a decoded artifact.** `grep` answers with the
   fallback walk while the index loads (`grep_executor.rs`, the
   `fallback_grep` branch); callgraph ops open the persisted SQLite store
   read-only and synchronously (`CallGraphStore::open_readonly`, no decode);
   outline, zoom and read parse the file; semantic search reports
   building/not ready and falls back. So a shelf does not shorten any first
   call. In both arms 70 to 95 % of first calls were answered by the
   walk.
2. **The decode is the small part of the load.** On a fresh process the load
   is HEAD probe + decode + a strict verify that walks the tree and hashes
   every indexed file. Adoption also has to redo the ignore-rules fingerprint
   (a recursive walk) to prove the entry still matches. The decode is 20 to
   120 ms per root; the full load 60 to 980 ms.
3. **The refusal window is pure added latency,** and the client's retry
   backoff rounds it up to the next attempt (a flip at 0.8 s is seen at 1.5 s).

## A finding the design doc had wrong

`docs/design/subc-readiness-warmup.md` says callers retry `module_warming`
until a 30 s route-open deadline. The Node SDK stops earlier: `maxAttempts: 6`
in `DEFAULT_RECONNECT_BACKOFF` also caps route-open retries, so the sixth
refusal (about 3.1 s after the first attempt) fails the open with "retry
budget exhausted", well inside the 30 s deadline. Any warm-up longer than
about 3 s would turn delayed calls into failed ones, whatever the 10 s budget
allows.

## Per-root phases (uncontended, page cache warm)

| root | files | decode ms | post-bind load ms (disk) | post-bind load ms (adopted) | fallback grep ms | indexed grep ms |
|---|---:|---:|---:|---:|---:|---:|
| (aft checkout) | 3591 | 99.0 | 420.4 | 402.0 | 99.2 | 1.7 |
| CortexKit/prefrontal | 2974 | 80.8 | 783.4 | 572.7 | 206.5 | 33.7 |
| CortexKit/benchmarks | 2726 | 123.3 | 979.0 | 428.4 | 176.2 | 22.1 |
| CortexKit/magic-context | 2364 | 78.0 | 737.5 | 311.1 | 69.5 | 3.0 |
| 5kat/5kat.com | 2070 | 38.4 | 363.8 | 144.4 | 112.9 | 40.4 |
| Bebi/bebi-panel-v2 | 1912 | 70.1 | 382.7 | 198.6 | 70.4 | 15.3 |
| Bebi/rtbkit | 1758 | 52.0 | 334.3 | 123.4 | 34.2 | 2.4 |
| Bebi/bebi-panel-dsp | 1508 | 42.9 | 282.1 | 158.7 | 45.9 | 4.7 |
| CortexKit/synapse | 903 | 62.2 | 243.2 | 134.1 | 21.6 | 1.6 |
| CortexKit/subconscious | 691 | 35.3 | 195.5 | 100.3 | 22.6 | 2.5 |
| CortexKit/cortexkit-e2e | 618 | 39.2 | 155.9 | 91.5 | 20.3 | 3.4 |
| CortexKit/broca | 485 | 43.9 | 150.8 | 82.0 | 22.2 | 1.6 |
| CortexKit/alfonso-ios | 368 | 40.4 | 122.0 | 66.4 | 6.4 | 0.7 |
| CortexKit/thalamus | 332 | 33.7 | 123.1 | 70.8 | 9.2 | 0.9 |
| CortexKit/engram | 170 | 24.7 | 66.2 | 48.5 | 4.0 | 0.1 |
| CortexKit/cerebellum | 230 | 27.9 | 100.5 | 53.8 | 7.2 | 0.0 |
| CortexKit/claustrum | 216 | 36.0 | 88.4 | 57.0 | 6.5 | 0.4 |
| CortexKit/callosum | 163 | 36.5 | 73.6 | 64.5 | 5.7 | 0.5 |
| CortexKit/openai-auth | 193 | 38.1 | 76.5 | 51.4 | 3.8 | 0.4 |
| CortexKit/plexus | 165 | 62.0 | 144.7 | 104.1 | 36.2 | 0.2 |
| CortexKit/insula | 154 | 22.4 | 63.5 | 44.0 | 2.5 | 0.4 |
| CortexKit/fusiform | 108 | 22.1 | 59.1 | 42.9 | 4.4 | 0.8 |
| CortexKit/astrocyte | 102 | 21.9 | 112.7 | 44.3 | 2.4 | 0.4 |
| CortexKit/commons | 89 | 31.5 | 65.3 | 42.1 | 3.6 | 0.3 |

The "adopted" column runs after the "disk" column on the same root, so part
of its advantage is page cache left warm by the first run; it is an upper
bound on what adoption saves.

## Reproduce

    cargo build --release -p agent-file-tools --example readiness_warm_burst_bench
    AFT_STORAGE_DIR=/tmp/aft-warm-bench/storage \
      target/release/examples/readiness_warm_burst_bench \
      --storage /tmp/aft-warm-bench/storage --roots roots.txt --calls 144 --reps 3

`roots.txt` lists one root per line. The first run builds each root's index
into the bench storage; later runs reuse it while HEAD is unchanged.
