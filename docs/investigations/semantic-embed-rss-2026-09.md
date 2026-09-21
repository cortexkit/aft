# Semantic embed build RSS investigation — September 2026 (issue #327)

## Result

**Not reproduced, and the mechanism is not named.** A daemon-level harness that
records resident set against embed batch number was built and run against four
binaries — v0.56.0, v0.56.1, v0.56.2 and the 0.57.0 development head — on both
macOS and Linux, on both embedding backends, and at the reporter's own corpus
scale of 44,200 files and 5,525 embed batches. Every run was flat: resident set
rose linearly with embedded chunks at the index's own per-chunk cost and stopped
when the build stopped. Nothing resembling the reported 0.5–3 GB/min appeared in
any of them.

The closest run to theirs settles at 2.4 GB, the same order as the 1.4–1.9 GB
band they themselves measured as *stable* on v0.56.0 — somewhat above it, which
is what a slightly different corpus should produce. At the batch number where
their failing run was at 16.2 GB, this one is at 0.89 GB.

That is a negative result, not a fix. What it buys is a large, measured
exclusion and a harness that runs on Linux, in a container, on either backend,
which the next attempt can point at the reporter's own corpus. The one artifact
that does ship as a guard is a regression test bounding the embed loop's working
set per chunk; it is proven to catch response buffering in that loop and is
documented below with its sensitivity floor.

The most useful next step is no longer more harness work. It is one specific
measurement from the reporter, named at the end of this document, which would
split the remaining search in half.

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

## On Linux, which is where the reported builds actually run

The macOS numbers above cannot speak for a Linux container. The daemon's
watcher is FSEvents on one and inotify on the other, the ONNX thread-count
derivation reads a cgroup CPU quota that exists only on Linux, and memory
accounting comes from `/proc`. So the harness was rerun inside a container with
VmRSS read from `/proc/<pid>/status`, the field the reporter sampled, and with a
six-CPU quota set so `/sys/fs/cgroup/cpu.max` reads `600000 100000` rather than
`max`.

Same corpus for all three (3,000 files / 60,000 chunks / 938 batches), stub
backend at 100 ms per batch, one tool call per second, linux/amd64:

| binary | MB per batch | window |
| --- | --- | --- |
| v0.56.1 | 0.222 | batch 225 → 883 |
| v0.56.2 | 0.227 | batch 220 → 880 |
| 0.57.0 dev head | 0.193 | batch 218 → 887 |

Flat, and in the same band as macOS.

### The local backend, and the cgroup path

Every run up to this point went through the remote lane, which never
instantiates the in-process ONNX embedder. That left the second reported
arrival untested, and with it the cgroup quota reader, since the quota is only
consulted when deriving ONNX thread counts. Covering Linux without loading the
local embedder covers less than it looks like it does.

Run on linux/arm64 so inference is native — emulated inference would distort
exactly the thread and CPU behaviour this lane is being measured for. The
daemon confirms it reached the path:

    local embedder ready: model=all-MiniLM-L6-v2 intra_threads=3
    intra_threads_source=parallelism available_parallelism=6
    cgroup_quota_threads=6 token_type_ids=true

Over 625 batches of real ONNX inference at 199 chunks/s, resident set went
808.9 MB at batch 28 to 889.9 MB at batch 596: **0.143 MB per batch**, flat. The
high baseline is the model and its session, which is expected and constant.

### At the reporter's own scale

The runs above are all far smaller than the reported corpus, and an accumulator
could in principle be per-file rather than per-batch and only bite at size. So
the last run matched their shape directly: **44,200 files, 353,600 chunks,
5,525 batches**, against their reported 44,200 files / 375,627 chunks / 5,870
batches.

| batch | VmRSS |
| --- | --- |
| 76 | 0.69 GB |
| 1,024 | 0.89 GB |
| 2,266 | 1.14 GB |
| 3,796 | 1.45 GB |
| 5,342 | 1.77 GB |

**0.205 MB per batch**, straight from end to end, and the same slope the
938-batch run produced. Peak during persistence was 4.5 GB; it settled at
2.4 GB and held there for the following twenty minutes.

That end state is the same order as the 1.4–1.9 GB band the reporter measured as
*stable* on v0.56.0 — above it, which a different corpus should produce. Their
failing runs were at 16.2 GB by batch ~1120; this run is at 0.89 GB there.
Whatever is happening to them is not a function of corpus size.

## Load shapes that were run and stayed flat

All on the development head, all measured against batch number.

| shape | scale | result |
| --- | --- | --- |
| fast quiet build | 4,000 files / 80,000 chunks / 1,250 batches | 0.201–0.207 MB/batch across six consecutive intervals; no quadratic term |
| slow build, real backend pace | 2.5 s per batch, 313 batches | 0.27 MB/batch, flat |
| agent-driven build | 375 batches, 1,453 tool calls | flat during build, returned to 184 MB and held for four minutes after |
| configure churn | 469 batches, 1,131 tool calls, 26 reconfigures | flat; one semantic build started, so supersession and adoption both held |
| embed lane alone | 400 batches of `model.embed` over HTTP | 29.3 → 30.9 MB total, i.e. flat |
| Linux container, remote lane | 938 batches, three binaries | 0.19–0.23 MB/batch |
| Linux container, local ONNX lane | 625 batches, cgroup quota applied | 0.143 MB/batch |
| Linux container, reporter's scale | 44,200 files / 5,525 batches | 0.205 MB/batch; settled at 2.4 GB |

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

- Every macOS number here is arm64. The Linux runs are linux/amd64 under
  emulation, except the local ONNX lane and the reporter-scale run, which are
  native linux/arm64 so that inference and parsing run at full speed. The
  reporter is on linux x64.
- All binaries are debug builds, on both platforms, so the comparison between
  them is consistent. The reporter runs release builds.
- This box had other compiles running throughout, so wall-clock timings are
  noisy. Batch-indexed slopes and allocator-counted bytes are not.
- On v0.56.0 the daemon's resident set dropped from 330 MB to 54 MB about a
  minute after its build finished, where v0.56.1, v0.56.2 and the head all held
  ~314 MB flat. No log line explains it and it appeared in one run. Resident set
  on macOS counts pages the allocator has freed but not returned, so the most
  likely explanation is the operating system reclaiming them under pressure from
  the other builds on this box. **This is recorded as an observation, not as a
  finding, and no claim is made that it is a product difference.**

## Workaround for shipped 0.56.1 and 0.56.2 users — REPORTED, NOT VERIFIED

**Everything in this section is the reporter's observation, reproduced here so
it is not lost. None of it has been tested by us.** We could not reproduce the
leak at all, so we are in no position to confirm that any configuration avoids
it. Treat these as the reporter's claims when relaying them, and say so.

What their matrix reports:

- **On 0.56.1 they report the local fastembed backend completing.** One full
  3h+ CPU build, stable in a 2.4–2.6 GB band. On that version they saw the leak
  only on remote backends. We have not run a local fastembed build of that
  length on any harness, so we cannot say whether it is genuinely leak-free or
  merely slow enough that the leak had not yet dominated.
- **On 0.56.2 they report no backend configuration that avoids it.** Runs A
  and B leaked on `openai_compatible` and run C leaked on local fastembed, on
  the same tree.

The remaining levers on 0.56.2 do not fix the leak, they only keep the daemon
under the watchdog by making the build smaller or by not running it. These
follow from how the build is shaped rather than from any measurement of theirs
or ours:

- Lower `semantic.max_files` below the corpus size, at the cost of coverage.
- Set `semantic_search: false`, which turns the feature off.

The reporter also notes that downgrading to 0.56.0 restores memory stability but
reintroduces the oversize-row build aborts that #318 fixed, so it is not a clean
escape for them.

## Where the next attempt should start

The exclusions now cover: the embed lane in isolation, the build loop, the
daemon's progress and status path, tool-call and reconfigure churn, both
backends, both operating systems, the cgroup quota path, the reporter's own file
and batch count, and four binaries spanning both reported arrival windows. None
of it grows.

What is still untested:

1. **Drive the daemon through the subc bridge rather than standalone NDJSON.**
   The reporter's daemon runs under the plugin, and the bridge transport is the
   one layer this harness does not exercise. `bridge.request_timeout_ms` and
   `hang_threshold` appear in their config and have no analogue here. The
   integration suite already stands a subc daemon up in-process
   (`crates/aft/tests/integration/subc_bridge_test.rs`), so this is reachable
   without the live fleet.
2. **Run against the reporter's actual corpus.** Scale has now been matched;
   content has not. Synthetic Rust files are uniform in a way real trees are
   not, and the cost-gate investigation established that language mix changes
   chunk count per file substantially.

A bisect over the 27-commit v0.56.0→v0.56.1 range is only worth running once
one of those reproduces. The parent's correction stands and was re-verified: no
commit in that range touches `crates/aft/src/semantic_index.rs` or
`crates/aft/src/synapse_embed.rs`, and `Cargo.lock` moves only the two workspace
version strings, so no dependency changed under the remote lane either.

## The measurement to ask the reporter for

Guessing at further environment differences is now the expensive path. One
cheap measurement on their side would split the remaining search in half, and
it is the thing to ask for before anyone writes more harness code.

**Ask them to run one leaking build with every other plane switched off.** In
their project config:

```jsonc
{
  "semantic_search": true,
  "search_index": false,
  "callgraph_store": false,
  "views": { "enabled": false }
}
```

and with no agent attached at all — configure the root, then leave it entirely
alone for the embed phase. No tool calls, no editing, no session activity.
Sample `/proc/<pid>/status` VmRSS every 30 s and keep the daemon log.

The answer discriminates cleanly:

- **If it still leaks**, the accumulator is in the semantic build path itself
  and this harness is missing something about their corpus or their backend's
  wire behaviour. The follow-up is then their corpus, or a capture of their
  backend's actual HTTP responses to replay.
- **If it does not leak**, the accumulator is in a plane that merely runs
  *alongside* the embed build — the trigram index, the callgraph store, Tier-2
  inspect, or the watcher over 44,200 files — and everything measured here has
  been looking in the wrong place. That is a different search entirely, and
  worth knowing before it costs another week.

Two smaller things worth collecting in the same run, both one-liners:

- `/proc/<pid>/smaps_rollup` at two points twenty minutes apart. It separates
  anonymous from file-backed memory and reports Pss, which says whether the
  growth is heap or mapped artifacts. VmRSS alone cannot tell those apart.
- The complete daemon log for that run, not an excerpt. The `index_event`
  stream would show whether more than one `build_started plane=semantic` is
  ever live at once, which is the one concurrency hypothesis this harness
  cannot rule out on a synthetic corpus, and which of the other planes are
  building while the embed phase runs.
