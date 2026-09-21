# Semantic embed build RSS investigation — September 2026 (issue #327)

## Result

**Not reproduced, and the mechanism is not named.** A daemon-level harness that
records resident set against embed batch number was built and run against four
binaries — v0.56.0, v0.56.1, v0.56.2 and the 0.57.0 development head — under
five load shapes. Every run was flat: resident set rose linearly with embedded
chunks at the index's own per-chunk cost and stopped when the build stopped.
Nothing resembling the reported 0.5–3 GB/min appeared in any of them.

That is a negative result, not a fix. What it buys is a large, measured
exclusion and a harness the next attempt can point at the reporter's own corpus
and backend. The one artifact that does ship as a guard is a regression test
bounding the embed loop's working set per chunk; it is proven to catch response
buffering in that loop and is documented below with its sensitivity floor.

## What the harness does

`docs/investigations/scripts/semantic-embed-rss.py` stands a whole daemon up
over the standalone NDJSON protocol against a synthetic corpus and a stub
embedding server on loopback. No plugin, editor or real backend is involved.
Corpus size, chunk count, batch size and per-batch backend latency are all set
from the command line, so batch count is driven directly rather than inferred.

Samples are recorded against the `stage=embed batch=N` number the daemon writes
to its own log, not against wall clock. That is the distinction the issue rests
on: an accumulator tied to work shows as a rising bytes-per-batch slope, one
tied to time does not.

Three load shapes layer on top of a plain build, because a quiet build exercises
far less of the daemon than a live session does:

- `--calls-per-second` — an agent issuing tool calls while the build runs
- `--reconfigure-every` — configure churn, which is what a reconnecting plugin does
- `--delay-ms` — per-batch backend latency, to match a real backend's pace

One setup detail worth recording because it silently produces the wrong
measurement: `semantic.backend` is a user-scoped setting. A project-scoped
`.cortexkit/aft.jsonc` drops it without complaint and the daemon falls back to
the local backend. The harness writes the backend into a user config and passes
`cortexkit_user_config_path`.

## Version comparison across both arrival windows

Same harness, same corpus (1,500 files / 30,000 chunks / 469 batches), same
stub backend at 120 ms per batch, one tool call per second. Slope is fitted
across the embed phase only.

| binary | MB per batch | peak RSS |
| --- | --- | --- |
| v0.56.0 | 0.271 | 331 MB |
| v0.56.1 | 0.330 | 315 MB |
| v0.56.2 | 0.304 | 312 MB |
| 0.57.0 dev head | 0.332 | 528 MB |

All four sit in one 0.27–0.33 MB/batch band. The reported build climbs about
22 MB per batch (run B: batch 680 → 1120 while RSS went 1.03 → 10.9 GB), which
is seventy times this band. The 22% spread between v0.56.0 and v0.56.1 here is
corpus and scheduling noise on a shared box, not a seventy-fold step.

**Neither arrival reproduces in this harness.** Bisecting the 27-commit
v0.56.0→v0.56.1 range was therefore not attempted: with no signal at the
endpoints there is nothing for a bisect to separate.

## Load shapes that were run and stayed flat

All on the development head, all measured against batch number.

| shape | scale | result |
| --- | --- | --- |
| fast quiet build | 4,000 files / 80,000 chunks / 1,250 batches | 0.201–0.207 MB/batch across six consecutive intervals; no quadratic term |
| slow build, real backend pace | 2.5 s per batch, 313 batches | 0.27 MB/batch, flat |
| agent-driven build | 375 batches, 1,453 tool calls | flat during build, returned to 184 MB and held for four minutes after |
| configure churn | 469 batches, 1,131 tool calls, 26 reconfigures | flat; one semantic build started, so supersession and adoption both held |
| embed lane alone | 400 batches of `model.embed` over HTTP | 29.3 → 30.9 MB total, i.e. flat |

The fast quiet build is the strongest of these. Its per-interval slope was
0.204, 0.207, 0.201, 0.203, 0.201, 0.201 MB/batch over 1,134 batches. A leak
that only appears late, or one that grows with the size of what is already
built, would bend that line. It is straight.

## What the legitimate working set costs

Worth writing down, because "flat" only means something against a known
baseline. Measured three ways:

- In-process build loop: 2.6 KB per chunk (54 → 86 MB over 12,000 chunks).
- Daemon, slow build: 4.2 KB per chunk.
- Counting allocator at the build's peak: 3,098 bytes per chunk.

The peak figure is higher than the after-build figure because while the loop
runs each chunk is held twice — once in the collected corpus being read and
once in the entry just produced. At 375,627 chunks that projects to roughly
1.2 GB, which matches the 1.4–1.9 GB band the reporter measured as stable on
v0.56.0. The reported failure is around 850 KB per chunk, three hundred times
this.

## The regression guard

`crates/aft/tests/semantic_embed_working_set_test.rs` drives a real build over
the `openai_compatible` lane against a stub server and asserts that live heap
bytes retained per embedded chunk stays under 4 KB at the build's peak.

Two design points matter:

**It samples inside the loop, not after it.** The first version of this test
measured after the build returned and was vacuous: a `Vec` accumulating every
batch's rows inside `build_from_chunks` is dropped when that function returns,
so the mutant and the clean build reported the same 1,877 bytes per chunk, byte
for byte. Sampling at each progress callback — which fires between batches, with
that batch's work still live — is what makes the bound mean anything.

**Live bytes are counted by a global allocator declared in the test binary.**
The count is exact and is not perturbed by other tests, by allocator free-list
behaviour, or by the operating system's page accounting. The allocator exists
only in that test binary; nothing in the product's allocation path changes.

Its sensitivity is bounded and should be stated plainly: an accumulator holding
less than about a kilobyte per chunk passes. A mutation retaining one copy of
every row's text (+496 bytes per chunk, 16%) does **not** trip it. What it
catches is buffering on the order of a vector or a response body per row.

## Measurement caveats

- Every number here is macOS arm64. The reporter runs Linux in a container.
- This box had other compiles running throughout, so wall-clock timings are
  noisy. Batch-indexed slopes and allocator-counted bytes are not.
- On v0.56.0 the daemon's resident set dropped from 330 MB to 54 MB about a
  minute after its build finished, where v0.56.1, v0.56.2 and the head all held
  ~314 MB flat. No log line explains it and it appeared in one run. Resident set
  on macOS counts pages the allocator has freed but not returned, so the most
  likely explanation is the operating system reclaiming them under pressure from
  the other builds on this box. **This is recorded as an observation, not as a
  finding, and no claim is made that it is a product difference.**

## Workaround for shipped 0.56.1 and 0.56.2 users

From the reporter's own matrix, not from anything measured here:

- **On 0.56.1 there is a workaround: the local fastembed backend.** One full
  3h+ CPU build completed stably in a 2.4–2.6 GB band. On that version the leak
  was remote-only.
- **On 0.56.2 there is no known backend configuration that avoids it.** Runs A
  and B leaked on `openai_compatible` and run C leaked on local fastembed, on
  the same tree.

The remaining levers on 0.56.2 do not fix the leak, they only keep the daemon
under the watchdog by making the build smaller or by not running it:

- Lower `semantic.max_files` below the corpus size, at the cost of coverage.
- Set `semantic_search: false`, which turns the feature off.

Downgrading to 0.56.0 restores memory stability but reintroduces the
oversize-row build aborts that #318 fixed, so it is not a clean escape for this
reporter.

## Where the next attempt should start

The exclusions above say the accumulator is not in the embed lane, not in the
build loop, not in the daemon's progress or status path, and not reachable by
tool-call or reconfigure churn on a synthetic corpus on macOS. What is left, in
the order worth trying:

1. **Run this harness on Linux**, ideally in a container matching the
   reporter's. Several of the untested paths are Linux-only: the inotify
   watcher over 44,200 files, the cgroup CPU-quota reader, and `/proc`-based
   process and memory accounting.
2. **Run it against the reporter's corpus rather than a synthetic one.** The
   harness takes a corpus of any shape; the cost-gate investigation established
   that language mix changes chunk count per file substantially, and the
   reporter's 8.5 chunks per file is well above this synthetic corpus.
3. **Drive the daemon through the subc bridge rather than standalone NDJSON.**
   The reporter's daemon runs under the plugin, and the bridge transport is the
   one layer this harness does not exercise. `bridge.request_timeout_ms` and
   `hang_threshold` appear in their config and have no analogue here.

Only after one of those reproduces is a bisect over the 27-commit
v0.56.0→v0.56.1 range worth running. The parent's correction stands and was
re-verified: no commit in that range touches `crates/aft/src/semantic_index.rs`
or `crates/aft/src/synapse_embed.rs`, and `Cargo.lock` moves only the two
workspace version strings, so no dependency changed under the remote lane
either.
